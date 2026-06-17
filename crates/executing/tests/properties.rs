//! Property-based tests for the FlowLog engine.
//!
//! Two kinds of property:
//!   1. **Batch correctness** — random EDB facts run through the batch pipeline
//!      must match a reference evaluator (join, recursion, stratified negation).
//!   2. **Incremental == batch** — a random sequence of inserts followed by
//!      deletes, streamed through the engine, must converge to the same result
//!      as a batch run over the final facts. This guards incremental
//!      maintenance, including retraction through recursion and negation.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use catalog::head::aggregation_catalog_from_program;
use executing::arg::Args;
use executing::dataflow::{program_execution, streaming_program_execution, StreamingConfig};
use parsing::parser::Program;
use planning::program::ProgramQueryPlan;
use proptest::prelude::*;
use reading::{KV_MAX, ROW_MAX};
use strata::stratification::Strata;

// ---------------------------------------------------------------------------
// Harnesses
// ---------------------------------------------------------------------------

fn build(program_dl: &str) -> (Program, Strata, ProgramQueryPlan, bool) {
    // parse from a temp file (FlowLog parses from a path)
    let dir = tempfile::tempdir().unwrap();
    let prog_path = dir.path().join("program.dl");
    std::fs::write(&prog_path, program_dl).unwrap();
    let program = Program::parse_from(prog_path.to_str().unwrap());
    let strata = Strata::from_parser(program.clone());
    let plan = ProgramQueryPlan::from_strata(&strata, false, None);
    let fat = plan.should_use_fat_mode(false, KV_MAX, ROW_MAX);
    (program, strata, plan, fat)
}

/// Run a program against EDB facts via the batch pipeline; return each IDB's set.
fn run_batch(
    program_dl: &str,
    edbs: &[(&str, Vec<Vec<i64>>)],
) -> HashMap<String, HashSet<Vec<i64>>> {
    let dir = tempfile::tempdir().unwrap();
    let facts_dir = dir.path().join("facts");
    let out_dir = dir.path().join("out");
    std::fs::create_dir_all(&facts_dir).unwrap();
    std::fs::create_dir_all(out_dir.join("csvs")).unwrap();

    for (rel, rows) in edbs {
        let mut s = String::new();
        for row in rows {
            s.push_str(
                &row.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            );
            s.push('\n');
        }
        std::fs::write(facts_dir.join(format!("{}.facts", rel)), s).unwrap();
    }

    let prog_path = dir.path().join("program.dl");
    std::fs::write(&prog_path, program_dl).unwrap();
    let program = Program::parse_from(prog_path.to_str().unwrap());
    let strata = Strata::from_parser(program.clone());
    let plan = ProgramQueryPlan::from_strata(&strata, false, None);
    let fat = plan.should_use_fat_mode(false, KV_MAX, ROW_MAX);
    let idb_map = aggregation_catalog_from_program(&program);

    let args = Args::new(
        prog_path.to_string_lossy().into_owned(),
        facts_dir.to_string_lossy().into_owned(),
        Some(out_dir.to_string_lossy().into_owned()),
        ",".to_string(),
        1,
    );
    program_execution(args, strata, plan.program_plan().to_owned(), fat, idb_map);

    let mut result: HashMap<String, HashSet<Vec<i64>>> = HashMap::new();
    for decl in program.idbs() {
        let name = decl.name().to_string();
        let mut set = HashSet::new();
        read_csv_into(&out_dir.join("csvs"), &name, &mut set);
        result.insert(name, set);
    }
    result
}

fn read_csv_into(dir: &Path, rel: &str, set: &mut HashSet<Vec<i64>>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let prefix = format!("{}.csv", rel);
    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().to_string();
        if fname == prefix || fname.starts_with(&prefix) {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                for line in content.lines().filter(|l| !l.trim().is_empty()) {
                    let row: Vec<i64> =
                        line.split(',').map(|s| s.trim().parse().unwrap()).collect();
                    set.insert(row);
                }
            }
        }
    }
}

/// Stream `inserts` (epoch 0) then `deletes` (epoch 1) through the engine and
/// return each IDB's final set (rows with net positive multiplicity).
fn run_streaming(
    program_dl: &str,
    edb_names: &[&str],
    inserts: &[(&str, Vec<i64>)],
    deletes: &[(&str, Vec<i64>)],
) -> HashMap<String, HashSet<Vec<i64>>> {
    let dir = tempfile::tempdir().unwrap();
    let facts_dir = dir.path().join("facts");
    std::fs::create_dir_all(&facts_dir).unwrap();
    let prog_path = dir.path().join("program.dl");
    std::fs::write(&prog_path, program_dl).unwrap();

    let (program, strata, plan, fat) = build(program_dl);
    for decl in program.edbs() {
        std::fs::write(facts_dir.join(format!("{}.facts", decl.name())), "").unwrap();
    }
    let idb_map = aggregation_catalog_from_program(&program);
    let args = Args::new(
        prog_path.to_string_lossy().into_owned(),
        facts_dir.to_string_lossy().into_owned(),
        None,
        ",".to_string(),
        1,
    );

    let mut channels = HashMap::new();
    let mut senders = HashMap::new();
    for rel in edb_names {
        let (tx, rx) = crossbeam_channel::bounded::<(Vec<i64>, isize)>(100_000);
        channels.insert(rel.to_string(), rx);
        senders.insert(rel.to_string(), tx);
    }
    let streaming_edbs: HashSet<String> = edb_names.iter().map(|s| s.to_string()).collect();

    let acc: Arc<Mutex<HashMap<(String, Vec<i64>), isize>>> = Arc::new(Mutex::new(HashMap::new()));
    let acc_cb = Arc::clone(&acc);
    let output_callback: Arc<dyn Fn(&str, Vec<String>, isize) + Send + Sync> =
        Arc::new(move |rel: &str, vals: Vec<String>, diff: isize| {
            let row: Vec<i64> = vals.iter().map(|s| s.trim().parse().unwrap_or(0)).collect();
            *acc_cb
                .lock()
                .unwrap()
                .entry((rel.to_string(), row))
                .or_insert(0) += diff;
        });

    let shutdown = Arc::new(AtomicBool::new(false));
    let cfg = StreamingConfig {
        channels,
        streaming_edbs,
        output_callback,
        shutdown: Arc::clone(&shutdown),
    };

    let handle = std::thread::spawn(move || {
        streaming_program_execution(
            args,
            strata,
            plan.program_plan().to_owned(),
            fat,
            idb_map,
            cfg,
        );
    });

    // Epoch 0: inserts.
    for (rel, row) in inserts {
        senders[*rel].send((row.clone(), 1)).unwrap();
    }
    std::thread::sleep(Duration::from_millis(400));
    // Epoch 1: deletes (exercises incremental retraction / re-derivation).
    for (rel, row) in deletes {
        senders[*rel].send((row.clone(), -1)).unwrap();
    }
    std::thread::sleep(Duration::from_millis(400));

    shutdown.store(true, Ordering::Relaxed);
    drop(senders);
    handle.join().unwrap();

    let mut result: HashMap<String, HashSet<Vec<i64>>> = HashMap::new();
    for decl in program.idbs() {
        result.entry(decl.name().to_string()).or_default();
    }
    for ((rel, row), count) in acc.lock().unwrap().iter() {
        if *count > 0 {
            result.entry(rel.clone()).or_default().insert(row.clone());
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Reference evaluators
// ---------------------------------------------------------------------------

fn reference_tc(edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    let mut tc = edges.clone();
    loop {
        let snap: Vec<(i64, i64)> = tc.iter().cloned().collect();
        let mut added = false;
        for &(x, z) in &snap {
            for &(z2, y) in &snap {
                if z == z2 && tc.insert((x, y)) {
                    added = true;
                }
            }
        }
        if !added {
            break;
        }
    }
    tc.into_iter().map(|(x, y)| vec![x, y]).collect()
}

/// leaf = nodes with no outgoing edge (antijoin / negation).
fn reference_leaf(nodes: &HashSet<i64>, edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    let with_succ: HashSet<i64> = edges.iter().map(|&(x, _)| x).collect();
    nodes
        .iter()
        .filter(|n| !with_succ.contains(n))
        .map(|&n| vec![n])
        .collect()
}

// ---------------------------------------------------------------------------
// Programs
// ---------------------------------------------------------------------------

const TC_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl tc(x: number, y: number)

.rule
tc(X, Y) :- edge(X, Y).
tc(X, Y) :- tc(X, Z), edge(Z, Y).
";

const LEAF_PROGRAM: &str = "\
.in
.decl node(x: number)
.input node.facts
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl has_succ(x: number)
.decl leaf(x: number)

.rule
has_succ(X) :- edge(X, _).
leaf(X) :- node(X), !has_succ(X).
";

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn edges_strategy() -> impl Strategy<Value = Vec<(i64, i64)>> {
    prop::collection::vec((0i64..5, 0i64..5), 0..9)
}

// ---------------------------------------------------------------------------
// Batch correctness properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn batch_tc_matches_reference(edges in edges_strategy()) {
        let edge_set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = edge_set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(TC_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["tc"].clone(), reference_tc(&edge_set));
    }

    #[test]
    fn batch_leaf_matches_reference(edges in edges_strategy()) {
        let nodes: HashSet<i64> = (0i64..5).collect();
        let edge_set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let node_rows: Vec<Vec<i64>> = nodes.iter().map(|&n| vec![n]).collect();
        let edge_rows: Vec<Vec<i64>> = edge_set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(LEAF_PROGRAM, &[("node", node_rows), ("edge", edge_rows)]);
        prop_assert_eq!(got["leaf"].clone(), reference_leaf(&nodes, &edge_set));
    }
}

// ---------------------------------------------------------------------------
// Incremental == batch properties (guards retraction through recursion + negation)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    #[test]
    fn streaming_tc_equals_batch(
        edges in edges_strategy(),
        to_delete in edges_strategy(),
    ) {
        // Insert all `edges`, then delete those in `to_delete`. Final = set diff.
        let inserted: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let deleted: HashSet<(i64, i64)> = to_delete
            .iter()
            .cloned()
            .filter(|e| inserted.contains(e))
            .collect();
        let final_edges: HashSet<(i64, i64)> = inserted.difference(&deleted).cloned().collect();

        let ins: Vec<(&str, Vec<i64>)> =
            inserted.iter().map(|&(x, y)| ("edge", vec![x, y])).collect();
        let del: Vec<(&str, Vec<i64>)> =
            deleted.iter().map(|&(x, y)| ("edge", vec![x, y])).collect();

        let streamed = run_streaming(TC_PROGRAM, &["edge"], &ins, &del);
        let final_rows: Vec<Vec<i64>> = final_edges.iter().map(|&(x, y)| vec![x, y]).collect();
        let batch = run_batch(TC_PROGRAM, &[("edge", final_rows)]);
        prop_assert_eq!(streamed["tc"].clone(), batch["tc"].clone());
    }

    #[test]
    fn streaming_leaf_equals_batch(
        edges in edges_strategy(),
        to_delete in edges_strategy(),
    ) {
        // Negation under streaming: deleting edges can re-derive `leaf` rows.
        let nodes: Vec<i64> = (0i64..5).collect();
        let inserted: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let deleted: HashSet<(i64, i64)> = to_delete
            .iter()
            .cloned()
            .filter(|e| inserted.contains(e))
            .collect();
        let final_edges: HashSet<(i64, i64)> = inserted.difference(&deleted).cloned().collect();

        let mut ins: Vec<(&str, Vec<i64>)> = nodes.iter().map(|&n| ("node", vec![n])).collect();
        ins.extend(inserted.iter().map(|&(x, y)| ("edge", vec![x, y])));
        let del: Vec<(&str, Vec<i64>)> =
            deleted.iter().map(|&(x, y)| ("edge", vec![x, y])).collect();

        let streamed = run_streaming(LEAF_PROGRAM, &["node", "edge"], &ins, &del);

        let node_rows: Vec<Vec<i64>> = nodes.iter().map(|&n| vec![n]).collect();
        let edge_rows: Vec<Vec<i64>> = final_edges.iter().map(|&(x, y)| vec![x, y]).collect();
        let batch = run_batch(LEAF_PROGRAM, &[("node", node_rows), ("edge", edge_rows)]);

        prop_assert_eq!(streamed["leaf"].clone(), batch["leaf"].clone());
    }
}
