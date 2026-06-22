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
use tracing::{info, warn};

use catalog::head::aggregation_catalog_from_program;
use executing::arg::Args as FlowlogArgs;
use executing::dataflow::{streaming_program_execution, StreamingConfig};
use planning::program::ProgramQueryPlan;
use reading::{KV_MAX, ROW_MAX};
use strata::stratification::Strata;

use dep2_plugin::{DataValue, Plugin, PluginContext, StreamingUpdate};
use parsing::decl::NULL_SENTINEL;

/// Encode a streaming plugin value into the `i64` the engine stores, using the
/// engine's global interner so ids agree with `.dl` literals, facts, and output
/// decoding.
fn encode_value(v: &DataValue) -> i64 {
    match v {
        DataValue::String(s) => reading::intern(s),
        DataValue::Integer(i) => *i,
        DataValue::Float(f) => reading::float_to_i64(*f),
        DataValue::Bool(b) => i64::from(*b),
        DataValue::Null => NULL_SENTINEL,
    }
}

/// Encode a value using an already-held interner lock (for batch encoding, so the
/// lock is taken once per batch rather than once per value).
fn encode_value_locked(ig: &mut reading::InternLock, v: &DataValue) -> i64 {
    match v {
        DataValue::String(s) => ig.intern(s),
        DataValue::Integer(i) => *i,
        DataValue::Float(f) => reading::float_to_i64(*f),
        DataValue::Bool(b) => i64::from(*b),
        DataValue::Null => NULL_SENTINEL,
    }
}

/// Engine configuration.
pub struct Dep2Config {
    /// Number of FlowLog worker threads.
    pub workers: usize,
    /// Print each `+`/`-` output update to stdout. Disable when serving the
    /// query API so a long-running process stays quiet.
    pub print_updates: bool,
}

impl Default for Dep2Config {
    fn default() -> Self {
        Self {
            workers: 1,
            print_updates: true,
        }
    }
}

/// Materialized current state of the output relations: relation name -> (row of
/// decoded string values -> net multiplicity). A row is present iff its count is
/// > 0. Shared with the query API while the engine runs.
pub type RelationState = HashMap<String, HashMap<Vec<String>, isize>>;

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
    /// Per-engine temp dir for the staged program/facts, unique within the
    /// process so multiple engines (e.g. in tests) don't clobber each other.
    work_dir: PathBuf,
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
            work_dir,
        }
    }

    /// A handle to the live materialized state of the output relations. The query
    /// API reads this while [`Dep2::run`] keeps it up to date.
    pub fn state(&self) -> Arc<Mutex<RelationState>> {
        Arc::clone(&self.state)
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
        let rewritten = reading::encode_literals(dl_src);

        // FlowLog parses from a file path, so stage the rewritten program in this
        // engine's own temp dir (unique per instance).
        std::fs::create_dir_all(&self.work_dir)
            .map_err(|e| format!("failed to create work dir: {}", e))?;
        let dl_path = self.work_dir.join("program.dl");
        std::fs::write(&dl_path, &rewritten)
            .map_err(|e| format!("failed to write program: {}", e))?;

        let program = std::panic::catch_unwind(|| Program::parse_from(&dl_path.to_string_lossy()))
            .map_err(|_| "failed to parse Datalog program (see stderr)".to_string())?;

        self.compiled = Some((program, rewritten));
        Ok(())
    }

    /// Run the program in streaming mode, blocking until `shutdown` is set.
    pub fn run(&mut self, shutdown: Arc<AtomicBool>) -> Result<(), String> {
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

        // Open each streaming source and route its outputs to relation channels.
        let mut channels = HashMap::new();
        let mut streaming_edbs: HashSet<String> = HashSet::new();
        let edb_names: HashSet<&str> = program.edbs().iter().map(|d| d.name()).collect();

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
            let source = provider
                .open_stream(&binding.config)
                .map_err(|e| format!("failed to open '{}': {}", binding.provider, e))?;

            // Resolve each declared output to a concrete relation and give it a
            // channel. A single-output source with an empty relation name takes
            // the binding's relation; multi-output sources name their own.
            let outputs = source.outputs();
            if outputs.is_empty() {
                return Err(format!(
                    "provider '{}' declared no outputs",
                    binding.provider
                ));
            }
            // Wire a channel only for outputs whose relation the program declares.
            // Outputs the program doesn't use (e.g. ast_span when a rules file
            // only needs ast_node) are dropped, so their rows are never queued.
            let mut senders: HashMap<String, crossbeam_channel::Sender<(Vec<i64>, isize)>> =
                HashMap::new();
            let mut wired: Vec<String> = Vec::new();
            for out in &outputs {
                let rel = if !out.relation.is_empty() {
                    out.relation.clone()
                } else {
                    binding.relation.clone().ok_or_else(|| {
                        format!(
                            "provider '{}' needs a relation name (use 'RELATION={}:...')",
                            binding.provider, binding.provider
                        )
                    })?
                };
                if !edb_names.contains(rel.as_str()) {
                    warn!(
                        "source output relation '{}' not declared in program; ignoring",
                        rel
                    );
                    continue;
                }
                let (tx, rx) = crossbeam_channel::bounded::<(Vec<i64>, isize)>(10_000);
                channels.insert(rel.clone(), rx);
                streaming_edbs.insert(rel.clone());
                senders.insert(rel.clone(), tx);
                wired.push(rel);
            }
            if wired.is_empty() {
                warn!(
                    "provider '{}' feeds no relations used by the program; skipping",
                    binding.provider
                );
                continue;
            }
            // Untagged Insert/Delete target the first wired output.
            let default_rel = wired[0].clone();

            let shutdown_thread = Arc::clone(&shutdown);

            std::thread::spawn(move || {
                let (raw_tx, raw_rx) = crossbeam_channel::bounded::<StreamingUpdate>(10_000);

                // The source produces typed updates on its own thread.
                let shutdown_src = Arc::clone(&shutdown_thread);
                let source_handle = std::thread::spawn(move || source.run(raw_tx, shutdown_src));

                // Encode (via the engine's global interner) and route a row to the
                // channel for `rel`. Returns false when the channel is gone.
                let route = |rel: &str, values: &[DataValue], diff: isize| -> bool {
                    match senders.get(rel) {
                        Some(tx) => {
                            let row: Vec<i64> = values.iter().map(encode_value).collect();
                            tx.send((row, diff)).is_ok()
                        }
                        None => true, // unknown output relation -> drop
                    }
                };

                loop {
                    match raw_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                        Ok(StreamingUpdate::Insert(v)) => {
                            if !route(&default_rel, &v, 1) {
                                break;
                            }
                        }
                        Ok(StreamingUpdate::Delete(v)) => {
                            if !route(&default_rel, &v, -1) {
                                break;
                            }
                        }
                        Ok(StreamingUpdate::InsertInto(rel, v)) => {
                            if !route(&rel, &v, 1) {
                                break;
                            }
                        }
                        Ok(StreamingUpdate::DeleteInto(rel, v)) => {
                            if !route(&rel, &v, -1) {
                                break;
                            }
                        }
                        Ok(StreamingUpdate::BatchInto(rel, rows)) => {
                            // One channel message carried a whole relation's worth of
                            // rows. Encode the whole batch under a single interner
                            // lock (millions of per-value locks otherwise dominate),
                            // then send — never holding the lock across a blocking
                            // send (that would stall the dataflow's decode).
                            if let Some(tx) = senders.get(&rel) {
                                let encoded: Vec<(Vec<i64>, isize)> = {
                                    let mut ig = reading::lock_interner();
                                    rows.iter()
                                        .map(|(values, diff)| {
                                            (
                                                values
                                                    .iter()
                                                    .map(|v| encode_value_locked(&mut ig, v))
                                                    .collect(),
                                                *diff,
                                            )
                                        })
                                        .collect()
                                };
                                let mut closed = false;
                                for (row, diff) in encoded {
                                    if tx.send((row, diff)).is_err() {
                                        closed = true;
                                        break;
                                    }
                                }
                                if closed {
                                    break;
                                }
                            }
                        }
                        Ok(StreamingUpdate::Eof) => break,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                            if shutdown_thread.load(Ordering::Relaxed) {
                                break;
                            }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }

                let _ = source_handle.join();
            });
        }

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
        // The engine decodes `string`/`float` columns before invoking this, so
        // `row_values` arrive already in their textual form.
        let output_callback: Arc<dyn Fn(&str, Vec<String>, isize) + Send + Sync> = Arc::new(
            move |rel_name: &str, row_values: Vec<String>, diff: isize| {
                if !printable.contains(rel_name) || diff == 0 {
                    return;
                }

                // Update the materialized state: a row is present iff net count > 0.
                {
                    let mut st = state_cb.lock().unwrap();
                    let rel_map = st.entry(rel_name.to_string()).or_default();
                    let count = rel_map.entry(row_values.clone()).or_insert(0);
                    *count += diff;
                    if *count <= 0 {
                        rel_map.remove(&row_values);
                    }
                }

                if print {
                    let kind = if diff > 0 { "+" } else { "-" };
                    println!("{} {}({})", kind, rel_name, row_values.join(", "));
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
            },
        );

        let streaming_config = StreamingConfig {
            channels,
            streaming_edbs,
            output_callback,
            shutdown: Arc::clone(&shutdown),
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

        Ok(())
    }
}

impl Default for Dep2 {
    fn default() -> Self {
        Self::new()
    }
}
