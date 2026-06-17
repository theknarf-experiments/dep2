//! The HCL-free Dep2 engine.
//!
//! Register streaming plugins, bind each Datalog relation to a streaming data
//! source, load a native `.dl` program, then [`Dep2::run`] to stream updates
//! into FlowLog continuously until shutdown.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parsing::decl::DataType;
use parsing::parser::Program;
use tracing::{info, warn};

use catalog::head::aggregation_catalog_from_program;
use executing::arg::Args as FlowlogArgs;
use executing::dataflow::{streaming_program_execution, StreamingConfig};
use planning::program::ProgramQueryPlan;
use reading::{KV_MAX, ROW_MAX};
use strata::stratification::Strata;

use dep2_plugin::{Plugin, PluginContext, StreamingUpdate};

use crate::string_table::{encode_value, intern_string_literals, RuntimeStringTable};

/// Engine configuration.
pub struct Dep2Config {
    /// Number of FlowLog worker threads.
    pub workers: usize,
}

impl Default for Dep2Config {
    fn default() -> Self {
        Self { workers: 1 }
    }
}

/// Binds a Datalog relation to a streaming data source provided by a plugin.
pub struct SourceBinding {
    /// The EDB relation name in the `.dl` program this source feeds.
    pub relation: String,
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
    /// Shared interning table: literals, streamed values, and outputs all use it.
    string_table: Arc<RuntimeStringTable>,
}

impl Dep2 {
    pub fn new() -> Self {
        Self::with_config(Dep2Config::default())
    }

    pub fn with_config(config: Dep2Config) -> Self {
        Self {
            plugins: Vec::new(),
            plugin_ctx: PluginContext::new(),
            config,
            bindings: Vec::new(),
            compiled: None,
            string_table: Arc::new(RuntimeStringTable::new()),
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

    /// Bind a Datalog relation to a streaming source from a registered provider.
    pub fn add_source(
        &mut self,
        relation: impl Into<String>,
        provider: impl Into<String>,
        config: HashMap<String, String>,
    ) {
        self.bindings.push(SourceBinding {
            relation: relation.into(),
            provider: provider.into(),
            config,
        });
    }

    /// Load a native FlowLog `.dl` program. String literals are interned into
    /// the shared table and replaced with integer ids before FlowLog sees them.
    pub fn load_program(&mut self, dl_src: &str) -> Result<(), String> {
        let rewritten = intern_string_literals(dl_src, &self.string_table);

        // FlowLog parses from a file path, so stage the rewritten program.
        let dl_path = std::env::temp_dir().join("dep2-program.dl");
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
        let facts_dir = std::env::temp_dir().join("dep2-facts");
        std::fs::create_dir_all(&facts_dir)
            .map_err(|e| format!("failed to create facts dir: {}", e))?;
        for decl in program.edbs() {
            let path = facts_dir.join(format!("{}.facts", decl.name()));
            std::fs::write(&path, "").map_err(|e| format!("failed to write facts: {}", e))?;
        }

        let dl_path = std::env::temp_dir().join("dep2-program.dl");
        std::fs::write(&dl_path, dl_text).map_err(|e| format!("failed to write program: {}", e))?;

        // Validate bindings against declared EDBs.
        let edb_names: HashSet<&str> = program.edbs().iter().map(|d| d.name()).collect();
        for b in &self.bindings {
            if !edb_names.contains(b.relation.as_str()) {
                warn!(
                    "source binding for relation '{}' has no matching .decl in the program",
                    b.relation
                );
            }
        }

        // Open each streaming source and spawn an encoding thread per source.
        let mut channels = HashMap::new();
        let streaming_edbs: HashSet<String> =
            self.bindings.iter().map(|b| b.relation.clone()).collect();

        for binding in &self.bindings {
            let provider = self
                .plugin_ctx
                .get_streaming_data_provider(&binding.provider)
                .ok_or_else(|| {
                    format!(
                        "no streaming provider registered for '{}' (relation '{}')",
                        binding.provider, binding.relation
                    )
                })?;
            let source = provider
                .open_stream(&binding.config)
                .map_err(|e| format!("failed to open '{}': {}", binding.provider, e))?;

            let (encoded_tx, encoded_rx) = crossbeam_channel::bounded::<(Vec<i64>, isize)>(10_000);
            channels.insert(binding.relation.clone(), encoded_rx);

            let table = Arc::clone(&self.string_table);
            let shutdown_thread = Arc::clone(&shutdown);

            std::thread::spawn(move || {
                let (raw_tx, raw_rx) = crossbeam_channel::bounded::<StreamingUpdate>(10_000);

                // The source produces typed updates on its own thread.
                let shutdown_src = Arc::clone(&shutdown_thread);
                let source_handle = std::thread::spawn(move || source.run(raw_tx, shutdown_src));

                // Encode typed updates into FlowLog's (Vec<i64>, diff) form.
                loop {
                    match raw_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                        Ok(StreamingUpdate::Insert(values)) => {
                            let row: Vec<i64> =
                                values.iter().map(|v| encode_value(v, &table)).collect();
                            if encoded_tx.send((row, 1)).is_err() {
                                break;
                            }
                        }
                        Ok(StreamingUpdate::Delete(values)) => {
                            let row: Vec<i64> =
                                values.iter().map(|v| encode_value(v, &table)).collect();
                            if encoded_tx.send((row, -1)).is_err() {
                                break;
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

        // Output decoding: map each IDB relation to its declared column types.
        let output_types: HashMap<String, Vec<DataType>> = program
            .idbs()
            .iter()
            .map(|d| {
                let types = d.attributes().iter().map(|a| *a.data_type()).collect();
                (d.name().to_string(), types)
            })
            .collect();

        // Only print *terminal* IDBs: relations not consumed by any other rule's
        // body (self-recursion doesn't count). Intermediate relations stay quiet.
        let mut consumed: HashSet<String> = HashSet::new();
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
                        consumed.insert(n.to_string());
                    }
                }
            }
        }
        let printable: HashSet<String> = output_types
            .keys()
            .filter(|n| !consumed.contains(*n))
            .cloned()
            .collect();

        let table_cb = Arc::clone(&self.string_table);
        let output_callback: Arc<dyn Fn(&str, Vec<String>, isize) + Send + Sync> = Arc::new(
            move |rel_name: &str, row_values: Vec<String>, diff: isize| {
                if !printable.contains(rel_name) {
                    return;
                }
                let col_types = output_types.get(rel_name);
                let decoded: Vec<String> = row_values
                    .iter()
                    .enumerate()
                    .map(|(i, val_str)| decode_field(val_str, col_types, i, &table_cb))
                    .collect();

                let kind = if diff > 0 {
                    "+"
                } else if diff < 0 {
                    "-"
                } else {
                    return;
                };
                println!("{} {}({})", kind, rel_name, decoded.join(", "));
                use std::io::Write;
                let _ = std::io::stdout().flush();
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

/// Decode one output field from its stringified `i64` using the column type.
fn decode_field(
    val_str: &str,
    col_types: Option<&Vec<DataType>>,
    col_idx: usize,
    table: &RuntimeStringTable,
) -> String {
    match col_types.and_then(|ct| ct.get(col_idx)) {
        Some(DataType::String) => match val_str.parse::<i64>() {
            Ok(id) => table.decode(id).unwrap_or_else(|| val_str.to_string()),
            Err(_) => val_str.to_string(),
        },
        Some(DataType::Float) => match val_str.parse::<i64>() {
            Ok(bits) => format!("{}", f64::from_bits(bits as u64)),
            Err(_) => val_str.to_string(),
        },
        _ => val_str.to_string(),
    }
}
