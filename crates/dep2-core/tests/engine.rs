//! End-to-end integration tests for the Dep2 engine: a real streaming source
//! (the CSV plugin — no wasmtime), through parse → strata → plan → execute →
//! output callback → live query state, plus the `.out`/served-relation logic.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use dep2_core::engine::{Dep2, Dep2Config};
use dep2_plugin::{
    ColumnDef, DataSchema, DataType, DataValue, Plugin, PluginContext, Source, StreamOutput,
    StreamingDataProvider, StreamingDataSource, ValueSink,
};
use dep2_plugin_csv::CsvPlugin;

// ---------------------------------------------------------------------------
// Synthetic streaming source for engine-level tests.
//
// Feeds `n` work units, one `item(id)` row per unit, pacing each `ingest` by a
// few ms and recording progress in a shared `fed` counter. That lets a test
// observe whether output streams out *before* all input is fed — the engine's
// incremental contract, which plain unit tests don't exercise and which has
// regressed before (coarse epoch sealing; multi-worker negation).
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Synthetic {
    n: usize,
    pace_ms: u64,
    fed: Arc<AtomicUsize>,
}

impl Plugin for Synthetic {
    fn name(&self) -> &str {
        "synthetic"
    }
    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(Plugin::name(self));
        ctx.register_streaming_data_provider(Box::new(self.clone()));
    }
}

impl StreamingDataProvider for Synthetic {
    fn name(&self) -> &str {
        "synthetic"
    }
    fn open_stream(
        &self,
        _config: &HashMap<String, String>,
    ) -> Result<Box<dyn StreamingDataSource>, String> {
        Ok(Box::new(self.clone()))
    }
}

impl StreamingDataSource for Synthetic {
    fn outputs(&self) -> Vec<StreamOutput> {
        vec![StreamOutput {
            relation: "item".to_string(),
            schema: DataSchema {
                columns: vec![ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Integer,
                }],
            },
        }]
    }
    fn seed_units(&self) -> Vec<String> {
        (0..self.n).map(|i| i.to_string()).collect()
    }
    fn open(&self) -> Box<dyn Source> {
        Box::new(self.clone())
    }
}

impl Source for Synthetic {
    fn ingest(&mut self, unit: &str, sink: &mut dyn ValueSink) {
        let id: i64 = unit.parse().unwrap();
        sink.push("item", &[DataValue::Integer(id)], 1);
        self.fed.fetch_add(1, Ordering::Relaxed);
        if self.pace_ms > 0 {
            thread::sleep(Duration::from_millis(self.pace_ms));
        }
    }
}

// A program with a join and a negation (mirrors import_graph's file_node, whose
// `!has_module` fallback is the rule that stopped streaming under multi-worker).
// pos(X) = every item except id 0.
const NEG_PROG: &str = "\
.in
.decl item(id: number)

.printsize
.decl zero(id: number)

.out
.decl pos(id: number)

.rule
zero(X) :- item(X), X = 0.
pos(X) :- item(X), !zero(X).
";

fn count(state: &Arc<std::sync::Mutex<dep2_core::engine::RelationState>>, rel: &str) -> usize {
    state.lock().unwrap().get(rel).map(|m| m.len()).unwrap_or(0)
}

/// Run the synthetic source + negation program with `workers` workers and report
/// `(saw_partial, final_pos)`: whether output appeared while the source was still
/// feeding (incremental streaming), and the settled `pos` count.
fn run_streaming(workers: usize) -> (bool, usize) {
    // Seal an epoch every 1ms so the streaming MECHANISM is exercised even with a
    // fast synthetic seed (with the 64ms default a sub-second seed seals only a few
    // epochs and output bunches at the end — real repos seed slowly enough to
    // stream under the default; here we make the cadence fine to test it directly).
    // A regression that stops streaming (coarse epochs; multi-worker recursion/
    // negation that only emits at the end) fails this.
    std::env::set_var("DEP2_EPOCH_MS", "1");

    // Many units (so the seed spans many epochs, like a real repo) paced by a small
    // per-unit sleep so feeding takes real wall-clock time. Pacing is safe and
    // realistic here: ingestion runs on the engine's PARSE POOL, not on the dataflow
    // worker, so a slow source models real parsing without starving dataflow
    // stepping. (Total feed time ~= n * pace_ms / parse_threads.)
    let n = 3000;
    let fed = Arc::new(AtomicUsize::new(0));
    let src = Synthetic {
        n,
        pace_ms: 1,
        fed: Arc::clone(&fed),
    };

    let mut engine = Dep2::with_config(Dep2Config {
        workers,
        print_updates: false,
        publish: true,
    });
    engine.add_plugin(Box::new(src));
    engine.add_source(None, "synthetic", HashMap::new());
    engine.load_program(NEG_PROG).unwrap();

    let state = engine.state();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    // Watch for output to appear before the source has fed all units.
    let mut saw_partial = false;
    for _ in 0..2000 {
        thread::sleep(Duration::from_millis(5));
        let f = fed.load(Ordering::Relaxed);
        let pos = count(&state, "pos");
        if pos > 0 && f < n {
            saw_partial = true;
            break;
        }
        if f >= n {
            break; // finished before we caught a partial — incremental is broken
        }
    }

    // Wait for completion + settle.
    let mut final_pos = 0;
    for _ in 0..1000 {
        thread::sleep(Duration::from_millis(10));
        if fed.load(Ordering::Relaxed) >= n {
            final_pos = count(&state, "pos");
            if final_pos == n - 1 {
                break;
            }
        }
    }
    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
    (saw_partial, final_pos)
}

/// 1 worker: output must stream live (appear while the source is still feeding),
/// and the final result must be correct. Catches no-streaming regressions (e.g.
/// coarse epoch sealing) that plain unit tests miss.
#[test]
fn single_worker_streams_and_is_correct() {
    let (saw_partial, final_pos) = run_streaming(1);
    assert!(
        saw_partial,
        "1 worker: output must stream incrementally, but `pos` was empty until the \
         source finished feeding"
    );
    assert_eq!(final_pos, 3000 - 1, "1 worker: every item except id 0");
}

/// Multiple workers must ALSO stream live (not just converge at the end) AND be
/// correct. The recursive/negated rule here is the one that regressed to
/// end-of-seed-only output under multi-worker; this guards the fix.
#[test]
fn multi_worker_streams_and_is_correct() {
    let (saw_partial, final_pos) = run_streaming(2);
    assert!(
        saw_partial,
        "2 workers: output must stream incrementally (not back-load to the end of \
         the seed), but `pos` was empty until the source finished feeding"
    );
    assert_eq!(final_pos, 3000 - 1, "2 workers: every item except id 0");
}

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
        publish: true,
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
    // State now stores raw encoded `i64` rows; the edges are integers, so the stored
    // ids are the integer values themselves.
    let mut tc: Vec<Vec<i64>> = Vec::new();
    for _ in 0..600 {
        thread::sleep(Duration::from_millis(50));
        if let Some(rows) = state.lock().unwrap().get("tc") {
            if rows.len() >= 3 {
                tc = rows.keys().map(|r| r.to_vec()).collect();
                break;
            }
        }
    }
    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();

    tc.sort();
    let expected: Vec<Vec<i64>> = vec![vec![1, 2], vec![1, 3], vec![2, 3]];
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

const FLOAT_PROG: &str = "\
.in
.decl sample(name: string, weight: float)

.out
.decl light(name: string, weight: float)
.decl mid(name: string, m: float)

.rule
light(N, U) :- sample(N, U), U < 1.5.
mid(N, (A + B) / 2.0) :- sample(N, A), sample(N, B), A < B.
";

/// Float literals and parenthesised expressions end to end: comparisons run in
/// float mode (0.5 < 1.5 as *numbers*, not bit patterns) and a parenthesised
/// head expression computes a float midpoint.
#[test]
fn float_literals_and_parens_through_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("sample.csv");
    std::fs::write(&csv, "name,weight\na,0.5\nb,2.25\nx,1.0\nx,3.0\n").unwrap();

    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: true,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    let mut config = HashMap::new();
    config.insert("path".to_string(), csv.to_string_lossy().into_owned());
    config.insert("types".to_string(), "string,float".to_string());
    engine.add_source(Some("sample".to_string()), "csv", config);
    engine.load_program(FLOAT_PROG).unwrap();

    let state = engine.state();
    let types = engine.relation_types();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    let decoded = |rel: &str| -> Vec<Vec<String>> {
        let mut rows: Vec<Vec<String>> = state
            .lock()
            .unwrap()
            .get(rel)
            .map(|m| {
                m.keys()
                    .map(|r| dep2_core::engine::decode_state_row(r, &types[rel]))
                    .collect()
            })
            .unwrap_or_default();
        rows.sort();
        rows
    };

    // a (0.5) and x (1.0) are light; b (2.25) is not. Bit-pattern comparison
    // would get this wrong (2.25's bits are a smaller i64 than 0.5's).
    let mut ok = false;
    for _ in 0..600 {
        thread::sleep(Duration::from_millis(50));
        if decoded("light").len() == 2 && decoded("mid").len() == 1 {
            ok = true;
            break;
        }
    }
    assert!(
        ok,
        "expected 2 light rows and 1 mid row, got light={:?} mid={:?}",
        decoded("light"),
        decoded("mid")
    );
    assert_eq!(
        decoded("light")[0],
        vec!["a".to_string(), "0.5".to_string()]
    );
    assert_eq!(decoded("light")[1], vec!["x".to_string(), "1".to_string()]);
    // (1.0 + 3.0) / 2.0 = 2 — parenthesised grouping, float division.
    assert_eq!(decoded("mid")[0], vec!["x".to_string(), "2".to_string()]);

    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
}

/// The wiring is by name only, so a source whose schema disagrees with the
/// `.decl` must be rejected at startup — not fed as silent garbage.
#[test]
fn source_schema_must_match_decl() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("sample.csv");
    std::fs::write(&csv, "name,weight\na,0.5\n").unwrap();

    // Declared arity 3, but the CSV has 2 columns.
    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: true,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    let mut config = HashMap::new();
    config.insert("path".to_string(), csv.to_string_lossy().into_owned());
    engine.add_source(Some("sample".to_string()), "csv", config.clone());
    engine
        .load_program(
            "\
.in
.decl sample(name: string, weight: float, extra: number)
.printsize
.decl n(c: number)
.rule
n(count(N)) :- sample(N, _, _).
",
        )
        .unwrap();
    let err = engine
        .run(Arc::new(AtomicBool::new(true)))
        .expect_err("arity mismatch must fail startup");
    assert!(err.contains("2 columns"), "unexpected error: {}", err);

    // Right arity, but the CSV infers weight as float while the decl says number.
    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: true,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    engine.add_source(Some("sample".to_string()), "csv", config);
    engine
        .load_program(
            "\
.in
.decl sample(name: string, weight: number)
.printsize
.decl n(c: number)
.rule
n(count(N)) :- sample(N, _).
",
        )
        .unwrap();
    let err = engine
        .run(Arc::new(AtomicBool::new(true)))
        .expect_err("column type mismatch must fail startup");
    assert!(
        err.contains("sample.weight as float"),
        "unexpected error: {}",
        err
    );
}

/// Add a query to a RUNNING engine: it must replay the published history,
/// track live updates (the watched CSV grows), reject invalid programs with a
/// real error, and stop tracking once removed.
#[test]
fn live_query_over_running_engine() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("edge.csv");
    std::fs::write(&csv, "x,y\n1,2\n2,3\n").unwrap();

    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: true,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    let mut config = HashMap::new();
    config.insert("path".to_string(), csv.to_string_lossy().into_owned());
    engine.add_source(Some("edge".to_string()), "csv", config);
    engine.load_program(TC_PROG).unwrap();

    let live = engine.live_queries().expect("program loaded");
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    // Let the base ingest the seed (tc reaches its closure).
    thread::sleep(Duration::from_millis(600));

    // A bad query errors synchronously and never reaches the workers.
    let err = live
        .add(
            "bad",
            ".in\n.decl nope(x: number)\n.printsize\n.decl q(x: number)\n.rule\nq(X) :- nope(X).\n",
        )
        .unwrap_err();
    assert!(err.contains("not published"), "got: {err}");

    // A real query over the published IDB `tc`, added at runtime.
    live.add(
        "hops",
        ".in\n.decl tc(x: number, y: number)\n.printsize\n.decl two_hop(x: number, y: number)\n.rule\ntwo_hop(X, Y) :- tc(X, Z), tc(Z, Y).\n",
    )
    .unwrap();
    assert_eq!(live.ids(), vec!["hops".to_string()]);

    let (qstate, _types) = live.state("hops").expect("state registered");
    let rows_of = |st: &Arc<Mutex<dep2_core::engine::RelationState>>| -> Vec<Vec<i64>> {
        let mut rows: Vec<Vec<i64>> = st
            .lock()
            .unwrap()
            .get("two_hop")
            .map(|m| m.keys().map(|r| r.to_vec()).collect())
            .unwrap_or_default();
        rows.sort();
        rows
    };

    // Replayed history: tc = {12,23,13} -> two_hop = {(1,3)}.
    let mut got = Vec::new();
    for _ in 0..600 {
        thread::sleep(Duration::from_millis(50));
        got = rows_of(&qstate);
        if got == vec![vec![1, 3]] {
            break;
        }
    }
    assert_eq!(got, vec![vec![1, 3]], "query replays published history");

    // Live tracking: the watched CSV grows an edge; tc gains 3->4 paths and
    // the query must follow ((1,4) via 1->3->4 among others).
    std::fs::write(&csv, "x,y\n1,2\n2,3\n3,4\n").unwrap();
    for _ in 0..600 {
        thread::sleep(Duration::from_millis(50));
        got = rows_of(&qstate);
        if got.len() == 3 {
            break;
        }
    }
    assert_eq!(
        got,
        vec![vec![1, 3], vec![1, 4], vec![2, 4]],
        "query tracks live base updates"
    );

    // Removal: the query is gone from the handle and stops updating.
    assert!(live.remove("hops"));
    assert!(live.ids().is_empty());
    assert!(!live.remove("hops"), "second remove is a no-op");

    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
}

/// LiveQueries validation runs control-side, before anything reaches the
/// workers — none of these need the engine running.
#[test]
fn live_query_validation_rejects_bad_programs() {
    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: true,
    });
    engine.load_program(TC_PROG).unwrap();
    let live = engine.live_queries().unwrap();

    // Parse/typing errors come back as the front-end's rendered report.
    let err = live.add("p", ".in\n.decl tc(x: number\n").unwrap_err();
    assert!(!err.is_empty(), "parse error must carry a report");

    // Column-type mismatch against the published schema.
    let err = live
        .add(
            "t",
            ".in\n.decl tc(x: string, y: string)\n.printsize\n.decl q(x: string)\n.rule\nq(X) :- tc(X, _).\n",
        )
        .unwrap_err();
    assert!(
        err.contains("does not match the published schema"),
        "got: {err}"
    );

    // Row-mode mismatch: a query wide enough to need fat mode cannot run
    // against a thin base.
    let err = live
        .add(
            "w",
            ".in\n.decl tc(x: number, y: number)\n.printsize\n.decl wide(a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number)\n.rule\nwide(X, Y, X, Y, X, Y, X, Y, X) :- tc(X, Y).\n",
        )
        .unwrap_err();
    assert!(err.contains("row mode"), "got: {err}");

    // Duplicate id.
    live.add(
        "ok",
        ".in\n.decl tc(x: number, y: number)\n.printsize\n.decl q(x: number)\n.rule\nq(X) :- tc(X, _).\n",
    )
    .unwrap();
    let err = live
        .add(
            "ok",
            ".in\n.decl tc(x: number, y: number)\n.printsize\n.decl q(x: number)\n.rule\nq(X) :- tc(X, _).\n",
        )
        .unwrap_err();
    assert!(err.contains("already exists"), "got: {err}");

    // Removing an unknown id is a clean no-op.
    assert!(!live.remove("never-added"));
}

/// String and float columns through a runtime query, including a string
/// LITERAL in the query's rules — the literal must be interned on the
/// LiveQueries path exactly like base-program literals are.
#[test]
fn live_query_with_string_and_float_columns() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("item.csv");
    std::fs::write(&csv, "name,price\nfoo,1.5\nbar,2.5\nfog,3.5\n").unwrap();

    const PROG: &str = "\
.in
.decl item(name: string, price: float)

.printsize
.decl expensive(name: string)

.rule
expensive(N) :- item(N, P), P > 2.0.
";

    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: true,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    let mut config = HashMap::new();
    config.insert("path".to_string(), csv.to_string_lossy().into_owned());
    config.insert("types".to_string(), "string,float".to_string());
    engine.add_source(Some("item".to_string()), "csv", config);
    engine.load_program(PROG).unwrap();

    let live = engine.live_queries().unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));
    thread::sleep(Duration::from_millis(600));

    live.add(
        "q",
        ".in\n.decl item(name: string, price: float)\n.printsize\n.decl hit(name: string)\n.rule\nhit(N) :- item(N, _), starts_with(N, \"fo\") = 1.\n",
    )
    .unwrap();

    let (qstate, _types) = live.state("q").unwrap();
    let mut expected: Vec<Vec<i64>> =
        vec![vec![reading::intern("foo")], vec![reading::intern("fog")]];
    expected.sort();
    let mut got: Vec<Vec<i64>> = Vec::new();
    for _ in 0..600 {
        thread::sleep(Duration::from_millis(50));
        got = qstate
            .lock()
            .unwrap()
            .get("hit")
            .map(|m| m.keys().map(|r| r.to_vec()).collect())
            .unwrap_or_default();
        got.sort();
        if got == expected {
            break;
        }
    }
    assert_eq!(got, expected, "string-literal filter over string column");

    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
}

/// `publish: false` opts out of the runtime-query surface entirely: no
/// LiveQueries handle exists (the HTTP query routes report unavailable), no
/// published arrangements are maintained, and streaming output is unaffected.
#[test]
fn publish_opt_out_disables_live_queries_but_streams() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("edge.csv");
    std::fs::write(&csv, "x,y\n1,2\n2,3\n").unwrap();

    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: false,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    let mut config = HashMap::new();
    config.insert("path".to_string(), csv.to_string_lossy().into_owned());
    engine.add_source(Some("edge".to_string()), "csv", config);
    engine.load_program(TC_PROG).unwrap();

    assert!(
        engine.live_queries().is_none(),
        "publish: false must leave no live-query handle"
    );

    let state = engine.state();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    let mut tc: Vec<Vec<i64>> = Vec::new();
    for _ in 0..600 {
        thread::sleep(Duration::from_millis(50));
        if let Some(rows) = state.lock().unwrap().get("tc") {
            if rows.len() >= 3 {
                tc = rows.keys().map(|r| r.to_vec()).collect();
                break;
            }
        }
    }
    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();

    tc.sort();
    let expected: Vec<Vec<i64>> = vec![vec![1, 2], vec![1, 3], vec![2, 3]];
    assert_eq!(
        tc, expected,
        "streaming output must be unaffected by opt-out"
    );
}
