use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::Parser;
use mimalloc::MiMalloc;
use tracing_subscriber::EnvFilter;

use dep2_core::engine::{Dep2, Dep2Config};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// Live semantic analysis: stream data sources into a FlowLog Datalog program.
#[derive(Parser, Debug)]
#[command(name = "dep2", version, about = "Streaming FlowLog runner")]
struct Cli {
    /// Native FlowLog `.dl` program to run.
    program: PathBuf,

    /// Bind a relation to a streaming source:
    /// `RELATION=PROVIDER[:k=v;k=v...]` (repeatable). Config pairs are
    /// `;`-separated so individual values may contain commas.
    #[arg(short = 's', long = "source")]
    sources: Vec<String>,

    /// Number of worker threads.
    #[arg(short = 'w', long = "workers", default_value_t = 1)]
    workers: usize,
}

/// Parse a `RELATION=PROVIDER[:k=v,...]` source spec.
fn parse_source(spec: &str) -> Result<(String, String, HashMap<String, String>), String> {
    let (relation, rest) = spec
        .split_once('=')
        .ok_or_else(|| format!("invalid --source '{}': expected RELATION=PROVIDER", spec))?;
    let (provider, cfg_str) = match rest.split_once(':') {
        Some((p, c)) => (p, c),
        None => (rest, ""),
    };
    let mut config = HashMap::new();
    if !cfg_str.is_empty() {
        for pair in cfg_str.split(';') {
            let (k, v) = pair
                .split_once('=')
                .ok_or_else(|| format!("invalid config pair '{}' in --source", pair))?;
            config.insert(k.to_string(), v.to_string());
        }
    }
    Ok((relation.to_string(), provider.to_string(), config))
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let mut engine = Dep2::with_config(Dep2Config {
        workers: cli.workers,
    });

    // Register available plugins.
    engine.add_plugin(Box::new(dep2_plugin_csv::CsvPlugin));
    engine.add_plugin(Box::new(dep2_plugin_fs::FsPlugin));
    engine.add_plugin(Box::new(dep2_plugin_treesitter::TreeSitterPlugin));

    // Bind sources from the command line.
    for spec in &cli.sources {
        let (relation, provider, config) = parse_source(spec).unwrap_or_else(|e| panic!("{}", e));
        engine.add_source(relation, provider, config);
    }

    let program_src = std::fs::read_to_string(&cli.program)
        .unwrap_or_else(|e| panic!("can't read {}: {}", cli.program.display(), e));
    engine
        .load_program(&program_src)
        .unwrap_or_else(|e| panic!("{}", e));

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_handler = Arc::clone(&shutdown);
    ctrlc::set_handler(move || {
        eprintln!("\nShutting down...");
        shutdown_handler.store(true, Ordering::Relaxed);
    })
    .expect("failed to set Ctrl-C handler");

    engine.run(shutdown).unwrap_or_else(|e| panic!("{}", e));
}
