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

/// Budget for the wait-for-condition polls below, as ticks of `SETTLE_MS`.
///
/// These loops sleep on wall-clock, so what they bound is how long the machine
/// has been busy, not how much work the engine had to do. Tests that settle in
/// four seconds idle have been seen to blow a twenty-second budget under a full
/// parallel suite and fail for no reason at all. The budget is only ever spent
/// when a test is going to fail anyway, so there is no reason for it to be
/// tight.
const SETTLE_TICKS: usize = 4000;
const SETTLE_MS: u64 = 50;

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
    for _ in 0..SETTLE_TICKS {
        thread::sleep(Duration::from_millis(SETTLE_MS));
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
    for _ in 0..SETTLE_TICKS {
        thread::sleep(Duration::from_millis(SETTLE_MS));
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
    for _ in 0..SETTLE_TICKS {
        thread::sleep(Duration::from_millis(SETTLE_MS));
        got = rows_of(&qstate);
        if got == vec![vec![1, 3]] {
            break;
        }
    }
    assert_eq!(got, vec![vec![1, 3]], "query replays published history");

    // Live tracking: the watched CSV grows an edge; tc gains 3->4 paths and
    // the query must follow ((1,4) via 1->3->4 among others).
    std::fs::write(&csv, "x,y\n1,2\n2,3\n3,4\n").unwrap();
    for _ in 0..SETTLE_TICKS {
        thread::sleep(Duration::from_millis(SETTLE_MS));
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
    for _ in 0..SETTLE_TICKS {
        thread::sleep(Duration::from_millis(SETTLE_MS));
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
    for _ in 0..SETTLE_TICKS {
        thread::sleep(Duration::from_millis(SETTLE_MS));
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

/// Source-row push-down end to end: with publishing off, rows matching none
/// of the program's constant atom patterns are dropped at the parse pool.
/// The observable contract is that results are IDENTICAL to the published
/// (unfiltered) run — a dropped row could never have fired a rule.
#[test]
fn source_pushdown_preserves_results() {
    const PROG: &str = "\
.in
.decl e(x: number, y: number)

.printsize
.decl a(x: number)
.decl b(x: number)

.rule
a(Y) :- e(1, Y).
b(Y) :- e(2, Y).
";
    let run = |publish: bool| -> (Vec<Vec<i64>>, Vec<Vec<i64>>) {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("e.csv");
        std::fs::write(&csv, "x,y\n1,10\n2,20\n3,30\n4,40\n").unwrap();
        let mut engine = Dep2::with_config(Dep2Config {
            workers: 1,
            print_updates: false,
            publish,
        });
        engine.add_plugin(Box::new(CsvPlugin));
        let mut config = HashMap::new();
        config.insert("path".to_string(), csv.to_string_lossy().into_owned());
        engine.add_source(Some("e".to_string()), "csv", config);
        engine.load_program(PROG).unwrap();

        let state = engine.state();
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = Arc::clone(&shutdown);
        let handle = thread::spawn(move || engine.run(sd));
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for _ in 0..SETTLE_TICKS {
            thread::sleep(Duration::from_millis(SETTLE_MS));
            let st = state.lock().unwrap();
            let get = |name: &str| {
                st.get(name)
                    .map(|m| m.keys().map(|r| r.to_vec()).collect::<Vec<_>>())
                    .unwrap_or_default()
            };
            a = get("a");
            b = get("b");
            if !a.is_empty() && !b.is_empty() {
                break;
            }
        }
        shutdown.store(true, Ordering::Relaxed);
        handle.join().unwrap().unwrap();
        a.sort();
        b.sort();
        (a, b)
    };

    let filtered = run(false);
    let published = run(true);
    assert_eq!(filtered, published, "push-down must not change results");
    assert_eq!(filtered.0, vec![vec![10i64]]);
    assert_eq!(filtered.1, vec![vec![20i64]]);
}

/// Decl-level `order_by`/`limit` flow from the program into the engine's
/// relation shapes, ready for the serving layer.
#[test]
fn order_by_limit_decl_reaches_relation_shapes() {
    let mut engine = Dep2::new();
    engine
        .load_program(
            ".in\n.decl m(s: number, v: number)\n.out\n.decl top(s: number, v: number) order_by(v desc, s) limit(3)\n.rule\ntop(S, V) :- m(S, V).\n",
        )
        .unwrap();
    let shapes = engine.relation_shapes();
    let (order, limit) = shapes.get("top").expect("top must carry a shape");
    assert_eq!(order, &vec![(1, true), (0, false)]);
    assert_eq!(*limit, Some(3));
}

/// `.import` end to end: the edge decl + tc rules live in an imported
/// library file; the entry program adds its own rule over the imported
/// relations, and the engine derives across both.
#[test]
fn imported_file_merges_into_the_running_program() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("tc_lib.dl"),
        ".in\n.decl edge(x: number, y: number)\n.printsize\n.decl tc(x: number, y: number)\n.rule\ntc(X, Y) :- edge(X, Y).\ntc(X, Z) :- tc(X, Y), edge(Y, Z).\n",
    )
    .unwrap();
    let main = dir.path().join("main.dl");
    std::fs::write(
        &main,
        ".import \"tc_lib.dl\"\n.printsize\n.decl reaches_three(x: number)\n.rule\nreaches_three(X) :- tc(X, 3).\n",
    )
    .unwrap();
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
    engine.load_program_file(&main).unwrap();

    let state = engine.state();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    let mut reaches: Vec<Vec<i64>> = Vec::new();
    for _ in 0..SETTLE_TICKS {
        thread::sleep(Duration::from_millis(SETTLE_MS));
        if let Some(rows) = state.lock().unwrap().get("reaches_three") {
            if rows.len() >= 2 {
                reaches = rows.keys().map(|r| r.to_vec()).collect();
                break;
            }
        }
    }
    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();

    reaches.sort();
    assert_eq!(
        reaches,
        vec![vec![1], vec![2]],
        "rule over imported tc must derive"
    );
}

/// Recursive `min` label propagation (connected components) under RETRACTION.
///
/// Regression: differential dataflow can deliver a row's retraction before the
/// matching addition, so the materialized state saw `-leader(3,2)` while that
/// row was absent, dropped it, then applied the later `+leader(3,2)` — leaving
/// a phantom row alongside the real one. Two labels for one node: the served
/// relation silently stopped being a function of its key.
#[test]
fn retracting_an_equation_leaves_no_phantom_labels() {
    const CC: &str = "\
.in
.decl eq(x: number, y: number)
.decl item(t: number)

.printsize
.decl edge(x: number, y: number)

.out
.decl leader(t: number, rep: number) merge(min)

.rule
edge(X, Y) :- eq(X, Y).
edge(Y, X) :- eq(X, Y).
leader(T, T) :- item(T).
leader(X, L) :- edge(X, Y), leader(Y, L).
";
    let dir = tempfile::tempdir().unwrap();
    let items = dir.path().join("item.csv");
    let eqs = dir.path().join("eq.csv");
    std::fs::write(&items, "t\n1\n2\n3\n").unwrap();
    // 1-2 and 2-3 asserted: all three collapse to leader 1.
    std::fs::write(&eqs, "x,y\n1,2\n2,3\n").unwrap();

    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: false,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    for (rel, path) in [("item", &items), ("eq", &eqs)] {
        let mut config = HashMap::new();
        config.insert("path".to_string(), path.to_string_lossy().into_owned());
        engine.add_source(Some(rel.to_string()), "csv", config);
    }
    engine.load_program(CC).unwrap();

    let state = engine.state();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    let labels = || -> Vec<(i64, i64)> {
        let st = state.lock().unwrap();
        let mut out: Vec<(i64, i64)> = st
            .get("leader")
            .map(|m| {
                dep2_core::engine::live_rows(m)
                    .map(|r| (r[0], r[1]))
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    };
    let settle = |want: Vec<(i64, i64)>| -> Vec<(i64, i64)> {
        let mut got = Vec::new();
        for _ in 0..SETTLE_TICKS {
            thread::sleep(Duration::from_millis(SETTLE_MS));
            got = labels();
            if got == want {
                break;
            }
        }
        got
    };

    let all_one = vec![(1, 1), (2, 1), (3, 1)];
    assert_eq!(
        settle(all_one.clone()),
        all_one,
        "1-2-3 should share leader 1"
    );

    // Retract 2-3: {1,2} keeps leader 1, and 3 becomes its own class. The
    // phantom bug showed 3 with BOTH (3,2) and (3,3).
    std::fs::write(&eqs, "x,y\n1,2\n").unwrap();
    let split = vec![(1, 1), (2, 1), (3, 3)];
    let got = settle(split.clone());
    assert_eq!(got, split, "3 must have exactly one label after retraction");

    // Retract everything: three singleton classes.
    std::fs::write(&eqs, "x,y\n").unwrap();
    let singles = vec![(1, 1), (2, 2), (3, 3)];
    assert_eq!(settle(singles.clone()), singles, "all classes split");

    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
}

const REQUIRE_PROG: &str = "\
.require nosuchplugin
.in
.decl a(x: number)
.out
.decl b(x: number)
.rule
b(X) :- a(X).
";

const REQUIRE_OK_PROG: &str = "\
.require csv
.in
.decl a(x: number)
.out
.decl b(x: number)
.rule
b(X) :- a(X).
";

/// A missing plugin has to be reported as a missing plugin.
///
/// Before `.require` existed the failure surfaced only when a source was bound
/// to an absent provider, as a panic reading "no streaming provider registered
/// for 'x'" — which names no alternatives, suggests no fix, and reads like an
/// engine fault rather than a build that left the plugin out. That is exactly
/// the trap a feature-gated plugin sets.
#[test]
fn a_missing_required_plugin_is_reported_before_anything_is_wired_up() {
    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: false,
    });
    engine.add_plugin(Box::new(CsvPlugin));

    let err = engine.load_program(REQUIRE_PROG).unwrap_err();
    assert!(err.contains("`nosuchplugin`"), "{}", err);
    // The alternatives matter as much as the failure: a typo is only obvious
    // next to the list of real names.
    assert!(err.contains("available plugins: csv"), "{}", err);
}

#[test]
fn a_required_plugin_that_is_registered_loads_normally() {
    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: false,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    engine.load_program(REQUIRE_OK_PROG).unwrap();
}

/// A program that names its own input needs no `--source` at all.
#[test]
fn an_inline_source_binds_without_any_command_line_flag() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("in.csv");
    std::fs::write(&csv, "name,n\nb,21\n").unwrap();
    let prog = dir.path().join("p.dl");
    std::fs::write(
        &prog,
        format!(
            ".require csv\n.source t = csv(path = \"{}\", types = \"string,integer\")\n\
             .in\n.decl t(name: string, n: number)\n.out\n.decl d(name: string, n2: number)\n\
             .rule\nd(N, X * 2) :- t(N, X).\n",
            csv.display()
        ),
    )
    .unwrap();

    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: false,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    engine.load_program_file(&prog).unwrap();

    let state = engine.state();
    let types = engine.relation_types();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    let mut ok = false;
    for _ in 0..SETTLE_TICKS {
        thread::sleep(Duration::from_millis(SETTLE_MS));
        if count(&state, "d") == 1 {
            ok = true;
            break;
        }
    }
    assert!(ok, "inline source should feed the program");
    let rows: Vec<Vec<String>> = state
        .lock()
        .unwrap()
        .get("d")
        .map(|m| {
            m.keys()
                .map(|r| dep2_core::engine::decode_state_row(&r.to_vec(), &types["d"]))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(rows, vec![vec!["b".to_string(), "42".to_string()]]);

    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
}

/// `--source` has to win, or a program naming a default input could never be
/// pointed at anything else without editing it.
#[test]
fn a_command_line_source_overrides_the_inline_one() {
    let dir = tempfile::tempdir().unwrap();
    let inline_csv = dir.path().join("inline.csv");
    let override_csv = dir.path().join("override.csv");
    std::fs::write(&inline_csv, "name,n\nb,21\n").unwrap();
    std::fs::write(&override_csv, "name,n\nz,100\n").unwrap();
    let prog = dir.path().join("p.dl");
    std::fs::write(
        &prog,
        format!(
            ".require csv\n.source t = csv(path = \"{}\", types = \"string,integer\")\n\
             .in\n.decl t(name: string, n: number)\n.out\n.decl d(name: string, n2: number)\n\
             .rule\nd(N, X * 2) :- t(N, X).\n",
            inline_csv.display()
        ),
    )
    .unwrap();

    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: false,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    // Bound before the program loads, exactly as the CLI does it.
    let mut cfg = HashMap::new();
    cfg.insert("path".to_string(), override_csv.display().to_string());
    cfg.insert("types".to_string(), "string,integer".to_string());
    engine.add_source(Some("t".to_string()), "csv", cfg);
    engine.load_program_file(&prog).unwrap();

    let state = engine.state();
    let types = engine.relation_types();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    let mut ok = false;
    for _ in 0..SETTLE_TICKS {
        thread::sleep(Duration::from_millis(SETTLE_MS));
        if count(&state, "d") == 1 {
            ok = true;
            break;
        }
    }
    assert!(ok, "the overriding source should feed the program");
    let rows: Vec<Vec<String>> = state
        .lock()
        .unwrap()
        .get("d")
        .map(|m| {
            m.keys()
                .map(|r| dep2_core::engine::decode_state_row(&r.to_vec(), &types["d"]))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        rows,
        vec![vec!["z".to_string(), "200".to_string()]],
        "inline binding must not win over --source"
    );

    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
}

/// `merge(min)` as a recursive lattice of lower bounds.
///
/// Patent expiry is the worked example because it is the case that motivated
/// the feature and it makes the rules concrete, but nothing here is about
/// patents: it is a min-fold whose candidates are themselves defined by the
/// fold.
///
/// A patent expires at its granted term, or when its maintenance fees stopped,
/// or when a patent it disclaimed over expires — whichever is EARLIEST. Each is
/// one rule contributing a candidate, and the disclaimer rule is recursive, so
/// a chain resolves to a fixpoint with nothing walking it.
///
/// The two cases worth pinning are the ones a hand-written traversal gets
/// wrong: a transitive chain (C disclaims over B disclaims over A, so C is
/// bounded by A), and a disclaimer over something that expires LATER, which
/// must not extend anything.
const LATTICE_PROG: &str = "\
.in
.decl base(patent: string, epoch: number)
.decl lapsed_at(patent: string, epoch: number)
.decl disclaimer(patent: string, over_patent: string)

.out
.decl expiry(patent: string, epoch: number) merge(min)

.rule
expiry(P, E) :- base(P, E).
expiry(P, E) :- lapsed_at(P, E).
expiry(P, E) :- disclaimer(P, Q), expiry(Q, E).
";

#[test]
fn merge_min_folds_a_recursive_lattice_of_lower_bounds() {
    let dir = tempfile::tempdir().unwrap();
    // Days as epoch-ish integers; only the ordering matters.
    let base = dir.path().join("base.csv");
    std::fs::write(
        &base,
        "patent,epoch\nA,2030\nB,2032\nC,2034\nD,2035\nE,2033\nF,2031\n",
    )
    .unwrap();
    let lapsed = dir.path().join("lapsed.csv");
    std::fs::write(&lapsed, "patent,epoch\nE,2020\n").unwrap();
    let disc = dir.path().join("disc.csv");
    // B over A, C over B (transitive), F over D (a LATER patent).
    std::fs::write(&disc, "patent,over_patent\nB,A\nC,B\nF,D\n").unwrap();

    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: false,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    for (rel, path, types) in [
        ("base", &base, "string,integer"),
        ("lapsed_at", &lapsed, "string,integer"),
        ("disclaimer", &disc, "string,string"),
    ] {
        let mut cfg = HashMap::new();
        cfg.insert("path".to_string(), path.display().to_string());
        cfg.insert("types".to_string(), types.to_string());
        engine.add_source(Some(rel.to_string()), "csv", cfg);
    }
    engine.load_program(LATTICE_PROG).unwrap();

    let state = engine.state();
    let types = engine.relation_types();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    let read = || -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> = state
            .lock()
            .unwrap()
            .get("expiry")
            .map(|m| {
                m.keys()
                    .map(|r| dep2_core::engine::decode_state_row(&r.to_vec(), &types["expiry"]))
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    };
    // Waiting for six rows is not enough: the fold refines in place, so a
    // patent can hold its own term for an instant before a disclaimed
    // ancestor's earlier date arrives. Wait for the VALUES to settle.
    let want: Vec<Vec<String>> = [
        ("A", "2030"),
        ("B", "2030"),
        ("C", "2030"),
        ("D", "2035"),
        ("E", "2020"),
        ("F", "2031"),
    ]
    .iter()
    .map(|(p, e)| vec![p.to_string(), e.to_string()])
    .collect();
    let mut ok = false;
    for _ in 0..SETTLE_TICKS {
        thread::sleep(Duration::from_millis(SETTLE_MS));
        if read() == want {
            ok = true;
            break;
        }
    }
    assert!(ok, "lattice did not settle; got {:?}", read());

    let rows = read();
    let get = |p: &str| -> String {
        rows.iter()
            .find(|r| r[0] == p)
            .unwrap_or_else(|| panic!("no row for {}", p))[1]
            .clone()
    };
    assert_eq!(get("A"), "2030", "own term");
    assert_eq!(get("B"), "2030", "bounded by the patent it disclaimed over");
    assert_eq!(
        get("C"),
        "2030",
        "TRANSITIVELY bounded by A through B — the fixpoint, not one hop"
    );
    assert_eq!(get("D"), "2035", "own term, untouched");
    assert_eq!(
        get("E"),
        "2020",
        "lapsed thirteen years before its term ran"
    );
    assert_eq!(
        get("F"),
        "2031",
        "disclaiming over a LATER patent must not extend anything"
    );

    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
}

// ---------------------------------------------------------------------------
// Existential body atoms
// ---------------------------------------------------------------------------

/// A body atom whose columns reach neither the head nor a join.
///
/// Every rule here asks only whether some row EXISTS — `v(J, _), J > 0` cares
/// that a match was found, not what it was. The planner used to have no way to
/// represent the resulting zero-column collection and hit
/// `Transformation::kv_to_kv: null signatures`, a `panic!` on the timely worker
/// building the rule; the process died before producing anything. That made a
/// large, ordinary corner of Datalog unusable: existence tests, ground body
/// atoms, and constant-only heads all take this shape.
const EXISTS_PROG: &str = "\
.in
.decl u(x: number)
.decl v(a: number, b: number)

.out
.decl filtered(id: number)
.decl present(id: number)
.decl ground(id: number)
.decl absent(id: number)
.decl no_match(id: number)

.rule
// The atom binds J only to filter on it; nothing survives to the head.
filtered(I) :- u(I), v(J, _), J > 0.
// A ground atom: no variable at all, so nothing is even nameable.
present(I) :- u(I), v(1, _).
absent(I) :- u(I), v(0, _).
// A head of pure constants, so the body is asked for no columns either.
ground(0) :- u(_).
// Existence that genuinely fails, to show the test can tell empty from broken.
no_match(I) :- u(I), v(J, K), J > 100, K > 100.
";

#[test]
fn existential_body_atoms_are_planned_rather_than_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let u_csv = dir.path().join("u.csv");
    let v_csv = dir.path().join("v.csv");
    std::fs::write(&u_csv, "x\n1\n2\n3\n").unwrap();
    std::fs::write(&v_csv, "a,b\n1,2\n5,6\n").unwrap();

    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: true,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    for (rel, path) in [("u", &u_csv), ("v", &v_csv)] {
        let mut config = HashMap::new();
        config.insert("path".to_string(), path.to_string_lossy().into_owned());
        engine.add_source(Some(rel.to_string()), "csv", config);
    }
    engine.load_program(EXISTS_PROG).unwrap();

    let state = engine.state();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    // Settle on the relations that should have rows; the empty ones are then
    // asserted, since "still empty" is not something a poll can wait for.
    let mut settled = false;
    for _ in 0..SETTLE_TICKS {
        thread::sleep(Duration::from_millis(SETTLE_MS));
        if count(&state, "filtered") == 3
            && count(&state, "present") == 3
            && count(&state, "ground") == 1
        {
            settled = true;
            break;
        }
    }

    let rows = |rel: &str| -> Vec<Vec<i64>> {
        let mut out: Vec<Vec<i64>> = state
            .lock()
            .unwrap()
            .get(rel)
            .map(|m| m.keys().map(|r| r.to_vec()).collect())
            .unwrap_or_default();
        out.sort();
        out
    };
    let filtered = rows("filtered");
    let present = rows("present");
    let ground = rows("ground");
    let absent = rows("absent");
    let no_match = rows("no_match");

    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();

    assert!(
        settled,
        "existential rules did not settle: filtered={:?} present={:?} ground={:?}",
        filtered, present, ground
    );

    // `v` has a row with a positive first column, so every `u` qualifies.
    assert_eq!(filtered, vec![vec![1], vec![2], vec![3]]);
    // `v(1, _)` matches (1, 2), so again every `u` qualifies — the existence
    // test must not leak `v`'s own columns into the answer.
    assert_eq!(present, vec![vec![1], vec![2], vec![3]]);
    // A constant head over a body that merely has to be non-empty.
    assert_eq!(ground, vec![vec![0]]);
    // `v` has no row whose first column is 0.
    assert!(absent.is_empty(), "expected no rows, got {:?}", absent);
    // Nor any row past 100.
    assert!(no_match.is_empty(), "expected no rows, got {:?}", no_match);
}

/// Arithmetic overflow must not take the engine with it.
///
/// `X + i64::MAX` used the plain `+`, so a debug build panicked on the timely
/// worker evaluating the rule. The worker died, the remaining workers then
/// logged "epoch has not completed" forever, and the engine hung rather than
/// failing — a query that never returns instead of one that reports a problem.
/// In a release build the same expression wrapped silently and produced a
/// plausible wrong number.
///
/// Overflow now yields NULL, matching division by zero, and — this is what the
/// test is really for — the rest of the program keeps running.
const OVERFLOW_PROG: &str = "\
.in
.decl u(x: number)

.out
.decl big(v: number)
.decl fine(v: number)

.rule
big(X + 9223372036854775807) :- u(X).
fine(X + 10) :- u(X).
";

#[test]
fn arithmetic_overflow_yields_null_without_wedging_the_dataflow() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("u.csv");
    std::fs::write(&csv, "x\n1\n2\n3\n").unwrap();

    let mut engine = Dep2::with_config(Dep2Config {
        workers: 2,
        print_updates: false,
        publish: true,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    let mut config = HashMap::new();
    config.insert("path".to_string(), csv.to_string_lossy().into_owned());
    engine.add_source(Some("u".to_string()), "csv", config);
    engine.load_program(OVERFLOW_PROG).unwrap();

    let state = engine.state();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    // `fine` settling at all is the real assertion: it can only happen if the
    // worker that evaluated the overflowing rule is still alive to finish the
    // epoch. Before the fix this loop ran out its budget.
    let mut settled = false;
    for _ in 0..SETTLE_TICKS {
        thread::sleep(Duration::from_millis(SETTLE_MS));
        if count(&state, "fine") == 3 && count(&state, "big") >= 1 {
            settled = true;
            break;
        }
    }

    let mut fine: Vec<Vec<i64>> = state
        .lock()
        .unwrap()
        .get("fine")
        .map(|m| m.keys().map(|r| r.to_vec()).collect())
        .unwrap_or_default();
    fine.sort();
    let big: Vec<Vec<i64>> = state
        .lock()
        .unwrap()
        .get("big")
        .map(|m| m.keys().map(|r| r.to_vec()).collect())
        .unwrap_or_default();

    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();

    assert!(
        settled,
        "the dataflow did not settle after an overflow: fine={:?} big={:?}",
        fine, big
    );
    // The rule that does not overflow is completely unaffected.
    assert_eq!(fine, vec![vec![11], vec![12], vec![13]]);
    // Every overflowing row collapses to the one null value.
    assert_eq!(
        big,
        vec![vec![parsing::decl::NULL_SENTINEL]],
        "overflow must report null, not a wrapped number"
    );
}
