//! End-to-end integration tests for the Dep2 engine: a real streaming source
//! (the CSV plugin — no wasmtime), through parse → strata → plan → execute →
//! output callback → live query state, plus the `.out`/served-relation logic.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
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
    for _ in 0..200 {
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
