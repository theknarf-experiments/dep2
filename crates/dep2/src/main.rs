use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use mimalloc::MiMalloc;
use tracing_subscriber::EnvFilter;

use dep2_core::engine::{Dep2, Dep2Config};

mod server;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const DEFAULT_ADDR: &str = "127.0.0.1:7878";

/// Live semantic analysis over a FlowLog Datalog program.
#[derive(Parser, Debug)]
#[command(
    name = "dep2",
    version,
    about = "Live semantic analysis over FlowLog Datalog"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run a program, stream sources into it, and serve the query API.
    Run(RunArgs),
    /// Query the current state of a running engine.
    Query(QueryArgs),
}

#[derive(Args, Debug)]
struct RunArgs {
    /// Native FlowLog `.dl` program to run.
    program: PathBuf,

    /// Bind a streaming source: `[RELATION=]PROVIDER[:k=v;k=v...]` (repeatable).
    /// RELATION is omitted for multi-output providers (e.g. treesitter, which
    /// feeds ast_node + ast_span). Config pairs are `;`-separated so values may
    /// contain commas.
    #[arg(short = 's', long = "source")]
    sources: Vec<String>,

    /// Number of FlowLog worker threads (0 = auto: one per CPU core).
    #[arg(short = 'w', long = "workers", default_value_t = 0)]
    workers: usize,

    /// Address to serve the query API on.
    #[arg(long = "addr", default_value = DEFAULT_ADDR)]
    addr: String,

    /// Do not serve the query API (just stream and print).
    #[arg(long = "no-serve")]
    no_serve: bool,

    /// Also print each `+`/`-` update to stdout (default off when serving).
    #[arg(long = "print")]
    print: bool,
}

#[derive(Args, Debug)]
struct QueryArgs {
    /// Relation to dump. Omit to list all output relations.
    relation: Option<String>,

    /// Address of the running engine's query API.
    #[arg(long = "addr", default_value = DEFAULT_ADDR)]
    addr: String,

    /// Print the raw JSON response.
    #[arg(long = "json")]
    json: bool,
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run(args) => run(args),
        Cmd::Query(args) => query(args),
    }
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

fn run(args: RunArgs) {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let serve = !args.no_serve;
    let workers = if args.workers == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    } else {
        args.workers
    };
    let mut engine = Dep2::with_config(Dep2Config {
        workers,
        // When serving, stay quiet by default (query the API instead).
        print_updates: args.print || args.no_serve,
    });

    engine.add_plugin(Box::new(dep2_plugin_csv::CsvPlugin));
    engine.add_plugin(Box::new(dep2_plugin_fs::FsPlugin));
    engine.add_plugin(Box::new(dep2_plugin_treesitter::TreeSitterPlugin));

    for spec in &args.sources {
        let (relation, provider, config) = parse_source(spec).unwrap_or_else(|e| panic!("{}", e));
        engine.add_source(relation, provider, config);
    }

    let program_src = std::fs::read_to_string(&args.program)
        .unwrap_or_else(|e| panic!("can't read {}: {}", args.program.display(), e));
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

    if serve {
        let state = engine.state();
        let unserved = Arc::new(engine.unserved_relations());
        let program = Arc::new(server::ProgramSource {
            path: args.program.display().to_string(),
            source: program_src.clone(),
        });
        let addr = args.addr.clone();
        let server_shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            if let Err(e) = server::serve(&addr, state, unserved, program, server_shutdown) {
                eprintln!("query API failed to start on {}: {}", addr, e);
            }
        });
        eprintln!("query API: http://{}/relations", args.addr);
    }

    engine.run(shutdown).unwrap_or_else(|e| panic!("{}", e));
}

/// Parse a source spec: `[RELATION=]PROVIDER[:k=v;k=v...]`.
fn parse_source(spec: &str) -> Result<(Option<String>, String, HashMap<String, String>), String> {
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

// ---------------------------------------------------------------------------
// query
// ---------------------------------------------------------------------------

fn query(args: QueryArgs) {
    let path = match &args.relation {
        Some(rel) => format!("/relations/{}", rel),
        None => "/relations".to_string(),
    };

    let (status, body) = match http_get(&args.addr, &path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    if args.json {
        println!("{}", body);
        std::process::exit(if status == 200 { 0 } else { 1 });
    }

    let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_else(|e| {
        eprintln!("bad response from {}: {} ({})", args.addr, e, body);
        std::process::exit(1);
    });

    if status != 200 {
        let msg = value
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("query failed");
        eprintln!("{}", msg);
        std::process::exit(1);
    }

    match &args.relation {
        // Dump one relation's rows.
        Some(_) => {
            let rows = value.get("rows").and_then(|v| v.as_array());
            match rows {
                Some(rows) => {
                    for row in rows {
                        let cols: Vec<String> = row
                            .as_array()
                            .map(|a| a.iter().map(json_cell).collect())
                            .unwrap_or_default();
                        println!("{}", cols.join(", "));
                    }
                    eprintln!("({} rows)", rows.len());
                }
                None => eprintln!("unexpected response: {}", body),
            }
        }
        // List relations.
        None => {
            if let Some(rels) = value.get("relations").and_then(|v| v.as_array()) {
                for rel in rels {
                    let name = rel.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let count = rel.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                    println!("{}\t{}", count, name);
                }
            } else {
                eprintln!("unexpected response: {}", body);
            }
        }
    }
}

/// Render a JSON cell as a plain string (string values unquoted).
fn json_cell(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Minimal HTTP GET against the local query API. Returns (status, body).
fn http_get(addr: &str, path: &str) -> Result<(u16, String), String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| {
        format!(
            "can't connect to {} ({}). Is `dep2 run` running with the query API?",
            addr, e
        )
    })?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, addr
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or("malformed HTTP response")?;
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or("missing HTTP status")?;
    Ok((status, body.to_string()))
}
