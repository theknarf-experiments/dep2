use std::path::{Path, PathBuf};

use clap::Parser;
use mimalloc::MiMalloc;
use parsing::decl::DataType;
use tracing::info;
use tracing_subscriber::EnvFilter;

use catalog::head::aggregation_catalog_from_program;
use executing::arg::Args as FlowlogArgs;
use executing::dataflow::program_execution;
use planning::program::ProgramQueryPlan;
use reading::KV_MAX;
use reading::ROW_MAX;
use strata::stratification::Strata;

use dbflow_core::compiler::{compile, emit_datalog, write_facts, CompileResult};
use dbflow_core::hcl_types::parse_hcl_body;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Parser, Debug)]
#[command(name = "dbflow", version, about = "HCL front-end for FlowLog")]
struct Cli {
    /// Input HCL file
    input: PathBuf,

    /// Print generated Datalog and exit
    #[arg(long)]
    emit_dl: bool,

    /// Path to EDB .facts files (additional external facts)
    #[arg(short = 'f', long = "facts")]
    facts_dir: Option<PathBuf>,

    /// Output directory for IDB result .csv files
    #[arg(short = 'c', long = "csvs")]
    csvs_dir: Option<PathBuf>,

    /// Number of worker threads
    #[arg(short = 'w', long = "workers", default_value_t = 1)]
    workers: usize,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // 1. Read and parse HCL file.
    let hcl_source = std::fs::read_to_string(&cli.input)
        .unwrap_or_else(|e| panic!("can't read {}: {}", cli.input.display(), e));

    let body: dbflow_core::hcl::Body = dbflow_core::hcl::from_str(&hcl_source)
        .unwrap_or_else(|e| panic!("HCL parse error in {}: {}", cli.input.display(), e));

    // 2. Build HclProgram from parsed body.
    let hcl_program = parse_hcl_body(&body)
        .unwrap_or_else(|e| panic!("HCL compilation error: {}", e));

    // 3. Compile to FlowLog Program.
    let base_path = cli.input.parent().unwrap_or_else(|| Path::new("."));
    let result = compile(hcl_program, Some(base_path))
        .unwrap_or_else(|e| panic!("compilation error: {}", e));

    // 4. If --emit-dl: print and exit.
    if cli.emit_dl {
        print!("{}", emit_datalog(&result));
        return;
    }

    // 5. Write EDB facts to a temporary directory.
    let facts_dir = cli.facts_dir.unwrap_or_else(|| {
        std::env::temp_dir().join("dbflow-facts")
    });
    write_facts(&result.edb_facts, &facts_dir)
        .unwrap_or_else(|e| panic!("failed to write facts: {}", e));

    info!("wrote EDB facts to {}", facts_dir.display());

    // 6. Write the Datalog program to a temp file (needed for FlowLog's Args).
    let dl_path = std::env::temp_dir().join("dbflow-program.dl");
    std::fs::write(&dl_path, format!("{}", result.program))
        .unwrap_or_else(|e| panic!("failed to write program: {}", e));

    // 7. Auto-create csvs directory when outputs exist.
    let has_outputs = !result.outputs.is_empty();
    let csvs_dir = cli.csvs_dir.or_else(|| {
        if has_outputs {
            Some(std::env::temp_dir().join("dbflow-csvs"))
        } else {
            None
        }
    });

    // 8. Run through FlowLog pipeline.
    let strata = Strata::from_parser(result.program.clone());

    let program_query_plan = ProgramQueryPlan::from_strata(&strata, false, None);

    let use_fat_mode = program_query_plan.should_use_fat_mode(false, KV_MAX, ROW_MAX);

    let idb_map = aggregation_catalog_from_program(&result.program);

    let flowlog_args = FlowlogArgs::new(
        dl_path.to_string_lossy().into_owned(),
        facts_dir.to_string_lossy().into_owned(),
        csvs_dir.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "\t".to_string(),
        cli.workers,
    );

    program_execution(
        flowlog_args,
        strata,
        program_query_plan.program_plan().to_owned(),
        use_fat_mode,
        idb_map,
    );

    info!("dbflow execution complete");

    // 9. Read and display outputs.
    if has_outputs {
        if let Some(ref csvs_dir) = csvs_dir {
            display_outputs(&result, csvs_dir);
        }
    }
}

/// Read output CSV files and display decoded results to stdout.
fn display_outputs(result: &CompileResult, csvs_dir: &PathBuf) {
    for output_info in &result.outputs {
        // Literal outputs are compiled as EDB facts (not IDB rules), so they
        // won't appear in CSV output. Decode them directly from in-memory facts.
        if let Some(facts) = result.edb_facts.get(&output_info.relation_name) {
            for tuple in facts {
                let decoded: Vec<String> = tuple
                    .iter()
                    .enumerate()
                    .map(|(i, v)| decode_value(*v, &output_info.column_types, i, &result.string_table))
                    .collect();
                println!("output \"{}\": {}", output_info.name, decoded.join(", "));
            }
            continue;
        }

        let csv_path = csvs_dir.join("csvs").join(format!("{}.csv", output_info.relation_name));

        if !csv_path.exists() {
            // Output relation might be empty or not computed.
            println!("output \"{}\": (no results)", output_info.name);
            continue;
        }

        let content = match std::fs::read_to_string(&csv_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: could not read output '{}': {}", output_info.name, e);
                continue;
            }
        };

        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        if lines.is_empty() {
            println!("output \"{}\": (empty)", output_info.name);
            continue;
        }

        // For single-value outputs, print inline.
        if lines.len() == 1 && output_info.column_types.len() == 1 {
            let decoded = decode_csv_line(lines[0], &output_info.column_types, &result.string_table);
            println!("output \"{}\": {}", output_info.name, decoded.join(", "));
        } else {
            println!("output \"{}\":", output_info.name);
            for line in &lines {
                let decoded = decode_csv_line(line, &output_info.column_types, &result.string_table);
                println!("  {}", decoded.join(", "));
            }
        }
    }
}

/// Decode a single i32 value using the column type and string table.
fn decode_value(
    val: i32,
    column_types: &[DataType],
    col_idx: usize,
    string_table: &dbflow_core::compiler::StringTable,
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
    string_table: &dbflow_core::compiler::StringTable,
) -> Vec<String> {
    line.split(", ")
        .enumerate()
        .map(|(i, val_str)| {
            let val: i32 = val_str.trim().parse().unwrap_or(0);
            decode_value(val, column_types, i, string_table)
        })
        .collect()
}
