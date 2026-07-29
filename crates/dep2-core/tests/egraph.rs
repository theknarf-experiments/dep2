//! Equality saturation on the factored e-graph, driven through the real engine.
//!
//! These use CSV sources rather than the `executing` batch harness because the
//! programs carry string DATA: term ids are structural strings, which the
//! `.facts` path cannot round-trip (it re-interns each token).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use dep2_core::engine::{Dep2, Dep2Config};
use dep2_plugin_csv::CsvPlugin;

/// Budget for the wait-for-condition polls below, as ticks of `SETTLE_MS`.
/// Wall-clock, so what it bounds is how busy the machine is rather than how
/// much work the engine had; a saturated box starves an otherwise-fast test
/// into asserting on an empty result. Only ever spent by a failing test.
///
/// Sixty seconds was not enough: twelve seconds idle became sixty-six under a
/// full parallel suite, so the ceiling tracks load rather than work.
const SETTLE_TICKS: usize = 4000;
const SETTLE_MS: u64 = 50;

const EXPR_HEADER: &str = "t,op,a,b\n";
/// Leaves are nullary symbols named after themselves — giving every literal the
/// operator "lit" would make congruence prove 1 = 2.
const BASE_ROWS: &str =
    "a,a,_,_\nb,b,_,_\n2,2,_,_\n1,1,_,_\n\"mul(a,2)\",mul,a,2\n\"mul(b,2)\",mul,b,2\n";
const DIV_ROW: &str = "\"div(mul(a,2),2)\",div,\"mul(a,2)\",2\n";

fn pairs(
    state: &Arc<Mutex<dep2_core::engine::RelationState>>,
    types: &dep2_core::engine::RelationTypes,
) -> Vec<String> {
    let st = state.lock().unwrap();
    let Some(rows) = st.get("proved_equal") else {
        return Vec::new();
    };
    let ty = &types["proved_equal"];
    let mut out: Vec<String> = dep2_core::engine::live_rows(rows)
        .map(|r| {
            let d = dep2_core::engine::decode_state_row(r, ty);
            format!("{}={}", d[0], d[1])
        })
        .collect();
    out.sort();
    out
}

#[test]
fn equality_saturation_invents_terms_and_still_retracts() {
    let dir = tempfile::tempdir().unwrap();
    let expr = dir.path().join("expr.csv");
    std::fs::write(&expr, format!("{EXPR_HEADER}{BASE_ROWS}{DIV_ROW}")).unwrap();

    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: false,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    let mut config = HashMap::new();
    config.insert("path".to_string(), expr.to_string_lossy().into_owned());
    engine.add_source(Some("input_node".to_string()), "csv", config);
    // Tests run with the crate dir as cwd; the example lives at the repo root.
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/egraph/saturation.dl");
    engine
        .load_program_file(&example)
        .expect("saturation.dl loads");

    let state = engine.state();
    let types = engine.relation_types();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    let settle = |want: &[&str]| -> Vec<String> {
        let mut got = Vec::new();
        for _ in 0..SETTLE_TICKS {
            thread::sleep(Duration::from_millis(SETTLE_MS));
            got = pairs(&state, &types);
            if got == want {
                break;
            }
        }
        got
    };

    // R1 mints `shl(a,1)` and `shl(b,1)`, which appear nowhere in the input.
    // R2 then matches a shl node in the DIVIDEND'S CLASS — e-matching modulo
    // equality — and proves the division equals `a`.
    let saturated = [
        "a=div(mul(a,2),2)",
        "mul(a,2)=shl(a,1)",
        "mul(b,2)=shl(b,1)",
    ];
    assert_eq!(
        settle(&saturated),
        saturated,
        "term-creating rewrites should saturate"
    );

    // Delete the division from the input program: the proof that depended on it
    // goes, the strength-reductions that did not stay.
    std::fs::write(&expr, format!("{EXPR_HEADER}{BASE_ROWS}")).unwrap();
    let reduced = ["mul(a,2)=shl(a,1)", "mul(b,2)=shl(b,1)"];
    assert_eq!(
        settle(&reduced),
        reduced,
        "retracting the division retracts only what rested on it"
    );

    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
}

/// A depth-growing rewrite terminates under a `depth` guard, and does not
/// without one.
///
/// `X = add(X, 0)` deepens its input every time it fires, so the fixpoint never
/// closes — the engine's divergence detector eventually reports that an epoch
/// has not completed. Guarding on a `merge(max)` fold over the children bounds
/// it exactly: at `depth < 2` the reachable terms are precisely `a`,
/// `add(a,0)` and `add(add(a,0),0)`.
///
/// Note which blowups do NOT need this. Structural ids make creation
/// idempotent, so commutativity re-creates a term that already exists and
/// closes on its own; depth is the quantity that actually runs away.
#[test]
fn a_depth_guard_bounds_a_term_growing_rewrite() {
    const PROG: &str = "\
.in
.decl input_node(t: string, op: string, a: string, b: string)

.printsize
.decl node(t: string, op: string, a: string, b: string)
.decl term(t: string)
.decl eq_input(x: string, y: string)
.decl eq_edge(x: string, y: string)
.decl cnode(t: string, op: string, la: string, lb: string)
.decl form_rep(op: string, la: string, lb: string, rep: string) merge(min)
.decl depth(t: string, d: number) merge(max)

.out
.decl leader(t: string, rep: string) merge(min)

.rule
node(T, Op, A, B) :- input_node(T, Op, A, B).

depth(T, 0) :- node(T, _, \"_\", \"_\").
depth(T, D + 1) :- node(T, _, A, _), A != \"_\", depth(A, D).
depth(T, D + 1) :- node(T, _, _, B), B != \"_\", depth(B, D).

// X = add(X, 0): deepens on every firing, so the guard is load-bearing.
node(concat(concat(\"add(\", X), \",0)\"), \"add\", X, \"0\") :-
    node(X, _, _, _), depth(X, D), D < 2.
eq_input(X, concat(concat(\"add(\", X), \",0)\")) :-
    node(X, _, _, _), depth(X, D), D < 2.

term(T) :- node(T, _, _, _).
term(A) :- node(_, _, A, _).
term(B) :- node(_, _, _, B).
term(X) :- eq_input(X, _).
term(Y) :- eq_input(_, Y).
eq_edge(X, Y) :- eq_input(X, Y).
eq_edge(Y, X) :- eq_input(X, Y).
cnode(T, Op, LA, LB) :- node(T, Op, A, B), leader(A, LA), leader(B, LB).
form_rep(Op, LA, LB, T) :- cnode(T, Op, LA, LB).
eq_edge(T, R) :- cnode(T, Op, LA, LB), form_rep(Op, LA, LB, R).
eq_edge(R, T) :- cnode(T, Op, LA, LB), form_rep(Op, LA, LB, R).
leader(T, T) :- term(T).
leader(X, L) :- eq_edge(X, Y), leader(Y, L).
leader(X, L) :- leader(X, M), leader(M, L).
";
    let dir = tempfile::tempdir().unwrap();
    let seed = dir.path().join("seed.csv");
    std::fs::write(&seed, "t,op,a,b\na,a,_,_\n").unwrap();

    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: false,
    });
    engine.add_plugin(Box::new(CsvPlugin));
    let mut config = HashMap::new();
    config.insert("path".to_string(), seed.to_string_lossy().into_owned());
    engine.add_source(Some("input_node".to_string()), "csv", config);
    engine.load_program(PROG).expect("program loads");

    let state = engine.state();
    let types = engine.relation_types();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    let terms = || -> Vec<String> {
        let st = state.lock().unwrap();
        let Some(rows) = st.get("leader") else {
            return Vec::new();
        };
        let ty = &types["leader"];
        let mut out: Vec<String> = dep2_core::engine::live_rows(rows)
            .map(|r| dep2_core::engine::decode_state_row(r, ty)[0].clone())
            .collect();
        out.sort();
        out.dedup();
        out
    };
    // "_" is the no-child sentinel and "0" the literal, both terms in their
    // own right; the guard stops the tower at two applications.
    let want: Vec<String> = ["0", "_", "a", "add(a,0)", "add(add(a,0),0)"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut got = Vec::new();
    for _ in 0..SETTLE_TICKS {
        thread::sleep(Duration::from_millis(SETTLE_MS));
        got = terms();
        if got == want {
            break;
        }
    }
    assert_eq!(
        got, want,
        "the depth guard should stop the tower at depth 2"
    );

    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
}
