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

/// two-hop = { (x, z) | exists y. edge(x,y) and edge(y,z) } (projection + join).
fn reference_two_hop(edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    let mut out = HashSet::new();
    for &(x, y) in edges {
        for &(y2, z) in edges {
            if y == y2 {
                out.insert(vec![x, z]);
            }
        }
    }
    out
}

/// sibling = { (x, y) | exists p. par(p,x) and par(p,y) and x != y }
/// (self-join with an inequality filter; symmetric, irreflexive).
fn reference_sibling(par: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    let mut out = HashSet::new();
    for &(p, x) in par {
        for &(p2, y) in par {
            if p == p2 && x != y {
                out.insert(vec![x, y]);
            }
        }
    }
    out
}

/// reach from a fixed source 0: reflexive-ish transitive reachability via edges,
/// expressed as union of a base rule and a recursive rule.
fn reference_reach(edges: &HashSet<(i64, i64)>, src: i64) -> HashSet<Vec<i64>> {
    let mut reach: HashSet<i64> = HashSet::new();
    // base: direct successors of src
    for &(x, y) in edges {
        if x == src {
            reach.insert(y);
        }
    }
    loop {
        let snap: Vec<i64> = reach.iter().cloned().collect();
        let mut added = false;
        for n in snap {
            for &(x, y) in edges {
                if x == n && reach.insert(y) {
                    added = true;
                }
            }
        }
        if !added {
            break;
        }
    }
    reach.into_iter().map(|n| vec![n]).collect()
}

/// minval = { (x, min y) | edge(x,y) } — per-key minimum aggregation.
fn reference_minval(edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    let mut by_key: HashMap<i64, i64> = HashMap::new();
    for &(x, y) in edges {
        by_key
            .entry(x)
            .and_modify(|m| {
                if y < *m {
                    *m = y
                }
            })
            .or_insert(y);
    }
    by_key.into_iter().map(|(x, m)| vec![x, m]).collect()
}

/// maxval = { (x, max y) | edge(x,y) }.
fn reference_maxval(edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    let mut by_key: HashMap<i64, i64> = HashMap::new();
    for &(x, y) in edges {
        by_key
            .entry(x)
            .and_modify(|m| {
                if y > *m {
                    *m = y
                }
            })
            .or_insert(y);
    }
    by_key.into_iter().map(|(x, m)| vec![x, m]).collect()
}

/// outdeg = { (x, #distinct y) | edge(x,y) } — count aggregation.
fn reference_count(edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    let mut by_key: HashMap<i64, HashSet<i64>> = HashMap::new();
    for &(x, y) in edges {
        by_key.entry(x).or_default().insert(y);
    }
    by_key
        .into_iter()
        .map(|(x, ys)| vec![x, ys.len() as i64])
        .collect()
}

/// total = { (x, sum of distinct y) | edge(x,y) } — sum aggregation.
fn reference_sum(edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    let mut by_key: HashMap<i64, HashSet<i64>> = HashMap::new();
    for &(x, y) in edges {
        by_key.entry(x).or_default().insert(y);
    }
    by_key
        .into_iter()
        .map(|(x, ys)| vec![x, ys.iter().sum::<i64>()])
        .collect()
}

/// unreach = nodes 0..5 not reachable from source 0 (recursion + negation).
fn reference_unreach(nodes: &HashSet<i64>, edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    let reachable: HashSet<i64> = reference_reach(edges, 0)
        .into_iter()
        .map(|r| r[0])
        .collect();
    nodes
        .iter()
        .filter(|n| !reachable.contains(n))
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

const TWO_HOP_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl two_hop(x: number, z: number)

.rule
two_hop(X, Z) :- edge(X, Y), edge(Y, Z).
";

const SIBLING_PROGRAM: &str = "\
.in
.decl par(p: number, c: number)
.input par.facts

.printsize
.decl sibling(x: number, y: number)

.rule
sibling(X, Y) :- par(P, X), par(P, Y), X != Y.
";

// Reachability from the constant source 0, as a base + recursive union.
const REACH_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl reach(n: number)

.rule
reach(Y) :- edge(0, Y).
reach(Y) :- reach(X), edge(X, Y).
";

const MINVAL_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl minval(x: number, m: number)

.rule
minval(X, min(Y)) :- edge(X, Y).
";

const MAXVAL_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl maxval(x: number, m: number)

.rule
maxval(X, max(Y)) :- edge(X, Y).
";

const COUNT_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl outdeg(x: number, c: number)

.rule
outdeg(X, count(Y)) :- edge(X, Y).
";

const SUM_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl total(x: number, s: number)

.rule
total(X, sum(Y)) :- edge(X, Y).
";

// Nodes not reachable from source 0: recursion (reach) feeding negation.
const UNREACH_PROGRAM: &str = "\
.in
.decl node(x: number)
.input node.facts
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl reach(n: number)
.decl unreach(n: number)

.rule
reach(Y) :- edge(0, Y).
reach(Y) :- reach(X), edge(X, Y).
unreach(N) :- node(N), !reach(N).
";

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn edges_strategy() -> impl Strategy<Value = Vec<(i64, i64)>> {
    prop::collection::vec((0i64..5, 0i64..5), 0..9)
}

/// All 5 nodes `0..5`, used as a permanent (never-deleted) `node` relation.
fn all_nodes() -> Vec<i64> {
    (0i64..5).collect()
}

/// Run a binary-EDB program both ways — stream (insert all, then delete a
/// subset) vs. batch (over the surviving rows) — and return `(streamed, batch)`
/// for relation `idb`. `churn_rel` is the inserted/deleted relation; if `nodes`
/// is given, a permanent `node` relation is seeded (never deleted). `deleted`
/// must be a subset of `inserted`.
fn stream_vs_batch(
    program: &str,
    idb: &str,
    churn_rel: &str,
    nodes: Option<&[i64]>,
    inserted: &HashSet<(i64, i64)>,
    deleted: &HashSet<(i64, i64)>,
) -> (HashSet<Vec<i64>>, HashSet<Vec<i64>>) {
    let mut edb_names: Vec<&str> = vec![churn_rel];
    let mut ins: Vec<(&str, Vec<i64>)> = Vec::new();
    if let Some(ns) = nodes {
        edb_names.push("node");
        ins.extend(ns.iter().map(|&n| ("node", vec![n])));
    }
    ins.extend(inserted.iter().map(|&(a, b)| (churn_rel, vec![a, b])));
    let del: Vec<(&str, Vec<i64>)> = deleted
        .iter()
        .map(|&(a, b)| (churn_rel, vec![a, b]))
        .collect();
    let streamed = run_streaming(program, &edb_names, &ins, &del);

    let final_pairs: HashSet<(i64, i64)> = inserted.difference(deleted).cloned().collect();
    let mut edbs: Vec<(&str, Vec<Vec<i64>>)> = vec![(
        churn_rel,
        final_pairs.iter().map(|&(a, b)| vec![a, b]).collect(),
    )];
    if let Some(ns) = nodes {
        edbs.push(("node", ns.iter().map(|&n| vec![n]).collect()));
    }
    let batch = run_batch(program, &edbs);

    (
        streamed.get(idb).cloned().unwrap_or_default(),
        batch.get(idb).cloned().unwrap_or_default(),
    )
}

/// Split `inserted`/`deleted` from two generated edge lists (deleted ⊆ inserted).
fn ins_del(
    edges: &[(i64, i64)],
    to_delete: &[(i64, i64)],
) -> (HashSet<(i64, i64)>, HashSet<(i64, i64)>) {
    let inserted: HashSet<(i64, i64)> = edges.iter().cloned().collect();
    let deleted: HashSet<(i64, i64)> = to_delete
        .iter()
        .cloned()
        .filter(|e| inserted.contains(e))
        .collect();
    (inserted, deleted)
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

    /// projection + join (two-hop), no recursion.
    #[test]
    fn batch_two_hop_matches_reference(edges in edges_strategy()) {
        let edge_set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = edge_set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(TWO_HOP_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["two_hop"].clone(), reference_two_hop(&edge_set));
    }

    /// self-join with an inequality (`X != Y`) filter.
    #[test]
    fn batch_sibling_matches_reference(par in edges_strategy()) {
        let par_set: HashSet<(i64, i64)> = par.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = par_set.iter().map(|&(p, c)| vec![p, c]).collect();
        let got = run_batch(SIBLING_PROGRAM, &[("par", rows)]);
        prop_assert_eq!(got["sibling"].clone(), reference_sibling(&par_set));
    }

    /// union of base + recursive rule, recursion from a constant source.
    #[test]
    fn batch_reach_matches_reference(edges in edges_strategy()) {
        let edge_set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = edge_set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(REACH_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["reach"].clone(), reference_reach(&edge_set, 0));
    }

    /// per-key `min` aggregation.
    #[test]
    fn batch_minval_matches_reference(edges in edges_strategy()) {
        let edge_set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = edge_set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(MINVAL_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["minval"].clone(), reference_minval(&edge_set));
    }

    /// per-key `max` aggregation.
    #[test]
    fn batch_maxval_matches_reference(edges in edges_strategy()) {
        let edge_set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = edge_set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(MAXVAL_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["maxval"].clone(), reference_maxval(&edge_set));
    }

    /// per-key `count` aggregation.
    #[test]
    fn batch_count_matches_reference(edges in edges_strategy()) {
        let edge_set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = edge_set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(COUNT_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["outdeg"].clone(), reference_count(&edge_set));
    }

    /// per-key `sum` aggregation.
    #[test]
    fn batch_sum_matches_reference(edges in edges_strategy()) {
        let edge_set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = edge_set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(SUM_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["total"].clone(), reference_sum(&edge_set));
    }

    /// recursion feeding negation: nodes not reachable from source 0.
    #[test]
    fn batch_unreach_matches_reference(edges in edges_strategy()) {
        let nodes: HashSet<i64> = (0i64..5).collect();
        let edge_set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let node_rows: Vec<Vec<i64>> = nodes.iter().map(|&n| vec![n]).collect();
        let edge_rows: Vec<Vec<i64>> = edge_set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(UNREACH_PROGRAM, &[("node", node_rows), ("edge", edge_rows)]);
        prop_assert_eq!(got["unreach"].clone(), reference_unreach(&nodes, &edge_set));
    }
}

// ---------------------------------------------------------------------------
// Incremental == batch properties (guards retraction through recursion + negation)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    /// Incremental recursion: insert edges (cyclic graphs included), delete a
    /// subset, and the streamed `tc` must equal a batch run over the remaining
    /// edges. This covers retraction of facts that lose their only well-founded
    /// support but retain circular support — see `streaming_tc_cyclic_retraction`.
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

    /// projection + join, incrementally.
    #[test]
    fn streaming_two_hop_equals_batch(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let (s, b) = stream_vs_batch(TWO_HOP_PROGRAM, "two_hop", "edge", None, &ins, &del);
        prop_assert_eq!(s, b);
    }

    /// self-join with inequality, incrementally.
    #[test]
    fn streaming_sibling_equals_batch(par in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&par, &to_delete);
        let (s, b) = stream_vs_batch(SIBLING_PROGRAM, "sibling", "par", None, &ins, &del);
        prop_assert_eq!(s, b);
    }

    /// recursion from a constant source, incrementally.
    #[test]
    fn streaming_reach_equals_batch(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let (s, b) = stream_vs_batch(REACH_PROGRAM, "reach", "edge", None, &ins, &del);
        prop_assert_eq!(s, b);
    }

    /// `min` aggregation, incrementally (deletes can raise the per-key minimum).
    #[test]
    fn streaming_minval_equals_batch(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let (s, b) = stream_vs_batch(MINVAL_PROGRAM, "minval", "edge", None, &ins, &del);
        prop_assert_eq!(s, b);
    }

    /// `count` aggregation, incrementally (deletes decrement per-key counts).
    #[test]
    fn streaming_count_equals_batch(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let (s, b) = stream_vs_batch(COUNT_PROGRAM, "outdeg", "edge", None, &ins, &del);
        prop_assert_eq!(s, b);
    }

    /// `max` aggregation, incrementally (deletes can lower the per-key maximum).
    #[test]
    fn streaming_maxval_equals_batch(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let (s, b) = stream_vs_batch(MAXVAL_PROGRAM, "maxval", "edge", None, &ins, &del);
        prop_assert_eq!(s, b);
    }

    /// `sum` aggregation, incrementally (deletes decrement the per-key sum).
    #[test]
    fn streaming_sum_equals_batch(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let (s, b) = stream_vs_batch(SUM_PROGRAM, "total", "edge", None, &ins, &del);
        prop_assert_eq!(s, b);
    }

    /// recursion feeding negation, incrementally: nodes (un)reachable from 0 as
    /// edges are added and removed (cyclic graphs included).
    #[test]
    fn streaming_unreach_equals_batch(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let nodes = all_nodes();
        let (s, b) = stream_vs_batch(UNREACH_PROGRAM, "unreach", "edge", Some(&nodes), &ins, &del);
        prop_assert_eq!(s, b);
    }
}

/// Regression test for incremental recursion retraction through a cycle.
///
/// Edges {(0,2),(2,2)}; delete edge(0,2). The correct `tc` afterward is
/// {(2,2)} — `tc(0,2)` is no longer derivable (its only remaining "derivation"
/// is the circular `tc(0,2) :- tc(0,2), edge(2,2)`, which is not well-founded),
/// so the engine must retract it. Previously it didn't: recursion used DD's
/// `SemigroupVariable` ("only grows"); under the `isize` semiring it now uses
/// the full `Variable`, which subtracts the prior iterate and retracts.
#[test]
fn streaming_tc_cyclic_retraction() {
    let ins = vec![("edge", vec![0, 2]), ("edge", vec![2, 2])];
    let del = vec![("edge", vec![0, 2])];
    let streamed = run_streaming(TC_PROGRAM, &["edge"], &ins, &del);
    let batch = run_batch(TC_PROGRAM, &[("edge", vec![vec![2, 2]])]);
    assert_eq!(
        streamed["tc"], batch["tc"],
        "expected {{(2,2)}} after deletion"
    );
}

/// Regression test for incremental aggregation retraction.
///
/// Insert edge(0,2) then delete it. The group for key 0 becomes empty, so the
/// aggregate must be retracted entirely. Previously the aggregation reduce logic
/// only emitted the new value and never subtracted the previously-produced
/// output, so `minval(0,2)` / `outdeg(0,1)` lingered after the last contributing
/// fact was deleted.
#[test]
fn streaming_aggregation_retraction() {
    let ins = vec![("edge", vec![0, 2])];
    let del = vec![("edge", vec![0, 2])];
    for (program, idb) in [
        (MINVAL_PROGRAM, "minval"),
        (MAXVAL_PROGRAM, "maxval"),
        (COUNT_PROGRAM, "outdeg"),
        (SUM_PROGRAM, "total"),
    ] {
        let streamed = run_streaming(program, &["edge"], &ins, &del);
        assert!(
            streamed[idb].is_empty(),
            "{}: expected empty after deleting the only fact, got {:?}",
            idb,
            streamed[idb]
        );
    }
}
