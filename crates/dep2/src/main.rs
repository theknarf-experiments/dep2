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
    /// Parse, type-check and plan programs without running them.
    Check(CheckArgs),
}

#[derive(Args, Debug)]
struct CheckArgs {
    /// Native FlowLog `.dl` programs to check (repeatable).
    programs: Vec<PathBuf>,

    /// Print each program that passes, not only the failures.
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,
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

    /// Number of FlowLog (Datalog) worker threads (0 = auto: one per CPU core).
    /// Parsing runs on a separate parse pool regardless of this; the workers run
    /// the Datalog compute (pulling from the shared parse queue and exchanging
    /// downstream). The dataflow compute is the usual bottleneck, and it scales:
    /// a 28k-file import graph loads 2.4x faster at 4 workers than at 1, still
    /// streaming incrementally. Drop to 1 to minimize memory, or when running
    /// several engines on one machine.
    #[arg(short = 'w', long = "workers", default_value_t = 4)]
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

    /// Do not publish relations for runtime queries. By default every EDB and
    /// served IDB keeps a whole-row arrangement per worker so queries can be
    /// added while the engine runs — memory proportional to those relations'
    /// sizes, paid even if no query is ever added. With this flag the
    /// arrangements are skipped entirely and the /query routes report the
    /// feature unavailable.
    #[arg(long = "no-publish")]
    no_publish: bool,
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
        Cmd::Check(args) => check(args),
    }
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// Load each program far enough to prove it would run, and report which do not.
///
/// Everything that can reject a program — parsing, the decl-driven typing pass,
/// rule safety, stratification and planning — happens in `load_program_file`,
/// before a single source is bound or a worker started. So the whole front end
/// can be exercised without network access, fixtures or an engine loop, which
/// is what makes checking a directory of programs cheap enough to do routinely.
///
/// Sources are deliberately NOT bound. A program's `.in` declarations are what
/// the typing pass checks against, and leaving sources out means a program is
/// validated on its own terms rather than against whatever happens to be
/// reachable. The one class of error this therefore cannot catch is a source
/// whose schema disagrees with the decl, which is reported at startup by `run`.
fn check(args: CheckArgs) {
    if args.programs.is_empty() {
        eprintln!("check: no programs given");
        std::process::exit(2);
    }
    let mut failed = Vec::new();
    for path in &args.programs {
        // A fresh engine per program: loading mutates the catalog, and a stale
        // one would let a later program pass on an earlier program's decls.
        let mut engine = Dep2::with_config(Dep2Config {
            workers: 1,
            print_updates: false,
            publish: false,
        });
        add_plugins(&mut engine);
        match engine.load_program_file(path) {
            Ok(()) => {
                if args.verbose {
                    println!("ok    {}", path.display());
                }
            }
            Err(e) => {
                // The labelled report is already on stderr.
                eprintln!("FAIL  {}: {}", path.display(), e);
                failed.push(path.clone());
            }
        }
    }
    let n = args.programs.len();
    if failed.is_empty() {
        println!("{} program{} ok", n, if n == 1 { "" } else { "s" });
    } else {
        eprintln!("{} of {} failed", failed.len(), n);
        std::process::exit(1);
    }
}

/// Register every built-in plugin. Shared by `run` and `check` so the set a
/// program is validated against cannot drift from the set it will run against.
fn add_plugins(engine: &mut Dep2) {
    engine.add_plugin(Box::new(dep2_plugin_csv::CsvPlugin));
    engine.add_plugin(Box::new(dep2_plugin_fs::FsPlugin));
    engine.add_plugin(Box::new(dep2_plugin_treesitter::TreeSitterPlugin));
    engine.add_plugin(Box::new(dep2_plugin_clock::ClockPlugin));
    engine.add_plugin(Box::new(dep2_plugin_git::GitPlugin));
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
        publish: !args.no_publish,
    });

    add_plugins(&mut engine);

    for spec in &args.sources {
        let (relation, provider, config) = parse_source(spec).unwrap_or_else(|e| panic!("{}", e));
        engine.add_source(relation, provider, config);
    }

    let program_src = std::fs::read_to_string(&args.program)
        .unwrap_or_else(|e| panic!("can't read {}: {}", args.program.display(), e));
    // Load by PATH so `.import "other.dl"` statements resolve relative to the
    // program file; program_src stays the entry file's text for /program.
    if let Err(e) = engine.load_program_file(&args.program) {
        // Parse/typing reports were already rendered to stderr.
        eprintln!("{}", e);
        std::process::exit(1);
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_handler = Arc::clone(&shutdown);
    ctrlc::set_handler(move || {
        eprintln!("\nShutting down...");
        shutdown_handler.store(true, Ordering::Relaxed);
    })
    .expect("failed to set Ctrl-C handler");

    if serve {
        let state = engine.state();
        let types = engine.relation_types();
        let shapes = engine.relation_shapes();
        let columns = engine.relation_columns();
        let viz = engine.viz_spec();
        let live = engine.live_queries();
        let unserved = Arc::new(engine.unserved_relations());
        let sources = engine.program_sources();
        let program = Arc::new(server::ProgramSource {
            path: args.program.display().to_string(),
            roots: engine.source_roots(),
            // Every loaded file (entry + `.import` closure), so the Rules
            // view can list and show them individually.
            files: if sources.is_empty() {
                vec![(args.program.display().to_string(), program_src.clone())]
            } else {
                sources.as_ref().clone()
            },
        });
        let addr = args.addr.clone();
        let server_shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            if let Err(e) = server::serve(
                &addr,
                state,
                types,
                shapes,
                columns,
                viz,
                unserved,
                program,
                live,
                server_shutdown,
            ) {
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

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn no_publish_flag_parses_and_defaults_on() {
        let cli = Cli::try_parse_from(["dep2", "run", "p.dl", "--no-publish"]).unwrap();
        let Cmd::Run(args) = cli.cmd else {
            panic!("expected run")
        };
        assert!(args.no_publish);

        let cli = Cli::try_parse_from(["dep2", "run", "p.dl"]).unwrap();
        let Cmd::Run(args) = cli.cmd else {
            panic!("expected run")
        };
        assert!(!args.no_publish, "publishing must stay on by default");
        assert_eq!(args.workers, 4, "multi-worker dataflow is the default");
    }

    #[test]
    fn check_takes_many_programs_so_a_whole_directory_can_be_validated() {
        let cli = Cli::try_parse_from(["dep2", "check", "a.dl", "b.dl", "-v"]).unwrap();
        let Cmd::Check(args) = cli.cmd else {
            panic!("expected check")
        };
        assert_eq!(args.programs.len(), 2);
        assert!(args.verbose);

        // Quiet by default: the point of checking a directory is that a clean
        // run says almost nothing.
        let cli = Cli::try_parse_from(["dep2", "check", "a.dl"]).unwrap();
        let Cmd::Check(args) = cli.cmd else {
            panic!("expected check")
        };
        assert!(!args.verbose);
    }
}
