use std::path::{Path, PathBuf};

use parsing::decl::DataType;
use tracing::info;

use catalog::head::aggregation_catalog_from_program;
use executing::arg::Args as FlowlogArgs;
use executing::dataflow::program_execution;
use planning::program::ProgramQueryPlan;
use reading::{KV_MAX, ROW_MAX};
use strata::stratification::Strata;

use crate::compiler::{compile, emit_datalog as compiler_emit_datalog, write_facts, CompileResult, StringTable};
use crate::hcl_types::parse_hcl_body;
use dbflow_plugin::{Plugin, PluginContext};

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
}

impl DbFlow {
    /// Create a new engine with default config.
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            plugin_ctx: PluginContext::new(),
            config: DbFlowConfig::default(),
            compiled: None,
        }
    }

    /// Create a new engine with the given config.
    pub fn with_config(config: DbFlowConfig) -> Self {
        Self {
            plugins: Vec::new(),
            plugin_ctx: PluginContext::new(),
            config,
            compiled: None,
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
        let body: crate::hcl::Body = crate::hcl::from_str(source)
            .map_err(|e| format!("HCL parse error: {}", e))?;

        let hcl_program = parse_hcl_body(&body)
            .map_err(|e| format!("HCL compilation error: {}", e))?;

        let result = compile(hcl_program, base_path)
            .map_err(|e| format!("compilation error: {}", e))?;

        self.compiled = Some(result);
        Ok(())
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

    /// Execute the compiled program and return decoded outputs.
    pub fn execute(&self) -> Result<Vec<OutputValue>, String> {
        let result = self.compiled.as_ref().ok_or("no program loaded")?;

        // Write EDB facts to facts directory.
        let facts_dir = self.config.facts_dir.clone().unwrap_or_else(|| {
            std::env::temp_dir().join("dbflow-facts")
        });
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
            let rows: Vec<Vec<String>> = facts
                .iter()
                .map(|tuple| {
                    tuple
                        .iter()
                        .enumerate()
                        .map(|(i, v)| decode_value(*v, &output_info.column_types, i, &result.string_table))
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

        let csv_path = csvs_dir.join("csvs").join(format!("{}.csv", output_info.relation_name));

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

/// Decode a single i32 value using the column type and string table.
fn decode_value(
    val: i32,
    column_types: &[DataType],
    col_idx: usize,
    string_table: &StringTable,
) -> String {
    let is_string = column_types
        .get(col_idx)
        .map(|dt| matches!(dt, DataType::String))
        .unwrap_or(false);
    if is_string {
        string_table
            .decode(val)
            .map(|s| s.to_string())
            .unwrap_or_else(|| val.to_string())
    } else {
        val.to_string()
    }
}

/// Decode a CSV line (comma-space separated i32 values) using the string table.
fn decode_csv_line(
    line: &str,
    column_types: &[DataType],
    string_table: &StringTable,
) -> Vec<String> {
    line.split(", ")
        .enumerate()
        .map(|(i, val_str)| {
            let val: i32 = val_str.trim().parse().unwrap_or(0);
            decode_value(val, column_types, i, string_table)
        })
        .collect()
}
