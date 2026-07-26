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
        for _ in 0..400 {
            thread::sleep(Duration::from_millis(50));
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
