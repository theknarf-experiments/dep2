//! A panicking timely worker must fail the run, not hang it.
//!
//! Every dataflow operator is sharded across workers, and an epoch completes
//! only when all of them report progress. So a worker that dies does not fail
//! the query — it stops answering, and the survivors spin in
//! `while probe.less_than(&epoch)` waiting for a progress message that will
//! never arrive. The engine goes quiet forever, which is the worst possible way
//! to report a bug: a hang is indistinguishable from a slow query, so it gets
//! diagnosed as a performance problem rather than as a crash.
//!
//! This was not hypothetical. Integer overflow used to panic a worker, and the
//! observed symptom was not "overflow" but three workers logging "epoch has not
//! completed after 8192 steps" until someone killed the process.
//!
//! WHY A FAULT-INJECTION HOOK. With the arithmetic fixed, no program is known to
//! panic a worker on purpose, so there is nothing to point this test at. The
//! remaining panic sites are internal contract assertions between the planner
//! and the executor — the sort that fire only when another bug already exists,
//! which is exactly when this recovery path has to work. Untested recovery code
//! is not recovery code, so `DEP2_TEST_PANIC_WORKER` kills a chosen worker on
//! demand.
//!
//! WHAT IS NOT COVERED, AND WHY THAT IS DEFENSIBLE. One case still hangs: a
//! strict subset of a multi-worker run dying *during dataflow assembly*, while
//! the workers are still allocating shared channels. A survivor gets past its
//! own assembly and out of the epoch loop, then blocks in timely's teardown,
//! which has no way to complete a barrier a vanished peer will never reach.
//! Recovering from that means supervising workers as processes rather than
//! threads, which is a different engine.
//!
//! It is also close to unreachable. Assembly-time panics come from plan shapes
//! the executor cannot build, and every worker assembles the SAME plan — so
//! such a bug takes all the workers down together, which is the `*:early` case
//! below, and that one is handled.
//!
//! This test lives in its own binary because it sets a process-wide environment
//! variable; a separate binary is a separate process even under plain
//! `cargo test`, so it cannot leak into another engine test running alongside.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use dep2_core::engine::{Dep2, Dep2Config};
use dep2_plugin_csv::CsvPlugin;

const PROG: &str = "\
.in
.decl item(id: number)

.out
.decl seen(id: number)

.rule
seen(X) :- item(X).
";

/// The tests in this binary share one environment variable, so they run one at
/// a time rather than relying on the harness's thread count.
static SERIAL: Mutex<()> = Mutex::new(());

/// Run to completion with workers killed per `spec`, returning the run's result.
///
/// `None` means the run never finished, which is the failure this file exists
/// to catch.
fn run_with_panicking_worker(workers: usize, spec: &str) -> Option<Result<(), String>> {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("item.csv");
    std::fs::write(&csv, "id\n1\n2\n3\n").unwrap();

    std::env::set_var("DEP2_TEST_PANIC_WORKER", spec);

    let mut engine = Dep2::with_config(Dep2Config {
        workers,
        print_updates: false,
        publish: true,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    let mut config = HashMap::new();
    config.insert("path".to_string(), csv.to_string_lossy().into_owned());
    engine.add_source(Some("item".to_string()), "csv", config);
    engine.load_program(PROG).unwrap();

    // Nothing ever signals shutdown: the run has to end on its own, which is
    // the whole point. Before containment it ran until the harness killed it.
    let shutdown = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let handle = thread::spawn(move || engine.run(shutdown));

    // Generous, because it is only ever spent on the failing path.
    let deadline = Duration::from_secs(60);
    while !handle.is_finished() && started.elapsed() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    std::env::remove_var("DEP2_TEST_PANIC_WORKER");

    if !handle.is_finished() {
        // The thread is wedged for good; leaking it is the only option, and the
        // caller is about to fail the test anyway.
        return None;
    }
    Some(
        handle
            .join()
            .expect("the driver thread itself must survive"),
    )
}

fn assert_reports_a_worker_panic(outcome: Option<Result<(), String>>, case: &str) {
    let result = outcome.unwrap_or_else(|| {
        panic!("{case}: the run never returned — a dead worker wedged the engine")
    });
    let error = match result {
        Err(e) => e,
        Ok(()) => panic!("{case}: a panicking worker must be reported as an error"),
    };
    assert!(
        error.contains("worker") && error.contains("panicked"),
        "{case}: the error should say what happened, got {error:?}"
    );
    assert!(
        error.contains("DEP2_TEST_PANIC_WORKER"),
        "{case}: the panic's own message should reach the caller, got {error:?}"
    );
}

/// The steady-state case: a worker dies while evaluating rows, which is where a
/// data-dependent panic (the overflow bug) actually fired.
#[test]
fn a_worker_dying_mid_run_fails_the_run() {
    let outcome = run_with_panicking_worker(2, "0");
    assert_reports_a_worker_panic(outcome, "one of two workers, after assembly");
}

/// The construction case: a plan the executor cannot build takes every worker
/// down at once, because they all assemble the same dataflow.
#[test]
fn every_worker_dying_during_assembly_fails_the_run() {
    let outcome = run_with_panicking_worker(2, "*:early");
    assert_reports_a_worker_panic(outcome, "all workers, during assembly");
}

/// The single-worker case, which has no peer to strand and so must be clean.
#[test]
fn a_single_worker_dying_fails_the_run() {
    let outcome = run_with_panicking_worker(1, "0:early");
    assert_reports_a_worker_panic(outcome, "the only worker");
}
