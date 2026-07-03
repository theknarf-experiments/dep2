//! End-to-end: the clock heartbeat + `date_epoch` builtin drive a time-based
//! rule over a CSV of deadlines — "due within a week of now".

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use dep2_core::engine::{Dep2, Dep2Config};
use dep2_plugin_clock::ClockPlugin;
use dep2_plugin_csv::CsvPlugin;

const PROG: &str = "\
.in
.decl deadline(name: string, due: string)
.decl now(iso: string, epoch: number)

.out
.decl soon(name: string, due: string)

.rule
soon(N, D) :- deadline(N, D), now(_, E),
    date_epoch(D) > E, date_epoch(D) < E + 604800.
";

fn rows(
    state: &Arc<std::sync::Mutex<dep2_core::engine::RelationState>>,
    rel: &str,
) -> Vec<Vec<i64>> {
    state
        .lock()
        .unwrap()
        .get(rel)
        .map(|m| m.keys().map(|r| r.to_vec()).collect())
        .unwrap_or_default()
}

fn wait_for<F: Fn() -> bool>(cond: F, secs: u64) -> bool {
    for _ in 0..(secs * 20) {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn deadlines_within_a_week_of_a_fixed_now() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("deadlines.csv");
    // Fixed clock at 2026-07-02: due-in-3-days is soon, September is not,
    // and yesterday is past (not "soon").
    std::fs::write(
        &csv,
        "name,due\n\
         ship,2026-07-05T12:00:00Z\n\
         conf,2026-09-01T00:00:00Z\n\
         gone,2026-07-01T00:00:00Z\n",
    )
    .unwrap();

    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
    });
    engine.add_plugin(Box::new(ClockPlugin));
    engine.add_plugin(Box::new(CsvPlugin));
    let mut clock_config = HashMap::new();
    clock_config.insert("fixed".to_string(), "2026-07-02T00:00:00Z".to_string());
    engine.add_source(None, "clock", clock_config);
    let mut csv_config = HashMap::new();
    csv_config.insert("path".to_string(), csv.to_string_lossy().into_owned());
    engine.add_source(Some("deadline".to_string()), "csv", csv_config);
    engine.load_program(PROG).unwrap();

    let state = engine.state();
    let types = engine.relation_types();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    assert!(
        wait_for(|| rows(&state, "soon").len() == 1, 15),
        "expected exactly one soon row, got {:?}",
        rows(&state, "soon")
    );
    let decoded = dep2_core::engine::decode_state_row(&rows(&state, "soon")[0], &types["soon"]);
    assert_eq!(decoded[0], "ship");

    // A new deadline lands inside the window: the CSV is watched, the rule
    // picks it up against the same fixed now.
    std::fs::write(
        &csv,
        "name,due\n\
         ship,2026-07-05T12:00:00Z\n\
         conf,2026-09-01T00:00:00Z\n\
         gone,2026-07-01T00:00:00Z\n\
         rush,2026-07-03T09:00:00Z\n",
    )
    .unwrap();
    assert!(
        wait_for(|| rows(&state, "soon").len() == 2, 15),
        "the new deadline should join `soon`, got {:?}",
        rows(&state, "soon")
    );

    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
}
