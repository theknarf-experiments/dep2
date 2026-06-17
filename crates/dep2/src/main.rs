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

    /// Bind a streaming source: `[RELATION=]PROVIDER[:k=v;k=v...]` (repeatable).
    /// RELATION is omitted for multi-output providers (e.g. treesitter, which
    /// feeds ast_node + ast_span). Config pairs are `;`-separated so values may
    /// contain commas.
    #[arg(short = 's', long = "source")]
    sources: Vec<String>,

    /// Number of worker threads.
    #[arg(short = 'w', long = "workers", default_value_t = 1)]
    workers: usize,
}

/// Parse a source spec: `[RELATION=]PROVIDER[:k=v;k=v...]`.
///
/// RELATION is optional — multi-output providers (e.g. treesitter) name their
/// own relations, so it is omitted for them. Config pairs are `;`-separated.
fn parse_source(spec: &str) -> Result<(Option<String>, String, HashMap<String, String>), String> {
    // Split provider/config on the first ':'; RELATION= (if any) is before it.
    let (left, cfg_str) = match spec.split_once(':') {
        Some((l, c)) => (l, c),
        None => (spec, ""),
    };
    let (relation, provider) = match left.split_once('=') {
        Some((r, p)) => (Some(r.to_string()), p.to_string()),
        None => (None, left.to_string()),
    };
    if provider.is_empty() {
        return Err(format!("invalid --source '{}': missing provider", spec));
    }
    let mut config = HashMap::new();
    if !cfg_str.is_empty() {
        for pair in cfg_str.split(';') {
            let (k, v) = pair
                .split_once('=')
                .ok_or_else(|| format!("invalid config pair '{}' in --source", pair))?;
            config.insert(k.to_string(), v.to_string());
        }
    }
    Ok((relation, provider, config))
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
