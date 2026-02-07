use std::path::PathBuf;

use clap::Parser;
use mimalloc::MiMalloc;
use tracing::info;
use tracing_subscriber::EnvFilter;

use catalog::head::aggregation_catalog_from_program;
use executing::arg::Args as FlowlogArgs;
use executing::dataflow::program_execution;
use planning::program::ProgramQueryPlan;
use reading::KV_MAX;
use reading::ROW_MAX;
use strata::stratification::Strata;

use hcl_flowlog::compiler::{compile, emit_datalog, write_facts};
use hcl_flowlog::hcl_types::parse_hcl_body;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Parser, Debug)]
#[command(name = "hcl-flowlog", version, about = "HCL front-end for FlowLog")]
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

    let body: hcl::Body = hcl::from_str(&hcl_source)
        .unwrap_or_else(|e| panic!("HCL parse error in {}: {}", cli.input.display(), e));

    // 2. Build HclProgram from parsed body.
    let hcl_program = parse_hcl_body(&body)
        .unwrap_or_else(|e| panic!("HCL compilation error: {}", e));

    // 3. Compile to FlowLog Program.
    let result = compile(hcl_program)
        .unwrap_or_else(|e| panic!("compilation error: {}", e));

    // 4. If --emit-dl: print and exit.
    if cli.emit_dl {
        print!("{}", emit_datalog(&result));
        return;
    }

    // 5. Write EDB facts to a temporary directory.
    let facts_dir = cli.facts_dir.unwrap_or_else(|| {
        let dir = std::env::temp_dir().join("hcl-flowlog-facts");
        dir
    });
    write_facts(&result.edb_facts, &facts_dir)
        .unwrap_or_else(|e| panic!("failed to write facts: {}", e));

    info!("wrote EDB facts to {}", facts_dir.display());

    // 6. Write the Datalog program to a temp file (needed for FlowLog's Args).
    let dl_path = std::env::temp_dir().join("hcl-flowlog-program.dl");
    std::fs::write(&dl_path, format!("{}", result.program))
        .unwrap_or_else(|e| panic!("failed to write program: {}", e));

    // 7. Run through FlowLog pipeline.
    let program = result.program;

    let strata = Strata::from_parser(program.clone());

    let program_query_plan = ProgramQueryPlan::from_strata(&strata, false, None);

    let use_fat_mode = program_query_plan.should_use_fat_mode(false, KV_MAX, ROW_MAX);

    let idb_map = aggregation_catalog_from_program(&program);

    let flowlog_args = FlowlogArgs::new(
        dl_path.to_string_lossy().into_owned(),
        facts_dir.to_string_lossy().into_owned(),
        cli.csvs_dir.map(|p| p.to_string_lossy().into_owned()),
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

    info!("hcl-flowlog execution complete");
}
