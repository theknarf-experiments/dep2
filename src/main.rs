use std::path::PathBuf;

use clap::Parser;
use mimalloc::MiMalloc;
use tracing_subscriber::EnvFilter;

use dbflow_core::engine::{DbFlow, DbFlowConfig, OutputValue};

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

    let config = DbFlowConfig {
        workers: cli.workers,
        facts_dir: cli.facts_dir,
        csvs_dir: cli.csvs_dir,
    };

    let mut engine = DbFlow::with_config(config);

    engine.add_plugin(Box::new(dbflow_plugin_kafka::KafkaPlugin));
    engine.add_plugin(Box::new(dbflow_plugin_csv::CsvPlugin));
    engine.add_plugin(Box::new(dbflow_plugin_postgres::PostgresPlugin));

    engine
        .load_hcl_file(&cli.input)
        .unwrap_or_else(|e| panic!("{}", e));

    if cli.emit_dl {
        let dl = engine.emit_datalog().unwrap_or_else(|e| panic!("{}", e));
        print!("{}", dl);
        return;
    }

    let outputs = engine.execute().unwrap_or_else(|e| panic!("{}", e));

    display_outputs(&outputs);
}

/// Display output values to stdout, preserving the exact format expected by e2e tests.
fn display_outputs(outputs: &[OutputValue]) {
    for output in outputs {
        if output.rows.is_empty() {
            if output.empty {
                println!("output \"{}\": (empty)", output.name);
            } else {
                println!("output \"{}\": (no results)", output.name);
            }
            continue;
        }

        // For single-value outputs, print inline.
        if output.rows.len() == 1 && output.rows[0].len() == 1 {
            println!("output \"{}\": {}", output.name, output.rows[0].join(", "));
        } else if output.rows.len() == 1 && output.rows[0].len() > 1 {
            // Single row, multiple columns — still inline for literal EDB outputs.
            println!("output \"{}\": {}", output.name, output.rows[0].join(", "));
        } else {
            println!("output \"{}\":", output.name);
            for row in &output.rows {
                println!("  {}", row.join(", "));
            }
        }
    }
}
