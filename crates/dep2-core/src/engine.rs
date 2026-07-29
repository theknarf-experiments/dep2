//! The Dep2 engine.
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
use parsing::decl::{is_null, DataType, NULL_SENTINEL};

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
    filters: &'a RelationFilters,
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
        // Push-down: a row matching none of the program's atom patterns for
        // this relation can never fire a rule — drop it before the interning
        // encode and the channel hop.
        if let Some(patterns) = self.filters.get(&*rel) {
            if !patterns.iter().any(|p| row_matches(p, row)) {
                PUSHDOWN_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            PUSHDOWN_KEPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let encoded: SmallVec<[i64; 8]> = row.iter().map(encode_value).collect();
        // A send error means the dataflow has shut down and dropped the receiver.
        let _ = self.tx.send((rel, encoded, diff));
    }
}

/// Push-down effectiveness counters (diagnostics; read via DEP2_DEBUG_STALL).
pub static PUSHDOWN_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PUSHDOWN_KEPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One column of a source-row filter pattern: a constant an EDB atom pins the
/// column to, or a wildcard.
#[derive(Debug, Clone, PartialEq)]
enum ColMatcher {
    Any,
    Int(i64),
    Str(Arc<str>),
}

/// Per-relation union of the program's constant atom patterns, for source-row
/// push-down filtering. A row that matches none of a relation's patterns can
/// never satisfy any body atom, so no rule can derive anything from it — it is
/// dropped before encoding/interning. Only relations with at least one atom
/// and no all-wildcard atom get an entry (an all-wildcard atom means every row
/// can match; relations no atom reads are handled by `set_wanted` upstream).
type RelationFilters = HashMap<String, Vec<Vec<ColMatcher>>>;

/// Extract [`RelationFilters`] from a loaded (constant-interned) program.
/// Positive AND negated atoms contribute patterns (a negated atom reads rows
/// too). String constants (interned by load) decode back to raw text so the
/// sink can match without interning; float-typed columns widen to `Any`
/// (bit-equality vs float-compare mismatch is not worth the rows).
fn source_filters(program: &Program) -> RelationFilters {
    let mut edb_types: HashMap<&str, Vec<DataType>> = HashMap::new();
    for decl in program.edbs() {
        edb_types.insert(
            decl.name(),
            decl.attributes().iter().map(|a| *a.data_type()).collect(),
        );
    }

    let mut per: HashMap<String, Option<Vec<Vec<ColMatcher>>>> = HashMap::new();
    for rule in program.rules() {
        for pred in rule.rhs() {
            let atom = match pred {
                parsing::rule::Predicate::AtomPredicate(a)
                | parsing::rule::Predicate::NegatedAtomPredicate(a) => a,
                parsing::rule::Predicate::ComparePredicate(_) => continue,
            };
            let Some(types) = edb_types.get(atom.name()) else {
                continue; // IDB or intermediate; sources never feed it
            };
            let mut pattern: Vec<ColMatcher> = Vec::with_capacity(atom.arity());
            let mut all_any = true;
            for (i, arg) in atom.arguments().iter().enumerate() {
                let m = match arg {
                    parsing::rule::AtomArg::Const(parsing::rule::Const::Integer(v)) => {
                        match types.get(i) {
                            Some(DataType::String) => reading::decode(*v)
                                .map(ColMatcher::Str)
                                .unwrap_or(ColMatcher::Any),
                            Some(DataType::Integer) => ColMatcher::Int(*v),
                            _ => ColMatcher::Any, // float or unknown: widen
                        }
                    }
                    // Text is interned to Integer at load; floats widen.
                    parsing::rule::AtomArg::Const(_) => ColMatcher::Any,
                    parsing::rule::AtomArg::Var(_) | parsing::rule::AtomArg::Placeholder => {
                        ColMatcher::Any
                    }
                };
                all_any &= matches!(m, ColMatcher::Any);
                pattern.push(m);
            }
            let entry = per
                .entry(atom.name().to_string())
                .or_insert_with(|| Some(Vec::new()));
            if all_any {
                *entry = None; // universal atom: never filter this relation
            } else if let Some(patterns) = entry {
                if !patterns.contains(&pattern) {
                    patterns.push(pattern);
                }
            }
        }
    }

    per.into_iter()
        .filter_map(|(name, patterns)| patterns.map(|p| (name, p)))
        .filter(|(_, p)| !p.is_empty())
        .collect()
}

/// Does a source row match one filter pattern? Arity mismatches pass (never
/// judge rows we do not understand); NULL never matches a constant, exactly
/// like the body-atom join it stands in for.
fn row_matches(pattern: &[ColMatcher], row: &[dep2_plugin::DataValue]) -> bool {
    if pattern.len() != row.len() {
        return true;
    }
    pattern.iter().zip(row).all(|(m, v)| match m {
        ColMatcher::Any => true,
        ColMatcher::Int(want) => matches!(v, dep2_plugin::DataValue::Integer(got) if got == want),
        ColMatcher::Str(want) => match v {
            dep2_plugin::DataValue::String(got) => got.as_str() == want.as_ref(),
            dep2_plugin::DataValue::Str(got) => got.as_ref() == want.as_ref(),
            _ => false,
        },
    })
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

/// Per-relation presentation shape from decl annotations:
/// (order_by as (column index, descending), row limit). Shapes only how the
/// query API serves rows — relations stay unordered sets.
pub type RelationShapes = HashMap<String, (Vec<(usize, bool)>, Option<usize>)>;

/// Rows of a relation that are actually PRESENT: the state map carries net
/// counts, and a row can sit at a negative count transiently while a batch is
/// still arriving (see the output callback in `engine.rs`). Only a positive
/// net count means the row is in the relation.
pub fn live_rows(
    rows: &HashMap<SmallVec<[i64; 8]>, isize>,
) -> impl Iterator<Item = &SmallVec<[i64; 8]>> {
    rows.iter()
        .filter(|(_, count)| **count > 0)
        .map(|(row, _)| row)
}

/// Decode one [`RelationState`] row (raw `i64`) to display strings using the
/// relation's column `types` (from [`RelationTypes`]). Columns beyond `types`
/// render as integers. The query API calls this lazily, only for served rows.
pub fn decode_state_row(row: &[i64], types: &[DataType]) -> Vec<String> {
    reading::decode_cells_i64(row, types)
}

/// Type-aware raw-row comparison for a decl's `order_by` spec: numbers sort
/// numerically, floats by value (NaN treated as equal), strings by decoded
/// text; NULLs sort last regardless of direction; ties fall back to raw-row
/// order so the result is deterministic.
pub fn ordered_cmp(
    a: &[i64],
    b: &[i64],
    order: &[(usize, bool)],
    types: &[DataType],
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for &(idx, desc) in order {
        let (x, y) = match (a.get(idx), b.get(idx)) {
            (Some(x), Some(y)) => (*x, *y),
            _ => continue,
        };
        let ord = match (is_null(x), is_null(y)) {
            (true, true) => Ordering::Equal,
            (true, false) => return Ordering::Greater, // NULLs last, always
            (false, true) => return Ordering::Less,
            (false, false) => {
                let base = match types.get(idx) {
                    Some(DataType::Float) => f64::from_bits(x as u64)
                        .partial_cmp(&f64::from_bits(y as u64))
                        .unwrap_or(Ordering::Equal),
                    Some(DataType::String) => match (reading::decode(x), reading::decode(y)) {
                        (Some(xs), Some(ys)) => xs.cmp(&ys),
                        _ => x.cmp(&y),
                    },
                    _ => x.cmp(&y),
                };
                if desc {
                    base.reverse()
                } else {
                    base
                }
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.cmp(b)
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
                    // Accumulate the FULL net count, negative values included.
                    // Differential dataflow may deliver a row's retraction
                    // before its matching addition (batches arrive per worker
                    // and per epoch, and a recursive aggregate re-derives a
                    // value it just withdrew). Dropping a `-1` for a row that
                    // is currently absent would let the later `+1` resurrect
                    // it as a phantom that never goes away — a stale label
                    // sitting alongside the real one, violating the relation's
                    // key. Only an exact zero means "gone".
                    match rel_map.entry(row) {
                        std::collections::hash_map::Entry::Occupied(mut e) => {
                            *e.get_mut() += diff;
                            if *e.get() == 0 {
                                e.remove();
                            }
                        }
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(diff);
                        }
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
    /// Per-relation presentation shape (order_by/limit decl annotations).
    relation_shapes: Arc<RelationShapes>,
    /// Loaded program files (display path, source), entry first.
    program_sources: Arc<Vec<(String, String)>>,
    /// Sidecar visualization spec (`<entry>.viz.json`), served verbatim at
    /// /spec. The engine treats it as opaque text for the UI.
    viz_spec: Arc<Option<String>>,
    /// Per-relation declared column NAMES (types live in relation_types).
    relation_columns: Arc<HashMap<String, Vec<String>>>,
    /// Source-row push-down filters (empty when publishing — published EDBs
    /// must stay complete for runtime queries).
    source_filters: Arc<RelationFilters>,
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
            relation_shapes: Arc::new(RelationShapes::new()),
            program_sources: Arc::new(Vec::new()),
            viz_spec: Arc::new(None),
            relation_columns: Arc::new(HashMap::new()),
            source_filters: Arc::new(RelationFilters::new()),
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

    /// Per-relation presentation shape (order_by/limit decl annotations),
    /// for the serving layer. Populated by [`Dep2::load_program`].
    pub fn relation_shapes(&self) -> Arc<RelationShapes> {
        Arc::clone(&self.relation_shapes)
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
        let (program, directives) =
            match syntax::parse_or_render_with_directives(name, dl_src, use_color()) {
                Ok(ok) => ok,
                Err(report) => {
                    eprintln!("{}", report);
                    return Err(format!("{} has errors (see report above)", name));
                }
            };
        self.apply_directives(&directives)?;
        self.program_sources = Arc::new(vec![(name.to_string(), dl_src.to_string())]);
        self.finish_load(program, dl_src.to_string())
    }

    /// Like [`Dep2::load_program_named`], loading from a file path so
    /// `.import "other.dl"` statements resolve (relative to the importing
    /// file). The staged program text and `/program` source stay the entry
    /// file's text; imported rules and declarations merge into the program.
    pub fn load_program_file(&mut self, path: &std::path::Path) -> Result<(), String> {
        let (program, sources) = match syntax::parse_file_with_sources(path, use_color()) {
            Ok(ok) => ok,
            Err(report) => {
                eprintln!("{}", report);
                return Err(format!("{} has errors (see report above)", path.display()));
            }
        };
        // Gathered across imports: a program that imports a file needing a
        // plugin or a source needs them too.
        let directives = syntax::parse_file_directives(path, use_color())?;
        self.apply_directives(&directives)?;
        let entry_src = sources.first().map(|(_, s)| s.clone()).unwrap_or_default();
        self.program_sources = Arc::new(sources);
        // Sidecar viz spec by convention: <entry>.viz.json next to the entry.
        let viz_path = path.with_extension("viz.json");
        self.viz_spec = Arc::new(std::fs::read_to_string(&viz_path).ok());
        self.finish_load(program, entry_src)
    }

    /// Check `.require`s and bind `.source`s.
    ///
    /// Anything bound on the command line WINS: `--source` overrides an inline
    /// source for the same relation, so a program that names a default input
    /// can still be pointed at something else without editing it. That is what
    /// keeps `git_stats.dl` runnable against any repository.
    fn apply_directives(&mut self, directives: &syntax::Directives) -> Result<(), String> {
        self.check_requires(&directives.requires)?;
        for spec in &directives.sources {
            let already_bound = self
                .bindings
                .iter()
                .any(|b| b.relation == spec.relation && b.provider == spec.provider);
            if already_bound {
                continue;
            }
            let config: HashMap<String, String> = spec.config.iter().cloned().collect();
            self.add_source(spec.relation.clone(), spec.provider.clone(), config);
        }
        Ok(())
    }

    /// Fail unless every `.require`d plugin is registered.
    ///
    /// Checked before anything is wired up, so a missing plugin is reported as
    /// a missing plugin. Without it the failure surfaces much later and much
    /// worse: binding a source to an absent provider panics with "no streaming
    /// provider registered for 'x'", which names no alternatives, suggests no
    /// fix, and looks like an engine fault rather than a build that left the
    /// plugin out.
    fn check_requires(&self, requires: &[String]) -> Result<(), String> {
        let available = self.loaded_plugins();
        let missing: Vec<&String> = requires
            .iter()
            .filter(|r| !available.iter().any(|a| a == *r))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let mut names: Vec<&str> = available.iter().map(String::as_str).collect();
        names.sort_unstable();
        Err(format!(
            "program requires plugin{} {}, which {} not registered.\n\
             available plugins: {}",
            if missing.len() == 1 { "" } else { "s" },
            missing
                .iter()
                .map(|m| format!("`{}`", m))
                .collect::<Vec<_>>()
                .join(", "),
            if missing.len() == 1 { "is" } else { "are" },
            if names.is_empty() {
                "(none)".to_string()
            } else {
                names.join(", ")
            }
        ))
    }

    /// Every loaded program file as (display path, source), entry first —
    /// imports in load order after it. One entry for text-loaded programs.
    pub fn program_sources(&self) -> Arc<Vec<(String, String)>> {
        Arc::clone(&self.program_sources)
    }

    /// The sidecar visualization spec (`<entry>.viz.json`), if one was found.
    pub fn viz_spec(&self) -> Arc<Option<String>> {
        Arc::clone(&self.viz_spec)
    }

    /// Declared column names per relation, for API metadata.
    pub fn relation_columns(&self) -> Arc<HashMap<String, Vec<String>>> {
        Arc::clone(&self.relation_columns)
    }

    /// Absolute scan roots of the bound sources (for editor links: relation
    /// file columns are paths relative to these).
    pub fn source_roots(&self) -> Vec<String> {
        self.bindings
            .iter()
            .filter_map(|b| b.config.get("root"))
            .filter_map(|r| {
                std::path::Path::new(r)
                    .canonicalize()
                    .ok()
                    .map(|p| p.display().to_string())
            })
            .collect()
    }

    fn finish_load(&mut self, mut program: Program, dl_src: String) -> Result<(), String> {
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
        let mut shapes = RelationShapes::new();
        let mut columns: HashMap<String, Vec<String>> = HashMap::new();
        for decl in program.edbs().iter().chain(program.idbs()) {
            columns.insert(
                decl.name().to_string(),
                decl.attributes()
                    .iter()
                    .map(|a| a.name().to_string())
                    .collect(),
            );
        }
        for decl in program.idbs() {
            types.insert(
                decl.name().to_string(),
                decl.attributes().iter().map(|a| *a.data_type()).collect(),
            );
            if !decl.order_by().is_empty() || decl.limit().is_some() {
                shapes.insert(
                    decl.name().to_string(),
                    (decl.order_by().to_vec(), decl.limit()),
                );
            }
        }
        self.relation_types = Arc::new(types);
        self.relation_shapes = Arc::new(shapes);
        self.relation_columns = Arc::new(columns);

        // Source-row push-down: rows matching none of the program's constant
        // atom patterns can never fire a rule, so the parse pool drops them
        // before encoding/interning. Disabled while publishing — a runtime
        // query over a published EDB must see the full relation.
        self.source_filters = Arc::new(
            if self.config.publish || std::env::var("DEP2_NO_PUSHDOWN").is_ok() {
                RelationFilters::new()
            } else {
                source_filters(&program)
            },
        );

        // Publishable relations for runtime queries: every EDB plus every
        // served IDB, with their column types. The base fat mode is what
        // queries must match (imported traces carry its row representation).
        // With publishing opted out there is no live-query surface at all —
        // and no per-relation arrangements maintained by the dataflow.
        if !self.config.publish {
            self.live = None;
            self.compiled = Some((program, dl_src));
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

        self.compiled = Some((program, dl_src));
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
        let source_filters = Arc::clone(&self.source_filters);
        for tid in 0..parse_threads {
            let entries = Arc::clone(&entries);
            let rel_names = Arc::clone(&rel_names);
            let tx = tx.clone();
            let shutdown = Arc::clone(&shutdown);
            let filters = Arc::clone(&source_filters);
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
                            filters: &filters,
                        };
                        src.ingest(unit, &mut sink);
                        seed_units += 1;
                    }
                    *units = Vec::new(); // free the seed list
                }
                if debug_stall {
                    eprintln!(
                        "[stall parse t{tid}] seed shard done: {seed_units} units in {:.1}s \
                         (pushdown kept={} dropped={})",
                        seed_started.elapsed().as_secs_f64(),
                        PUSHDOWN_KEPT.load(Ordering::Relaxed),
                        PUSHDOWN_DROPPED.load(Ordering::Relaxed),
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
                                    filters: &filters,
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
                    // Accumulate the FULL net count, negative values included.
                    // Differential dataflow may deliver a row's retraction
                    // before its matching addition (batches arrive per worker
                    // and per epoch, and a recursive aggregate re-derives a
                    // value it just withdrew). Dropping a `-1` for a row that
                    // is currently absent would let the later `+1` resurrect
                    // it as a phantom that never goes away — a stale label
                    // sitting alongside the real one, violating the relation's
                    // key. Only an exact zero means "gone".
                    match rel_map.entry(row) {
                        std::collections::hash_map::Entry::Occupied(mut e) => {
                            *e.get_mut() += diff;
                            if *e.get() == 0 {
                                e.remove();
                            }
                        }
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(diff);
                        }
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

#[cfg(test)]
mod filter_tests {
    use super::*;
    use dep2_plugin::DataValue;

    fn program(src: &str) -> Program {
        let mut program = syntax::parse(src).unwrap();
        program.map_constants(|c| match c {
            parsing::rule::Const::Text(quoted) => Some(parsing::rule::Const::Integer(
                reading::intern_literal(quoted),
            )),
            _ => None,
        });
        program
    }

    const P: &str = "\
.in
.decl e(x: number, y: number)
.decl s(k: string, v: number)
.decl u(a: number, b: number)
.decl unread(z: number)

.printsize
.decl a(x: number)
.decl b(x: number)
.decl c(x: number)

.rule
a(Y) :- e(1, Y).
b(Y) :- e(2, Y), !s(\"key\", Y).
c(Y) :- u(_, Y).
";

    #[test]
    fn patterns_union_constants_and_mark_universal() {
        let f = source_filters(&program(P));
        // e is read at x=1 and x=2: two patterns, wildcard second column.
        let e = f.get("e").expect("e must be filtered");
        assert_eq!(e.len(), 2);
        // s is read (negated) at k="key": string constants match decoded.
        let s = f.get("s").expect("s must be filtered");
        assert_eq!(s.len(), 1);
        assert!(matches!(&s[0][0], ColMatcher::Str(v) if v.as_ref() == "key"));
        assert!(matches!(s[0][1], ColMatcher::Any));
        // u has an all-wildcard atom: universal, never filtered.
        assert!(!f.contains_key("u"));
        // unread appears in no atom: left to set_wanted, not filtered here.
        assert!(!f.contains_key("unread"));
    }

    #[test]
    fn rows_match_against_the_pattern_union() {
        let f = source_filters(&program(P));
        let e = f.get("e").unwrap();
        let keep = |row: &[DataValue]| e.iter().any(|p| row_matches(p, row));
        assert!(keep(&[DataValue::Integer(1), DataValue::Integer(7)]));
        assert!(keep(&[DataValue::Integer(2), DataValue::Integer(9)]));
        assert!(!keep(&[DataValue::Integer(3), DataValue::Integer(7)]));

        let s = f.get("s").unwrap();
        let keep = |row: &[DataValue]| s.iter().any(|p| row_matches(p, row));
        assert!(keep(&[
            DataValue::String("key".to_string()),
            DataValue::Integer(1)
        ]));
        assert!(keep(&[DataValue::Str("key".into()), DataValue::Integer(2)]));
        assert!(!keep(&[
            DataValue::String("other".to_string()),
            DataValue::Integer(1)
        ]));
        // A row whose arity doesn't match the pattern is passed through, and
        // NULL never matches a constant.
        assert!(row_matches(
            &[ColMatcher::Int(1)],
            &[DataValue::Integer(9), DataValue::Integer(9)]
        ));
        assert!(!e
            .iter()
            .any(|p| row_matches(p, &[DataValue::Null, DataValue::Integer(7)])));
    }
}
