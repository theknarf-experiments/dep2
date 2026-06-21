//! End-to-end integration tests for the Dep2 engine: a real streaming source
//! (the CSV plugin — no wasmtime), through parse → strata → plan → execute →
//! output callback → live query state, plus the `.out`/served-relation logic.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use dep2_core::engine::{Dep2, Dep2Config};
use dep2_plugin_csv::CsvPlugin;

const TC_PROG: &str = "\
.in
.decl edge(x: number, y: number)

.printsize
.decl tc(x: number, y: number)

.rule
tc(X, Y) :- edge(X, Y).
tc(X, Y) :- tc(X, Z), edge(Z, Y).
";

/// Stream edges from a CSV through a recursive transitive-closure program and
/// read the materialized result off the live query state.
#[test]
fn csv_source_transitive_closure() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("edge.csv");
    std::fs::write(&csv, "x,y\n1,2\n2,3\n").unwrap();

    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    let mut config = HashMap::new();
    config.insert("path".to_string(), csv.to_string_lossy().into_owned());
    engine.add_source(Some("edge".to_string()), "csv", config);
    engine.load_program(TC_PROG).unwrap();

    let state = engine.state();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    // Poll the live state until the closure settles (1->2, 2->3, 1->3) or time out.
    let mut tc: Vec<Vec<String>> = Vec::new();
    for _ in 0..200 {
        thread::sleep(Duration::from_millis(50));
        if let Some(rows) = state.lock().unwrap().get("tc") {
            if rows.len() >= 3 {
                tc = rows.keys().cloned().collect();
                break;
            }
        }
    }
    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();

    tc.sort();
    let expected: Vec<Vec<String>> = [["1", "2"], ["1", "3"], ["2", "3"]]
        .iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect();
    assert_eq!(tc, expected, "transitive closure over the CSV edges");
}

const SERVE_PROG: &str = "\
.in
.decl e(x: number)

.printsize
.decl mid(x: number)

.out
.decl forced(x: number)

.printsize
.decl top(x: number)

.rule
mid(X) :- e(X).
forced(X) :- e(X).
top(X) :- mid(X), forced(X).
";

/// `.out` force-serves a consumed relation; a `.printsize` consumed relation is
/// reported as unserved (with its consumer) so the query API can explain it.
#[test]
fn out_section_controls_served_relations() {
    let mut engine = Dep2::new();
    engine.load_program(SERVE_PROG).unwrap();

    let unserved = engine.unserved_relations();

    // mid: .printsize and consumed by `top` -> unserved, attributed to `top`.
    assert_eq!(
        unserved.get("mid").map(|v| v.as_slice()),
        Some(&["top".to_string()][..]),
        "mid should be unserved and attributed to its consumer"
    );
    // forced: .out -> served even though consumed by `top`.
    assert!(
        !unserved.contains_key("forced"),
        "`.out` relation must be served (not reported unserved)"
    );
    // top: terminal -> served.
    assert!(!unserved.contains_key("top"), "terminal relation is served");
}
