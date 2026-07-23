//! The HCL-free Dep2 engine.
//!
//! Register streaming plugins, bind each Datalog relation to a streaming data
//! source, load a native `.dl` program, then [`Dep2::run`] to stream updates
//! into FlowLog continuously until shutdown.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use parsing::parser::Program;
use smallvec::SmallVec;
use tracing::{info, warn};

use catalog::head::aggregation_catalog_from_program;
use executing::arg::Args as FlowlogArgs;
use executing::dataflow::{streaming_program_execution, StreamingConfig};
use executing::dataflow::{CommandLog, CompiledQuery, QueryCommand};
use planning::program::ProgramQueryPlan;
use reading::{KV_MAX, ROW_MAX};
use strata::stratification::Strata;

use dep2_plugin::{DataValue, Plugin, PluginContext, Source, StreamingDataSource, ValueSink};
use parsing::decl::{DataType, NULL_SENTINEL};

/// One pre-encoded input row pushed from the parse pool to the dataflow:
/// `(relation, encoded i64 row, diff)`. The relation is an `Arc<str>` so the hot
/// path clones a refcount instead of allocating a `String` per row, and the row is
/// a `SmallVec` sized to the engine's max non-fat arity (8) so every non-fat row
/// lives inline with no per-row heap allocation (fat rows still spill to the heap).
type EncodedRow = (Arc<str>, SmallVec<[i64; 8]>, isize);

/// Encode a streaming value into the `i64` the engine stores, using the engine's
/// (sharded, concurrent) global interner so ids agree with `.dl` literals, facts,
/// and output decoding.
fn encode_value(v: &DataValue) -> i64 {
    match v {
        DataValue::String(s) => reading::intern(s),
        DataValue::Str(s) => reading::intern(s),
        DataValue::Integer(i) => *i,
        DataValue::Float(f) => reading::float_to_i64(*f),
        DataValue::Bool(b) => i64::from(*b),
        DataValue::Null => NULL_SENTINEL,
    }
}

/// Stable (deterministic, seed-free) hash of a unit id — FNV-1a. Shards work units
/// across the parse-pool threads; the seed and the live-edit poll use the same
/// function so a unit always lands on the same parse thread (which holds its cache).
fn unit_shard(unit: &str, threads: usize) -> usize {
    if threads <= 1 {
        return 0;
    }
    let mut h: u64 = 0xcbf29ce484222325;
    for b in unit.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h % threads as u64) as usize
}

/// A bound source (cloneable config), the relation an unnamed (single-output) push
/// targets, and its enumerated work units.
struct SourceEntry {
    source: Box<dyn StreamingDataSource>,
    default_rel: Option<String>,
    units: Vec<String>,
}

/// A `ValueSink` that encodes each plugin row (`DataValue` -> `i64` via the global
/// interner) and pushes `(relation, row, diff)` onto the bounded queue the parse
/// pool shares with the dataflow worker(s). The send blocks when the queue is full,
/// which backpressures parsing while the dataflow catches up. An empty relation
/// resolves to the source's default output.
///
/// `rel_names` maps each known relation name to a shared `Arc<str>`, so the hot
/// path clones a refcount instead of allocating a `String` per row.
struct QueueSink<'a> {
    tx: &'a crossbeam_channel::Sender<EncodedRow>,
    rel_names: &'a HashMap<String, Arc<str>>,
    default_rel: Option<&'a Arc<str>>,
}

impl ValueSink for QueueSink<'_> {
    fn push(&mut self, relation: &str, row: &[DataValue], diff: isize) {
        let rel: Arc<str> = if relation.is_empty() {
            match self.default_rel {
                Some(r) => Arc::clone(r),
                None => return,
            }
        } else {
            match self.rel_names.get(relation) {
                Some(r) => Arc::clone(r),
                // Unknown relation (not in any source's outputs) — fall back to a
                // fresh allocation; should not happen for well-behaved plugins.
                None => Arc::from(relation),
            }
        };
        let encoded: SmallVec<[i64; 8]> = row.iter().map(encode_value).collect();
        // A send error means the dataflow has shut down and dropped the receiver.
        let _ = self.tx.send((rel, encoded, diff));
    }
}

/// Engine configuration.
pub struct Dep2Config {
    /// Number of FlowLog worker threads.
    pub workers: usize,
    /// Print each `+`/`-` output update to stdout. Disable when serving the
    /// query API so a long-running process stays quiet.
    pub print_updates: bool,
    /// Publish relations for runtime queries (default). Every EDB and served
    /// IDB then maintains a whole-row arrangement per worker so queries can be
    /// added while the engine runs — memory proportional to the published
    /// relations' sizes, paid even if no query is ever added. Opt out to skip
    /// the arrangements entirely; [`Dep2::live_queries`] then returns `None`
    /// and the HTTP query routes report the feature unavailable.
    pub publish: bool,
}

impl Default for Dep2Config {
    fn default() -> Self {
        Self {
            workers: 4,
            print_updates: true,
            publish: true,
        }
    }
}

/// Materialized current state of the output relations: relation name -> (row of
/// *raw encoded `i64`* values -> net multiplicity). A row is present iff its count
/// is > 0. Rows are stored encoded (interned-string ids / float bits / integers)
/// and decoded to display text only when queried — so a row inserted and retracted
/// during a seed is never decoded. Use [`RelationTypes`] (via [`Dep2::relation_types`])
/// to decode. Shared with the query API while the engine runs.
pub type RelationState = HashMap<String, HashMap<SmallVec<[i64; 8]>, isize>>;

/// Per-relation column types, used to decode a [`RelationState`] row's raw `i64`
/// values back to display strings at query time.
pub type RelationTypes = HashMap<String, Vec<DataType>>;

/// Decode one [`RelationState`] row (raw `i64`) to display strings using the
/// relation's column `types` (from [`RelationTypes`]). Columns beyond `types`
/// render as integers. The query API calls this lazily, only for served rows.
pub fn decode_state_row(row: &[i64], types: &[DataType]) -> Vec<String> {
    reading::decode_cells_i64(row, types)
}

/// Color error reports only when stderr is a terminal.
fn use_color() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

/// Verify a source output's schema against the `.decl` it is wired to (the
/// wiring is by name only). Arity or column-type disagreement would feed
/// silent garbage — a float pushed into a `number` column arrives as its bit
/// pattern — so it is an error, not a warning.
fn check_source_schema(
    provider: &str,
    relation: &str,
    schema: &dep2_plugin::DataSchema,
    decl: &parsing::decl::RelDecl,
) -> Result<(), String> {
    if schema.columns.len() != decl.arity() {
        return Err(format!(
            "source '{}' feeds relation {} with {} columns, but it is declared with {} \
             — fix the .decl or the source config",
            provider,
            relation,
            schema.columns.len(),
            decl.arity()
        ));
    }
    for (col, attr) in schema.columns.iter().zip(decl.attributes()) {
        let source_type = match col.data_type {
            dep2_plugin::DataType::String => DataType::String,
            dep2_plugin::DataType::Integer => DataType::Integer,
            dep2_plugin::DataType::Float => DataType::Float,
        };
        if source_type != *attr.data_type() {
            return Err(format!(
                "source '{}' feeds {}.{} as {}, but it is declared {} — fix the .decl \
                 or the source config (e.g. csv `types=`)",
                provider,
                relation,
                attr.name(),
                source_type,
                attr.data_type()
            ));
        }
    }
    Ok(())
}

/// Classify declared IDB relations into served and unserved.
///
/// A relation is served (returned `true` set) when it is *terminal* — not used in
/// any other rule's body (self-recursion doesn't count) — or declared `.out`
/// (force-serve). The second map holds each unserved relation -> the sorted rule
/// heads that consume it, so the query API can explain the omission.
fn classify_relations(
    program: &Program,
) -> (
    std::collections::HashSet<String>,
    HashMap<String, Vec<String>>,
) {
    use std::collections::{BTreeSet, HashSet};

    let mut consumers: HashMap<String, BTreeSet<String>> = HashMap::new();
    for rule in program.rules() {
        let head = rule.head().name().as_str();
        for pred in rule.rhs() {
            let name = match pred {
                parsing::rule::Predicate::AtomPredicate(a) => Some(a.name()),
                parsing::rule::Predicate::NegatedAtomPredicate(a) => Some(a.name()),
                parsing::rule::Predicate::ComparePredicate(_) => None,
            };
            if let Some(n) = name {
                if n != head {
                    consumers
                        .entry(n.to_string())
                        .or_default()
                        .insert(head.to_string());
                }
            }
        }
    }

    let mut served: HashSet<String> = HashSet::new();
    let mut unserved: HashMap<String, Vec<String>> = HashMap::new();
    for decl in program.idbs() {
        let name = decl.name().to_string();
        let consumed = consumers.contains_key(&name);
        if !consumed || decl.force_serve() {
            served.insert(name);
        } else {
            let by = consumers
                .get(&name)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            unserved.insert(name, by);
        }
    }
    (served, unserved)
}

/// What live queries are validated against: the running program's published
/// relations (name -> column types) and its row representation.
struct QueryBase {
    published: HashMap<String, Vec<DataType>>,
    fat_mode: bool,
}

/// Control handle for adding and removing queries on a RUNNING engine.
///
/// Obtained from [`Dep2::live_queries`] once a program is loaded; cloneable
/// and thread-safe, so e.g. an HTTP layer can hold one while the engine runs.
/// A query is a full `.dl` program whose `.in` decls name relations the base
/// program publishes (its EDBs and served IDBs); it is parsed, typed, and
/// validated here — control-side — then instantiated by every worker as its
/// own dataflow over imported traces: it sees the full history of the base
/// relations, then follows live updates, exactly as if its rules had been
/// part of the program from the start.
#[derive(Clone)]
pub struct LiveQueries {
    commands: CommandLog,
    base: Arc<QueryBase>,
    /// Per-query materialized output, column types, and the original `.dl`
    /// source (for introspection), keyed by query id.
    #[allow(clippy::type_complexity)]
    states: Arc<Mutex<HashMap<String, (Arc<Mutex<RelationState>>, Arc<RelationTypes>, Arc<str>)>>>,
}

impl LiveQueries {
    /// Compile, validate, and add a query. Errors (with the front-end's
    /// rendered report for parse/typing problems) come back synchronously;
    /// workers only ever see queries that compiled.
    pub fn add(&self, id: &str, dl_src: &str) -> Result<(), String> {
        if self.states.lock().unwrap().contains_key(id) {
            return Err(format!("query '{}' already exists", id));
        }

        let name = format!("query '{}'", id);
        let mut program = syntax::parse_or_render(&name, dl_src, false)
            .map_err(|report| format!("{}", report))?;
        program.map_constants(|c| match c {
            parsing::rule::Const::Text(quoted) => Some(parsing::rule::Const::Integer(
                reading::intern_literal(quoted),
            )),
            _ => None,
        });

        // Every query input must be published by the base, with the same schema.
        for edb in program.edbs() {
            let cols = self.base.published.get(edb.name()).ok_or_else(|| {
                let mut names: Vec<&str> = self.base.published.keys().map(|s| s.as_str()).collect();
                names.sort();
                format!(
                    "query base relation '{}' is not published by the running program \
                     (published: {})",
                    edb.name(),
                    names.join(", ")
                )
            })?;
            let declared: Vec<DataType> = edb.attributes().iter().map(|a| *a.data_type()).collect();
            if declared != *cols {
                return Err(format!(
                    "query decl '{}' does not match the published schema \
                     (declared {:?}, published {:?})",
                    edb.name(),
                    declared,
                    cols
                ));
            }
        }
        if program.idbs().is_empty() {
            return Err(format!("query '{}' derives nothing (no rules)", id));
        }

        let strata = Strata::from_parser(program.clone());
        let plan = ProgramQueryPlan::from_strata(&strata, false, None);
        let fat = plan.should_use_fat_mode(false, KV_MAX, ROW_MAX);
        if fat != self.base.fat_mode {
            return Err(
                "query would run in a different row mode (fat) than the running program; \
                 reduce the query's rule width"
                    .to_string(),
            );
        }
        let idb_map = aggregation_catalog_from_program(&program);

        // Per-query materialized state, updated by the query's own callback
        // (same accumulate-net-counts logic as the engine's main output path).
        let mut types = RelationTypes::new();
        let mut initial = RelationState::new();
        for decl in program.idbs() {
            types.insert(
                decl.name().to_string(),
                decl.attributes().iter().map(|a| *a.data_type()).collect(),
            );
            initial.entry(decl.name().to_string()).or_default();
        }
        let state = Arc::new(Mutex::new(initial));
        let state_cb = Arc::clone(&state);
        let output_callback: Arc<dyn Fn(&str, SmallVec<[i64; 8]>, isize) + Send + Sync> = Arc::new(
            move |rel_name: &str, row: SmallVec<[i64; 8]>, diff: isize| {
                if diff == 0 {
                    return;
                }
                let mut st = state_cb.lock().unwrap();
                if let Some(rel_map) = st.get_mut(rel_name) {
                    if let Some(count) = rel_map.get_mut(&row) {
                        *count += diff;
                        if *count <= 0 {
                            rel_map.remove(&row);
                        }
                    } else if diff > 0 {
                        rel_map.insert(row, diff);
                    }
                }
            },
        );

        self.states.lock().unwrap().insert(
            id.to_string(),
            (Arc::clone(&state), Arc::new(types), Arc::from(dl_src)),
        );
        self.commands
            .push(QueryCommand::Add(Arc::new(CompiledQuery {
                id: id.to_string(),
                strata,
                plans: plan.program_plan().to_owned(),
                idb_map,
                fat_mode: fat,
                output_callback,
            })));
        Ok(())
    }

    /// Drop a live query: its dataflow retires and its state is discarded.
    /// Returns false when no such query exists.
    pub fn remove(&self, id: &str) -> bool {
        if self.states.lock().unwrap().remove(id).is_none() {
            return false;
        }
        self.commands
            .push(QueryCommand::Drop { id: id.to_string() });
        true
    }

    /// Ids of the live queries, sorted.
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.states.lock().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// A query's materialized state and column types, for serving.
    #[allow(clippy::type_complexity)]
    pub fn state(&self, id: &str) -> Option<(Arc<Mutex<RelationState>>, Arc<RelationTypes>)> {
        self.states
            .lock()
            .unwrap()
            .get(id)
            .map(|(state, types, _)| (Arc::clone(state), Arc::clone(types)))
    }

    /// The `.dl` source a query was added with, for introspection.
    pub fn source(&self, id: &str) -> Option<Arc<str>> {
        self.states
            .lock()
            .unwrap()
            .get(id)
            .map(|(_, _, src)| Arc::clone(src))
    }
}

/// Binds a streaming data source provided by a plugin to Datalog relation(s).
pub struct SourceBinding {
    /// The EDB relation name for a single-output source (e.g. csv, fs). `None`
    /// for multi-output sources (e.g. treesitter), which name their own outputs.
    pub relation: Option<String>,
    /// The streaming provider type (must be registered by a plugin).
    pub provider: String,
    /// Provider-specific configuration (e.g. `root`, `path`, ...).
    pub config: HashMap<String, String>,
}

/// The Dep2 engine.
pub struct Dep2 {
    plugins: Vec<Box<dyn Plugin>>,
    plugin_ctx: PluginContext,
    config: Dep2Config,
    bindings: Vec<SourceBinding>,
    /// Parsed program plus the integer-rewritten `.dl` text.
    compiled: Option<(Program, String)>,
    /// Live materialized state of the output relations, updated as the engine runs.
    state: Arc<Mutex<RelationState>>,
    /// Per-relation column types, for decoding `state` rows at query time.
    relation_types: Arc<RelationTypes>,
    /// Per-engine temp dir for the staged program/facts, unique within the
    /// process so multiple engines (e.g. in tests) don't clobber each other.
    work_dir: PathBuf,
    /// Runtime-query control handle, created when a program loads.
    live: Option<LiveQueries>,
}

impl Dep2 {
    pub fn new() -> Self {
        Self::with_config(Dep2Config::default())
    }

    pub fn with_config(config: Dep2Config) -> Self {
        static ENGINE_SEQ: AtomicU64 = AtomicU64::new(0);
        let id = ENGINE_SEQ.fetch_add(1, Ordering::Relaxed);
        let work_dir = std::env::temp_dir().join(format!("dep2-{}-{}", std::process::id(), id));
        Self {
            plugins: Vec::new(),
            plugin_ctx: PluginContext::new(),
            config,
            bindings: Vec::new(),
            compiled: None,
            state: Arc::new(Mutex::new(RelationState::new())),
            relation_types: Arc::new(RelationTypes::new()),
            work_dir,
            live: None,
        }
    }

    /// A handle to the live materialized state of the output relations. The query
    /// API reads this while [`Dep2::run`] keeps it up to date.
    pub fn state(&self) -> Arc<Mutex<RelationState>> {
        Arc::clone(&self.state)
    }

    /// Per-relation column types, for decoding [`Dep2::state`] rows (raw `i64`)
    /// back to display strings. Populated by [`Dep2::load_program`]; empty before.
    pub fn relation_types(&self) -> Arc<RelationTypes> {
        Arc::clone(&self.relation_types)
    }

    /// The runtime-query control handle (add/remove queries on the running
    /// engine). Available once a program is loaded; clone it before `run`.
    pub fn live_queries(&self) -> Option<LiveQueries> {
        self.live.clone()
    }

    /// Declared relations that are computed but *not* served over the query API
    /// (consumed by another rule and not declared `.out`), each mapped to the
    /// rule heads that consume it. Lets the server explain why a query returns
    /// nothing instead of a bare "unknown relation". Empty before a program loads.
    pub fn unserved_relations(&self) -> HashMap<String, Vec<String>> {
        match &self.compiled {
            Some((program, _)) => classify_relations(program).1,
            None => HashMap::new(),
        }
    }

    /// Register a plugin and run its setup (provider registration).
    pub fn add_plugin(&mut self, plugin: Box<dyn Plugin>) {
        plugin.setup(&mut self.plugin_ctx);
        self.plugins.push(plugin);
    }

    /// Names of registered plugins.
    pub fn loaded_plugins(&self) -> &[String] {
        self.plugin_ctx.registered_plugins()
    }

    /// Bind a streaming source from a registered provider. `relation` names the
    /// target EDB for single-output sources; pass `None` for multi-output sources
    /// (which declare their own relation names).
    pub fn add_source(
        &mut self,
        relation: Option<String>,
        provider: impl Into<String>,
        config: HashMap<String, String>,
    ) {
        self.bindings.push(SourceBinding {
            relation,
            provider: provider.into(),
            config,
        });
    }

    /// Load a native FlowLog `.dl` program. String literals are interned into
    /// the engine's global table and replaced with integer ids before FlowLog
    /// parses them.
    pub fn load_program(&mut self, dl_src: &str) -> Result<(), String> {
        self.load_program_named(dl_src, "program.dl")
    }

    /// Like [`Dep2::load_program`], with the program's file name for error
    /// reports. Parse/typing/validation errors are rendered as labelled source
    /// snippets (via the `syntax` front-end) on stderr.
    pub fn load_program_named(&mut self, dl_src: &str, name: &str) -> Result<(), String> {
        // Parse and validate the ORIGINAL source (spans in error reports point
        // at what the user wrote).
        let mut program = match syntax::parse_or_render(name, dl_src, use_color()) {
            Ok(program) => program,
            Err(report) => {
                eprintln!("{}", report);
                return Err(format!("{} has errors (see report above)", name));
            }
        };

        // Intern string literals into ids at the AST level (the engine works
        // on i64 columns; string constants become their interned ids, exactly
        // as `reading::encode_literals` used to do by rewriting the source
        // before a second parse).
        program.map_constants(|c| match c {
            parsing::rule::Const::Text(quoted) => Some(parsing::rule::Const::Integer(
                reading::intern_literal(quoted),
            )),
            _ => None,
        });

        // Record each IDB's column types so the query API can decode the raw `i64`
        // rows stored in `state` back to display text on demand.
        let mut types = RelationTypes::new();
        for decl in program.idbs() {
            types.insert(
                decl.name().to_string(),
                decl.attributes().iter().map(|a| *a.data_type()).collect(),
            );
        }
        self.relation_types = Arc::new(types);

        // Publishable relations for runtime queries: every EDB plus every
        // served IDB, with their column types. The base fat mode is what
        // queries must match (imported traces carry its row representation).
        // With publishing opted out there is no live-query surface at all —
        // and no per-relation arrangements maintained by the dataflow.
        if !self.config.publish {
            self.live = None;
            self.compiled = Some((program, dl_src.to_string()));
            return Ok(());
        }
        let (served, _) = classify_relations(&program);
        let mut published: HashMap<String, Vec<DataType>> = HashMap::new();
        for decl in program.edbs() {
            published.insert(
                decl.name().to_string(),
                decl.attributes().iter().map(|a| *a.data_type()).collect(),
            );
        }
        for decl in program.idbs() {
            if served.contains(decl.name()) {
                published.insert(
                    decl.name().to_string(),
                    decl.attributes().iter().map(|a| *a.data_type()).collect(),
                );
            }
        }
        let base_fat = {
            let strata = Strata::from_parser(program.clone());
            let plan = ProgramQueryPlan::from_strata(&strata, false, None);
            plan.should_use_fat_mode(false, KV_MAX, ROW_MAX)
        };
        self.live = Some(LiveQueries {
            commands: CommandLog::default(),
            base: Arc::new(QueryBase {
                published,
                fat_mode: base_fat,
            }),
            states: Arc::new(Mutex::new(HashMap::new())),
        });

        self.compiled = Some((program, dl_src.to_string()));
        Ok(())
    }

    /// Run the program in streaming mode, blocking until `shutdown` is set.
    pub fn run(&mut self, shutdown: Arc<AtomicBool>) -> Result<(), String> {
        // `dl_text` is the ORIGINAL program source: the staged program.dl file
        // is informational (executing uses the path only for display names; the
        // AST travels via `Strata::from_parser`).
        let (program, dl_text) = self.compiled.as_ref().ok_or("no program loaded")?;

        // Stage the program file and an empty facts dir. Every EDB gets an empty
        // `.facts` file so FlowLog's batch load (epoch 0) finds something; the
        // bound relations are then fed live via streaming channels.
        let facts_dir = self.work_dir.join("facts");
        std::fs::create_dir_all(&facts_dir)
            .map_err(|e| format!("failed to create facts dir: {}", e))?;
        for decl in program.edbs() {
            let path = facts_dir.join(format!("{}.facts", decl.name()));
            std::fs::write(&path, "").map_err(|e| format!("failed to write facts: {}", e))?;
        }

        let dl_path = self.work_dir.join("program.dl");
        std::fs::write(&dl_path, dl_text).map_err(|e| format!("failed to write program: {}", e))?;

        // Open each streaming source. Sources run on a dedicated parse pool (see
        // below) and push pre-encoded rows onto a bounded queue that the dataflow
        // worker(s) drain — no route thread, no MPMC fan-out.
        let edb_names: HashSet<&str> = program.edbs().iter().map(|d| d.name()).collect();
        let edb_decls: HashMap<&str, &parsing::decl::RelDecl> =
            program.edbs().iter().map(|d| (d.name(), d)).collect();
        let mut entries: Vec<SourceEntry> = Vec::new();

        for binding in &self.bindings {
            let provider = self
                .plugin_ctx
                .get_streaming_data_provider(&binding.provider)
                .ok_or_else(|| {
                    format!(
                        "no streaming provider registered for '{}'",
                        binding.provider
                    )
                })?;
            let mut source = provider
                .open_stream(&binding.config)
                .map_err(|e| format!("failed to open '{}': {}", binding.provider, e))?;

            // Resolve each declared output to a concrete relation. A single-output
            // source with an empty relation name takes the binding's relation
            // (recorded as `default_rel`); multi-output sources name their own.
            let outputs = source.outputs();
            if outputs.is_empty() {
                return Err(format!(
                    "provider '{}' declared no outputs",
                    binding.provider
                ));
            }
            let mut wired: Vec<String> = Vec::new();
            let mut default_rel: Option<String> = None;
            for out in &outputs {
                let (rel, is_default) = if !out.relation.is_empty() {
                    (out.relation.clone(), false)
                } else {
                    let r = binding.relation.clone().ok_or_else(|| {
                        format!(
                            "provider '{}' needs a relation name (use 'RELATION={}:...')",
                            binding.provider, binding.provider
                        )
                    })?;
                    (r, true)
                };
                // Outputs the program doesn't declare (e.g. ast_span when a rules
                // file only needs ast_node) are dropped — never fed.
                if !edb_names.contains(rel.as_str()) {
                    warn!(
                        "source output relation '{}' not declared in program; ignoring",
                        rel
                    );
                    continue;
                }
                // The wiring is by name only, so verify the source's schema
                // actually matches the declaration — otherwise a wrong arity or
                // column type feeds silent garbage (a float pushed into a
                // `number` column is interpreted as its bit pattern).
                check_source_schema(
                    &binding.provider,
                    &rel,
                    &out.schema,
                    edb_decls[rel.as_str()],
                )?;
                if is_default {
                    default_rel = Some(rel.clone());
                }
                wired.push(rel);
            }
            if wired.is_empty() {
                warn!(
                    "provider '{}' feeds no relations used by the program; skipping",
                    binding.provider
                );
                continue;
            }
            // Let the source skip building outputs nothing consumes.
            let wired_set: HashSet<String> = wired.iter().cloned().collect();
            source.set_wanted(&wired_set);

            // Enumerate the work units once (the engine shards them per worker).
            let units = source.seed_units();

            entries.push(SourceEntry {
                source,
                default_rel,
                units,
            });
        }

        // Parse pool: parsing (the CPU-heavy part) runs on a dedicated pool of
        // threads, NOT on the dataflow workers, so it parallelizes independently of
        // the Datalog worker count. Each thread opens its own per-source `Source`
        // (so it may hold non-Send state like a wasm parser), takes its shard of the
        // units (a stable hash assigns each unit to one thread, consistently for the
        // seed and for live edits, so a unit's cache stays on one thread), parses
        // them, and pushes pre-encoded rows onto a bounded queue. The dataflow
        // worker(s) drain that queue; a full queue backpressures the parsers.
        let entries = Arc::new(entries);

        // Intern every known relation name (each source's outputs plus its default)
        // to a shared `Arc<str>` once, so the per-row hot path clones a refcount
        // instead of allocating a `String`.
        let mut rel_names: HashMap<String, Arc<str>> = HashMap::new();
        for e in entries.iter() {
            for out in e.source.outputs() {
                rel_names
                    .entry(out.relation.clone())
                    .or_insert_with(|| Arc::from(out.relation.as_str()));
            }
            if let Some(dr) = &e.default_rel {
                rel_names
                    .entry(dr.clone())
                    .or_insert_with(|| Arc::from(dr.as_str()));
            }
        }
        let rel_names = Arc::new(rel_names);

        let parse_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .max(1);
        let (tx, rx) = crossbeam_channel::bounded::<EncodedRow>(100_000);
        let mut parse_handles = Vec::new();
        for tid in 0..parse_threads {
            let entries = Arc::clone(&entries);
            let rel_names = Arc::clone(&rel_names);
            let tx = tx.clone();
            let shutdown = Arc::clone(&shutdown);
            parse_handles.push(std::thread::spawn(move || {
                // Open a per-source runner on THIS thread (non-Send state lives here)
                // and compute this thread's shard of the seed units.
                let mut opened: Vec<(Box<dyn Source>, Option<Arc<str>>, Vec<String>)> = entries
                    .iter()
                    .map(|e| {
                        let shard = e
                            .units
                            .iter()
                            .filter(|u| unit_shard(u, parse_threads) == tid)
                            .cloned()
                            .collect();
                        let default_rel = e.default_rel.as_ref().map(|d| {
                            rel_names
                                .get(d)
                                .cloned()
                                .unwrap_or_else(|| Arc::from(d.as_str()))
                        });
                        (e.source.open(), default_rel, shard)
                    })
                    .collect();

                // Seed: parse this thread's shard, pushing rows onto the queue.
                let debug_stall = std::env::var("DEP2_DEBUG_STALL").is_ok();
                let seed_started = std::time::Instant::now();
                let mut seed_units = 0usize;
                for (src, default_rel, units) in &mut opened {
                    for unit in units.iter() {
                        if shutdown.load(Ordering::Relaxed) {
                            return;
                        }
                        let mut sink = QueueSink {
                            tx: &tx,
                            rel_names: &rel_names,
                            default_rel: default_rel.as_ref(),
                        };
                        src.ingest(unit, &mut sink);
                        seed_units += 1;
                    }
                    *units = Vec::new(); // free the seed list
                }
                if debug_stall {
                    eprintln!(
                        "[stall parse t{tid}] seed shard done: {seed_units} units in {:.1}s",
                        seed_started.elapsed().as_secs_f64()
                    );
                }

                // Watch: poll each source for changed units; reconcile the ones in
                // this thread's shard (same hash, so its cache is here).
                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    let mut any = false;
                    let mut reingested = 0usize;
                    for (src, default_rel, _) in &mut opened {
                        for unit in src.poll_changes() {
                            if unit_shard(&unit, parse_threads) == tid {
                                reingested += 1;
                                let mut sink = QueueSink {
                                    tx: &tx,
                                    rel_names: &rel_names,
                                    default_rel: default_rel.as_ref(),
                                };
                                src.ingest(&unit, &mut sink);
                                any = true;
                            }
                        }
                    }
                    if debug_stall && reingested > 0 {
                        eprintln!("[stall parse t{tid}] reingested {reingested} changed units");
                    }
                    if !any {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            }));
        }
        // Only the parse threads hold senders now; when they exit (shutdown), the
        // receiver disconnects.
        drop(tx);

        // Serve *terminal* IDBs by default; `.out` relations force-serve even when
        // consumed (see `classify_relations`). The dataflow decodes columns itself,
        // so the engine only needs the served-relation set here.
        let (printable, _) = classify_relations(program);

        // Pre-register output relations so they appear (possibly empty) in the
        // query API even before any rows are derived.
        {
            let mut st = self.state.lock().unwrap();
            st.clear();
            for name in &printable {
                st.entry(name.clone()).or_default();
            }
        }

        let state_cb = Arc::clone(&self.state);
        let print = self.config.print_updates;
        let types_cb = Arc::clone(&self.relation_types);
        let output_seq = Arc::new(AtomicU64::new(0));
        let output_seq_cb = Arc::clone(&output_seq);
        // Rows arrive as the engine's raw encoded `i64` and are stored as-is; they
        // are decoded to text only when the query API serves them (or here for the
        // optional `--print` debug stream). This keeps the output hot path free of
        // per-tuple string allocation/decoding.
        let output_callback: Arc<dyn Fn(&str, SmallVec<[i64; 8]>, isize) + Send + Sync> = Arc::new(
            move |rel_name: &str, row: SmallVec<[i64; 8]>, diff: isize| {
                if !printable.contains(rel_name) || diff == 0 {
                    return;
                }
                output_seq_cb.fetch_add(1, Ordering::Relaxed);

                // Decode for the optional debug print BEFORE moving `row` into the
                // map. Only runs under `--print`; serving leaves rows encoded.
                if print {
                    let decoded = match types_cb.get(rel_name) {
                        Some(t) => reading::decode_cells_i64(&row, t).join(", "),
                        None => row
                            .iter()
                            .map(|v| v.to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                    };
                    let kind = if diff > 0 { "+" } else { "-" };
                    println!("{} {}({})", kind, rel_name, decoded);
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }

                // Update the materialized state: a row is present iff net count > 0.
                // Relations are pre-registered above, so `get_mut` finds the map.
                // The hot path adds no allocation: update in place, or move `row`
                // in on first insert — no clone of the row.
                let mut st = state_cb.lock().unwrap();
                if let Some(rel_map) = st.get_mut(rel_name) {
                    if let Some(count) = rel_map.get_mut(&row) {
                        *count += diff;
                        if *count <= 0 {
                            rel_map.remove(&row);
                        }
                    } else if diff > 0 {
                        rel_map.insert(row, diff);
                    }
                }
            },
        );

        // With publishing opted out, hand the dataflow an empty publish set (no
        // arrangements built) and a command log nobody holds a handle to.
        let (publish, commands) = match self.live.as_ref() {
            Some(live) => (
                live.base.published.keys().cloned().collect(),
                live.commands.clone(),
            ),
            None => (Default::default(), CommandLog::default()),
        };
        let streaming_config = StreamingConfig {
            input: rx,
            output_callback,
            shutdown: Arc::clone(&shutdown),
            output_seq,
            publish,
            commands,
        };

        // Build the FlowLog execution plan and run.
        let strata = Strata::from_parser(program.clone());
        let plan = ProgramQueryPlan::from_strata(&strata, false, None);
        let fat_mode = plan.should_use_fat_mode(false, KV_MAX, ROW_MAX);
        let idb_map = aggregation_catalog_from_program(program);

        let flowlog_args = FlowlogArgs::new(
            dl_path.to_string_lossy().into_owned(),
            facts_dir.to_string_lossy().into_owned(),
            None,
            "\t".to_string(),
            self.config.workers,
        );

        info!("dep2 streaming execution starting");
        streaming_program_execution(
            flowlog_args,
            strata,
            plan.program_plan().to_owned(),
            fat_mode,
            idb_map,
            streaming_config,
        );
        info!("dep2 streaming execution complete");

        // The dataflow returned (shutdown), dropping the queue receiver, so the
        // parse threads' sends now fail and they observe `shutdown`; join them.
        for h in parse_handles {
            let _ = h.join();
        }

        Ok(())
    }
}

impl Default for Dep2 {
    fn default() -> Self {
        Self::new()
    }
}
