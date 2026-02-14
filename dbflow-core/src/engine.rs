use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parsing::decl::{is_null, DataType, NULL_SENTINEL};
use tracing::info;

use catalog::head::aggregation_catalog_from_program;
use executing::arg::Args as FlowlogArgs;
use executing::dataflow::{program_execution, streaming_program_execution, StreamingConfig};
use planning::program::ProgramQueryPlan;
use reading::{KV_MAX, ROW_MAX};
use strata::stratification::Strata;

use crate::compiler::{
    compile, emit_datalog as compiler_emit_datalog, write_facts, CompileResult, FetchedDataBlock,
    RuntimeStringTable, StreamingDataBlock, StringTable,
};
use crate::hcl_types::{parse_hcl_body, HclDataBlock};
use dbflow_plugin::{Plugin, PluginContext, StreamingUpdate};

/// Configuration for the DbFlow engine.
pub struct DbFlowConfig {
    /// Number of worker threads.
    pub workers: usize,
    /// Path to EDB .facts files directory.
    pub facts_dir: Option<PathBuf>,
    /// Output directory for IDB result .csv files.
    pub csvs_dir: Option<PathBuf>,
}

impl Default for DbFlowConfig {
    fn default() -> Self {
        Self {
            workers: 1,
            facts_dir: None,
            csvs_dir: None,
        }
    }
}

/// A single output from execution.
pub struct OutputValue {
    /// User-visible name of the output.
    pub name: String,
    /// Decoded rows. Each row is a vector of string-decoded column values.
    pub rows: Vec<Vec<String>>,
    /// Whether the output CSV file existed but was empty (as opposed to missing entirely).
    pub empty: bool,
}

/// The DbFlow engine encapsulating the full pipeline.
pub struct DbFlow {
    plugins: Vec<Box<dyn Plugin>>,
    plugin_ctx: PluginContext,
    config: DbFlowConfig,
    compiled: Option<CompileResult>,
    /// Streaming sources opened during load, held until execute_streaming().
    streaming_sources: Vec<(String, Box<dyn dbflow_plugin::StreamingDataSource>)>,
}

impl DbFlow {
    /// Create a new engine with default config.
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            plugin_ctx: PluginContext::new(),
            config: DbFlowConfig::default(),
            compiled: None,
            streaming_sources: Vec::new(),
        }
    }

    /// Create a new engine with the given config.
    pub fn with_config(config: DbFlowConfig) -> Self {
        Self {
            plugins: Vec::new(),
            plugin_ctx: PluginContext::new(),
            config,
            compiled: None,
            streaming_sources: Vec::new(),
        }
    }

    /// Register a plugin with the engine.
    pub fn add_plugin(&mut self, plugin: Box<dyn Plugin>) {
        plugin.setup(&mut self.plugin_ctx);
        self.plugins.push(plugin);
    }

    /// Return the names of loaded plugins.
    pub fn loaded_plugins(&self) -> &[String] {
        self.plugin_ctx.registered_plugins()
    }

    /// Parse and compile an HCL source string.
    ///
    /// `base_path` is the directory used to resolve relative module `source` paths.
    pub fn load_hcl(&mut self, source: &str, base_path: Option<&Path>) -> Result<(), String> {
        let body: crate::hcl::Body =
            crate::hcl::from_str(source).map_err(|e| format!("HCL parse error: {}", e))?;

        let hcl_program =
            parse_hcl_body(&body).map_err(|e| format!("HCL compilation error: {}", e))?;

        let (fetched_data, streaming_data) = self.fetch_data_blocks(&hcl_program.data_blocks)?;

        let result = compile(hcl_program, base_path, &fetched_data, &streaming_data)
            .map_err(|e| format!("compilation error: {}", e))?;

        self.compiled = Some(result);
        Ok(())
    }

    /// Fetch data from all `data` blocks using registered data providers.
    /// Returns (batch data blocks, streaming data blocks).
    /// Streaming sources are stored in `self.streaming_sources` for later use.
    fn fetch_data_blocks(
        &mut self,
        data_blocks: &[HclDataBlock],
    ) -> Result<(Vec<FetchedDataBlock>, Vec<StreamingDataBlock>), String> {
        let mut fetched = Vec::new();
        let mut streaming = Vec::new();

        for block in data_blocks {
            // Try streaming provider first; if it declines (returns Err), fall through to batch.
            let mut used_streaming = false;
            if let Some(sp) = self
                .plugin_ctx
                .get_streaming_data_provider(&block.provider_type)
            {
                if let Ok(source) = sp.open_stream(&block.config) {
                    let schema = source.schema().clone();

                    streaming.push(StreamingDataBlock {
                        provider_type: block.provider_type.clone(),
                        label: block.label.clone(),
                        schema,
                    });

                    // Store the source handle for execute_streaming()
                    let rel_name = format!("_data_{}_{}", block.provider_type, block.label);
                    self.streaming_sources.push((rel_name, source));
                    used_streaming = true;
                }
            }

            if !used_streaming {
                if let Some(bp) = self.plugin_ctx.get_data_provider(&block.provider_type) {
                    let source = bp.open(&block.config)?;
                    let schema = source.schema().clone();
                    let rows = source.fetch_all()?;

                    fetched.push(FetchedDataBlock {
                        provider_type: block.provider_type.clone(),
                        label: block.label.clone(),
                        schema,
                        rows,
                    });
                } else {
                    return Err(format!(
                        "no data provider registered for type '{}' (data block data.{}.{})",
                        block.provider_type, block.provider_type, block.label
                    ));
                }
            }
        }
        Ok((fetched, streaming))
    }

    /// Parse and compile an HCL file.
    pub fn load_hcl_file(&mut self, path: &Path) -> Result<(), String> {
        let source = std::fs::read_to_string(path)
            .map_err(|e| format!("can't read {}: {}", path.display(), e))?;

        let base_path = path.parent().unwrap_or_else(|| Path::new("."));
        self.load_hcl(&source, Some(base_path))
    }

    /// Emit the compiled Datalog program as a string.
    pub fn emit_datalog(&self) -> Result<String, String> {
        let result = self.compiled.as_ref().ok_or("no program loaded")?;
        Ok(compiler_emit_datalog(result))
    }

    /// Whether the loaded program has streaming data sources.
    pub fn has_streaming(&self) -> bool {
        self.compiled
            .as_ref()
            .map(|r| !r.streaming_edbs.is_empty())
            .unwrap_or(false)
    }

    /// Execute the compiled program and return decoded outputs (batch mode).
    pub fn execute(&self) -> Result<Vec<OutputValue>, String> {
        let result = self.compiled.as_ref().ok_or("no program loaded")?;

        // Write EDB facts to facts directory.
        let facts_dir = self
            .config
            .facts_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("dbflow-facts"));
        write_facts(&result.edb_facts, &facts_dir)
            .map_err(|e| format!("failed to write facts: {}", e))?;

        info!("wrote EDB facts to {}", facts_dir.display());

        // Write the Datalog program to a temp file.
        let dl_path = std::env::temp_dir().join("dbflow-program.dl");
        std::fs::write(&dl_path, format!("{}", result.program))
            .map_err(|e| format!("failed to write program: {}", e))?;

        // Auto-create csvs directory when outputs exist.
        let has_outputs = !result.outputs.is_empty();
        let csvs_dir = self.config.csvs_dir.clone().or_else(|| {
            if has_outputs {
                Some(std::env::temp_dir().join("dbflow-csvs"))
            } else {
                None
            }
        });

        // Run through FlowLog pipeline.
        let strata = Strata::from_parser(result.program.clone());
        let program_query_plan = ProgramQueryPlan::from_strata(&strata, false, None);
        let use_fat_mode = program_query_plan.should_use_fat_mode(false, KV_MAX, ROW_MAX);
        let idb_map = aggregation_catalog_from_program(&result.program);

        let flowlog_args = FlowlogArgs::new(
            dl_path.to_string_lossy().into_owned(),
            facts_dir.to_string_lossy().into_owned(),
            csvs_dir.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "\t".to_string(),
            self.config.workers,
        );

        program_execution(
            flowlog_args,
            strata,
            program_query_plan.program_plan().to_owned(),
            use_fat_mode,
            idb_map,
        );

        info!("dbflow execution complete");

        // Collect outputs.
        let mut outputs = Vec::new();
        if has_outputs {
            if let Some(ref csvs_dir) = csvs_dir {
                outputs = collect_outputs(result, csvs_dir);
            }
        }

        Ok(outputs)
    }

    /// Execute the compiled program in streaming mode.
    /// This blocks until the shutdown flag is set.
    pub fn execute_streaming(&mut self, shutdown: Arc<AtomicBool>) -> Result<(), String> {
        let result = self.compiled.as_ref().ok_or("no program loaded")?;

        // Write EDB facts to facts directory (batch EDBs, streaming ones are empty).
        let facts_dir = self
            .config
            .facts_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("dbflow-facts"));
        write_facts(&result.edb_facts, &facts_dir)
            .map_err(|e| format!("failed to write facts: {}", e))?;

        info!("wrote EDB facts to {}", facts_dir.display());

        // Write the Datalog program to a temp file.
        let dl_path = std::env::temp_dir().join("dbflow-program.dl");
        std::fs::write(&dl_path, format!("{}", result.program))
            .map_err(|e| format!("failed to write program: {}", e))?;

        // Auto-create csvs directory when outputs exist.
        let has_outputs = !result.outputs.is_empty();
        let csvs_dir = self.config.csvs_dir.clone().or_else(|| {
            if has_outputs {
                Some(std::env::temp_dir().join("dbflow-csvs"))
            } else {
                None
            }
        });

        // Build runtime string table for streaming encoding.
        let runtime_st = Arc::new(RuntimeStringTable::from(result.string_table.clone()));

        // Create channels for streaming EDBs and spawn source threads.
        let mut streaming_channels = HashMap::new();
        let streaming_sources = std::mem::take(&mut self.streaming_sources);

        for (rel_name, source) in streaming_sources {
            let (encoded_tx, encoded_rx) = crossbeam_channel::bounded::<(Vec<i64>, isize)>(10_000);
            streaming_channels.insert(rel_name.clone(), encoded_rx);

            // Collect function EDB channels that depend on this source,
            // including chained fn EDBs (where one fn EDB feeds another).
            // Each entry: (sender, input_col_idx, fn_kind, chained_fn_edb_senders)
            // Chained senders are fn EDBs whose source is the current fn EDB.
            struct FnEdbSender {
                tx: crossbeam_channel::Sender<(Vec<i64>, isize)>,
                input_col_idx: usize,
                fn_kind: crate::compiler::ScalarFnKind,
                children: Vec<FnEdbSender>,
            }

            // Build fn EDB senders recursively: find all fn EDBs whose source is `parent_name`,
            // and for each one, recursively find fn EDBs chained off it.
            fn build_fn_edb_tree(
                parent_name: &str,
                all_fn_edbs: &[crate::compiler::StreamingFnEdb],
                streaming_channels: &mut HashMap<String, crossbeam_channel::Receiver<(Vec<i64>, isize)>>,
            ) -> Vec<FnEdbSender> {
                all_fn_edbs
                    .iter()
                    .filter(|fe| fe.source_edb_name == parent_name)
                    .map(|fe| {
                        let (fn_tx, fn_rx) =
                            crossbeam_channel::bounded::<(Vec<i64>, isize)>(10_000);
                        streaming_channels.insert(fe.fn_edb_name.clone(), fn_rx);
                        let children = build_fn_edb_tree(&fe.fn_edb_name, all_fn_edbs, streaming_channels);
                        FnEdbSender {
                            tx: fn_tx,
                            input_col_idx: fe.input_col_idx,
                            fn_kind: fe.function.clone(),
                            children,
                        }
                    })
                    .collect()
            }

            let fn_edb_senders = build_fn_edb_tree(&rel_name, &result.streaming_fn_edbs, &mut streaming_channels);

            let runtime_st_clone = Arc::clone(&runtime_st);
            let shutdown_clone = Arc::clone(&shutdown);

            // Background thread: source sends StreamingUpdate → we encode to i32 → send to channel
            std::thread::spawn(move || {
                let (raw_tx, raw_rx) = crossbeam_channel::bounded::<StreamingUpdate>(10_000);

                // Spawn the source runner in its own thread
                let shutdown_inner = Arc::clone(&shutdown_clone);
                let source_handle = std::thread::spawn(move || {
                    source.run(raw_tx, shutdown_inner);
                });

                // Encode a Vec<DataValue> into Vec<i64>
                let encode_values = |values: &[dbflow_plugin::DataValue]| -> Vec<i64> {
                    values
                        .iter()
                        .map(|v| match v {
                            dbflow_plugin::DataValue::String(s) => runtime_st_clone.intern(s),
                            dbflow_plugin::DataValue::Integer(i) => *i,
                            dbflow_plugin::DataValue::Float(f) => {
                                let bits = f.to_bits() as i64;
                                if bits == NULL_SENTINEL {
                                    NULL_SENTINEL + 1
                                } else {
                                    bits
                                }
                            }
                            dbflow_plugin::DataValue::Bool(b) => {
                                if *b {
                                    1
                                } else {
                                    0
                                }
                            }
                            dbflow_plugin::DataValue::Null => NULL_SENTINEL,
                        })
                        .collect()
                };

                // Send computed function values to fn EDB channels, recursively
                // handling chained fn EDBs.
                fn send_fn_edb_values(
                    row: &[i64],
                    diff: isize,
                    senders: &[FnEdbSender],
                ) {
                    for sender in senders {
                        if sender.input_col_idx < row.len() {
                            let input_val = row[sender.input_col_idx];
                            let output_val =
                                crate::compiler::compile::apply_scalar_fn(&sender.fn_kind, input_val);
                            let fn_row = vec![input_val, output_val];
                            // Recursively send to any fn EDBs chained off this one.
                            send_fn_edb_values(&fn_row, diff, &sender.children);
                            let _ = sender.tx.send((fn_row, diff));
                        }
                    }
                }

                // Encoding loop: convert DataValues to i64 with diff
                loop {
                    match raw_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                        Ok(StreamingUpdate::Insert(values)) => {
                            let encoded = encode_values(&values);
                            send_fn_edb_values(&encoded, 1, &fn_edb_senders);
                            if encoded_tx.send((encoded, 1)).is_err() {
                                break;
                            }
                        }
                        Ok(StreamingUpdate::Delete(values)) => {
                            let encoded = encode_values(&values);
                            send_fn_edb_values(&encoded, -1, &fn_edb_senders);
                            if encoded_tx.send((encoded, -1)).is_err() {
                                break;
                            }
                        }
                        Ok(StreamingUpdate::Eof) => break,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                            if shutdown_clone.load(Ordering::Relaxed) {
                                break;
                            }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }

                let _ = source_handle.join();
            });
        }

        // Build output callback using runtime string table.
        let output_infos: Vec<(String, Vec<DataType>)> = result
            .outputs
            .iter()
            .map(|o| (o.relation_name.clone(), o.column_types.clone()))
            .collect();

        let output_names: HashMap<String, String> = result
            .outputs
            .iter()
            .map(|o| (o.relation_name.clone(), o.name.clone()))
            .collect();

        let runtime_st_cb = Arc::clone(&runtime_st);
        let output_callback: Arc<dyn Fn(&str, Vec<String>, isize) + Send + Sync> = Arc::new(
            move |rel_name: &str, row_values: Vec<String>, diff: isize| {
                // Only print output for user-defined output blocks, skip intermediate IDBs.
                let display_name = match output_names.get(rel_name) {
                    Some(name) => name.as_str(),
                    None => return,
                };

                // Find the output info for this relation to decode values.
                let col_types = output_infos
                    .iter()
                    .find(|(rn, _)| rn == rel_name)
                    .map(|(_, ct)| ct.as_slice());

                let decoded: Vec<String> = row_values
                    .iter()
                    .enumerate()
                    .map(|(i, val_str)| {
                        let col_type = col_types.and_then(|ct| ct.get(i));
                        match col_type {
                            Some(DataType::String) => {
                                if let Ok(id) = val_str.parse::<i64>() {
                                    runtime_st_cb.decode(id).unwrap_or_else(|| val_str.clone())
                                } else {
                                    val_str.clone()
                                }
                            }
                            Some(DataType::Float) => {
                                if let Ok(bits) = val_str.parse::<i64>() {
                                    let f = f64::from_bits(bits as u64);
                                    format!("{}", f)
                                } else {
                                    val_str.clone()
                                }
                            }
                            _ => val_str.clone(),
                        }
                    })
                    .collect();

                if diff > 0 {
                    println!("output \"{}\": {}", display_name, decoded.join(", "));
                } else if diff < 0 {
                    println!("retract \"{}\": {}", display_name, decoded.join(", "));
                }
                use std::io::Write;
                let _ = std::io::stdout().flush();
            },
        );

        // Build streaming config.
        let streaming_edb_set: HashSet<String> = result.streaming_edbs.iter().cloned().collect();

        let streaming_config = StreamingConfig {
            channels: streaming_channels,
            streaming_edbs: streaming_edb_set,
            output_callback,
            shutdown: Arc::clone(&shutdown),
        };

        // Run through FlowLog pipeline.
        let strata = Strata::from_parser(result.program.clone());
        let program_query_plan = ProgramQueryPlan::from_strata(&strata, false, None);
        let use_fat_mode = program_query_plan.should_use_fat_mode(false, KV_MAX, ROW_MAX);
        let idb_map = aggregation_catalog_from_program(&result.program);

        let flowlog_args = FlowlogArgs::new(
            dl_path.to_string_lossy().into_owned(),
            facts_dir.to_string_lossy().into_owned(),
            csvs_dir.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "\t".to_string(),
            self.config.workers,
        );

        streaming_program_execution(
            flowlog_args,
            strata,
            program_query_plan.program_plan().to_owned(),
            use_fat_mode,
            idb_map,
            streaming_config,
        );

        info!("dbflow streaming execution complete");
        Ok(())
    }
}

impl Default for DbFlow {
    fn default() -> Self {
        Self::new()
    }
}

/// Collect and decode output values from the compile result and CSV directory.
fn collect_outputs(result: &CompileResult, csvs_dir: &Path) -> Vec<OutputValue> {
    let mut outputs = Vec::new();

    for output_info in &result.outputs {
        // Literal outputs are compiled as EDB facts — decode from in-memory facts.
        if let Some(facts) = result.edb_facts.get(&output_info.relation_name) {
            if !facts.is_empty() {
                let rows: Vec<Vec<String>> = facts
                    .iter()
                    .map(|tuple| {
                        tuple
                            .iter()
                            .enumerate()
                            .map(|(i, v)| {
                                decode_value(*v, &output_info.column_types, i, &result.string_table)
                            })
                            .collect()
                    })
                    .collect();
                outputs.push(OutputValue {
                    name: output_info.name.clone(),
                    rows,
                    empty: false,
                });
                continue;
            }
        }

        let csv_path = csvs_dir
            .join("csvs")
            .join(format!("{}.csv", output_info.relation_name));

        if !csv_path.exists() {
            outputs.push(OutputValue {
                name: output_info.name.clone(),
                rows: Vec::new(),
                empty: false,
            });
            continue;
        }

        let content = match std::fs::read_to_string(&csv_path) {
            Ok(c) => c,
            Err(_) => {
                outputs.push(OutputValue {
                    name: output_info.name.clone(),
                    rows: Vec::new(),
                    empty: false,
                });
                continue;
            }
        };

        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        if lines.is_empty() {
            outputs.push(OutputValue {
                name: output_info.name.clone(),
                rows: Vec::new(),
                empty: true,
            });
            continue;
        }

        let rows: Vec<Vec<String>> = lines
            .iter()
            .map(|line| decode_csv_line(line, &output_info.column_types, &result.string_table))
            .collect();

        outputs.push(OutputValue {
            name: output_info.name.clone(),
            rows,
            empty: false,
        });
    }

    outputs
}

/// Decode a single i64 value using the column type and string table.
fn decode_value(
    val: i64,
    column_types: &[DataType],
    col_idx: usize,
    string_table: &StringTable,
) -> String {
    if is_null(val) {
        return "NULL".to_string();
    }
    match column_types.get(col_idx) {
        Some(DataType::String) => string_table
            .decode(val)
            .map(|s| s.to_string())
            .unwrap_or_else(|| val.to_string()),
        Some(DataType::Float) => {
            let f = f64::from_bits(val as u64);
            format!("{}", f)
        }
        _ => val.to_string(),
    }
}

/// Decode a CSV line (comma-space separated i64 values) using the string table.
fn decode_csv_line(
    line: &str,
    column_types: &[DataType],
    string_table: &StringTable,
) -> Vec<String> {
    line.split(", ")
        .enumerate()
        .map(|(i, val_str)| {
            let val: i64 = val_str.trim().parse().unwrap_or(0);
            decode_value(val, column_types, i, string_table)
        })
        .collect()
}
