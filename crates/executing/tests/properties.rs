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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use catalog::head::aggregation_catalog_from_program;
use executing::arg::Args;
use executing::dataflow::{
    program_execution, streaming_program_execution, CommandLog, CompiledQuery, QueryCommand,
    StreamingConfig,
};
use parsing::decl::DataType;
use parsing::parser::Program;
use planning::program::ProgramQueryPlan;
use proptest::prelude::*;
use reading::{KV_MAX, ROW_MAX};
use strata::stratification::Strata;

// ---------------------------------------------------------------------------
// Harnesses
// ---------------------------------------------------------------------------

/// The production literal-interning transform (see `Dep2::load_program_named`).
fn intern_text_literals(c: &parsing::rule::Const) -> Option<parsing::rule::Const> {
    match c {
        parsing::rule::Const::Text(quoted) => Some(parsing::rule::Const::Integer(
            reading::intern_literal(quoted),
        )),
        _ => None,
    }
}

fn build(program_dl: &str) -> (Program, Strata, ProgramQueryPlan, bool) {
    // Through the production parser (the chumsky front-end), with the
    // production literal-interning transform (string consts -> ids).
    let mut program = syntax::parse(program_dl)
        .unwrap_or_else(|d| panic!("{}", syntax::render("program.dl", program_dl, &d, false)));
    program.map_constants(intern_text_literals);
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
    let mut program = syntax::parse(program_dl)
        .unwrap_or_else(|d| panic!("{}", syntax::render("program.dl", program_dl, &d, false)));
    // Same literal-interning transform production runs; without it a program
    // with string constants reaches the dataflow with un-interned `Text`
    // constants and panics. Every program here is integer-only, so the
    // omission went unnoticed. NOTE this harness still cannot carry string
    // DATA: rows are written to `.facts` as decimal and the reader re-interns
    // the token for a string column, so an id written here comes back as the
    // id of its own digits. Programs over string columns want the engine
    // harness instead (see dep2-core/tests/egraph.rs).
    program.map_constants(intern_text_literals);
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

    let _ = &edb_names;
    let (tx, rx) =
        crossbeam_channel::bounded::<(Arc<str>, smallvec::SmallVec<[i64; 8]>, isize)>(100_000);
    let acc: Arc<Mutex<HashMap<(String, Vec<i64>), isize>>> = Arc::new(Mutex::new(HashMap::new()));
    let acc_cb = Arc::clone(&acc);
    let output_callback: Arc<dyn Fn(&str, smallvec::SmallVec<[i64; 8]>, isize) + Send + Sync> =
        Arc::new(
            move |rel: &str, row: smallvec::SmallVec<[i64; 8]>, diff: isize| {
                *acc_cb
                    .lock()
                    .unwrap()
                    .entry((rel.to_string(), row.to_vec()))
                    .or_insert(0) += diff;
            },
        );

    let shutdown = Arc::new(AtomicBool::new(false));
    let cfg = StreamingConfig {
        input: rx,
        output_callback,
        shutdown: Arc::clone(&shutdown),
        output_seq: Arc::new(AtomicU64::new(0)),
        publish: HashSet::new(),
        commands: CommandLog::default(),
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
        tx.send((Arc::from(*rel), row.iter().copied().collect(), 1))
            .unwrap();
    }
    std::thread::sleep(Duration::from_millis(400));
    // Epoch 1: deletes (exercises incremental retraction / re-derivation).
    for (rel, row) in deletes {
        tx.send((Arc::from(*rel), row.iter().copied().collect(), -1))
            .unwrap();
    }
    std::thread::sleep(Duration::from_millis(400));

    shutdown.store(true, Ordering::Relaxed);
    drop(tx);
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

/// n = { (f, #distinct a) | t(_, f, a) } — count over the projected set.
fn reference_proj_count(triples: &HashSet<(i64, i64, i64)>) -> HashSet<Vec<i64>> {
    let mut by_key: HashMap<i64, HashSet<i64>> = HashMap::new();
    for &(_, f, a) in triples {
        by_key.entry(f).or_default().insert(a);
    }
    by_key
        .into_iter()
        .map(|(f, aa)| vec![f, aa.len() as i64])
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

/// All-pairs path lengths folded by `op` ("min"/"max") over positive-weight
/// edges: the transitive closure of edge concatenation, reduced per (x, y).
fn reference_merge_path(edges: &HashSet<(i64, i64, i64)>, op: &str) -> HashSet<Vec<i64>> {
    // The engine folds per key at EVERY step of the fixpoint, so this relaxes a
    // (x, y) -> best map the same way. Enumerating raw derivations instead
    // would not terminate: each trip around a cycle yields another length.
    let fold = |a: i64, b: i64| if op == "min" { a.min(b) } else { a.max(b) };
    let mut best: HashMap<(i64, i64), i64> = HashMap::new();
    for &(x, y, l) in edges {
        best.entry((x, y))
            .and_modify(|b| *b = fold(*b, l))
            .or_insert(l);
    }
    loop {
        let mut changed = false;
        for (&(x, y), &l1) in &best.clone() {
            for &(y2, z, l2) in edges {
                if y != y2 {
                    continue;
                }
                let candidate = l1 + l2;
                let merged = match best.get(&(x, z)) {
                    Some(&current) => fold(current, candidate),
                    None => candidate,
                };
                if best.get(&(x, z)) != Some(&merged) {
                    best.insert((x, z), merged);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    best.into_iter().map(|((x, y), l)| vec![x, y, l]).collect()
}

/// Positive-weight edges over 4 nodes. Cycles are allowed: `min` still
/// converges because going around a cycle only lengthens a path.
fn weighted_edges_strategy() -> impl Strategy<Value = Vec<(i64, i64, i64)>> {
    prop::collection::vec((0i64..4, 0i64..4, 1i64..5), 0..8)
}

/// Positive-weight edges constrained to point FORWARD (x < y), so the graph is
/// acyclic and `max` has a finite fixpoint.
fn dag_edges_strategy() -> impl Strategy<Value = Vec<(i64, i64, i64)>> {
    prop::collection::vec((0i64..4, 0i64..4, 1i64..5), 0..8)
        .prop_map(|v| v.into_iter().filter(|(x, y, _)| x < y).collect())
}

/// Textbook congruence closure with a real union-find, recomputed from
/// scratch — the oracle the factored encoding must match exactly.
fn reference_congruence(
    nodes: &HashSet<(i64, i64, i64, i64)>,
    eqs: &HashSet<(i64, i64)>,
) -> HashSet<Vec<i64>> {
    let mut terms: HashSet<i64> = HashSet::new();
    for &(t, _, a, b) in nodes {
        terms.insert(t);
        terms.insert(a);
        terms.insert(b);
    }
    // Asserting an equation introduces its operands as terms (nullary
    // constants) — the same convention the program's `term` rules use.
    for &(x, y) in eqs {
        terms.insert(x);
        terms.insert(y);
    }
    let mut parent: HashMap<i64, i64> = terms.iter().map(|&t| (t, t)).collect();
    fn find(parent: &mut HashMap<i64, i64>, x: i64) -> i64 {
        let mut r = x;
        while parent[&r] != r {
            r = parent[&r];
        }
        let mut c = x;
        while parent[&c] != c {
            let next = parent[&c];
            parent.insert(c, r);
            c = next;
        }
        r
    }
    // Union toward the SMALLER representative so the class leader is its min,
    // matching merge(min).
    let mut union = |parent: &mut HashMap<i64, i64>, x: i64, y: i64| -> bool {
        let (rx, ry) = (find(parent, x), find(parent, y));
        if rx == ry {
            return false;
        }
        let (lo, hi) = if rx < ry { (rx, ry) } else { (ry, rx) };
        parent.insert(hi, lo);
        true
    };
    loop {
        let mut changed = false;
        for &(x, y) in eqs {
            if union(&mut parent, x, y) {
                changed = true;
            }
        }
        // Congruence: nodes sharing a canonical form join one class.
        let mut seen: HashMap<(i64, i64, i64), i64> = HashMap::new();
        let forms: Vec<(i64, (i64, i64, i64))> = nodes
            .iter()
            .map(|&(t, op, a, b)| (t, (op, find(&mut parent, a), find(&mut parent, b))))
            .collect();
        for (t, key) in forms {
            match seen.get(&key) {
                Some(&other) => {
                    if union(&mut parent, t, other) {
                        changed = true;
                    }
                }
                None => {
                    seen.insert(key, t);
                }
            }
        }
        if !changed {
            break;
        }
    }
    terms
        .iter()
        .map(|&t| {
            let r = find(&mut parent, t);
            vec![t, r]
        })
        .collect()
}

/// Term tables that MAY be cyclic — a term can be its own descendant. Used to
/// probe the one case where the encoding's answer is not forced.
fn egraph_cyclic_nodes_strategy() -> impl Strategy<Value = Vec<(i64, i64, i64, i64)>> {
    prop::collection::vec(
        (
            1i64..7,
            0i64..3,
            prop::sample::select(vec![-1i64, 1, 2, 3, 4, 5, 6]),
            prop::sample::select(vec![-1i64, 1, 2, 3, 4, 5, 6]),
        ),
        0..7,
    )
    .prop_map(|v| {
        let mut by_id: HashMap<i64, (i64, i64, i64, i64)> = HashMap::new();
        for tup in v {
            by_id.entry(tup.0).or_insert(tup);
        }
        by_id.into_values().collect()
    })
}

/// Small well-formed term DAGs: ids 1..6, ops 0..2. A child is either the `-1`
/// sentinel or a STRICTLY SMALLER id, so the node table is acyclic — terms are
/// built bottom-up, as they are when read out of a source program.
///
/// Cyclic node tables (a term that is its own descendant) are excluded
/// deliberately, and the reason is a real semantic boundary rather than a
/// generator convenience: there, the merge justifying `t = t'` can rest on
/// itself, and a destructive union-find keeps such a merge (it never
/// re-examines the justification) while this encoding drops it (differential
/// dataflow retracts facts with no well-founded support). See
/// `cyclic_terms_differ_from_union_find` and docs/retractable-egraph.md.
fn egraph_nodes_strategy() -> impl Strategy<Value = Vec<(i64, i64, i64, i64)>> {
    prop::collection::vec((1i64..7, 0i64..3, 0usize..7, 0usize..7), 0..7).prop_map(|v| {
        let mut by_id: HashMap<i64, (i64, i64, i64, i64)> = HashMap::new();
        for (t, op, a_pick, b_pick) in v {
            // Children range over {-1} ∪ {1..t-1}: index 0 is the sentinel.
            let child = |pick: usize| -> i64 {
                if t <= 1 {
                    return -1;
                }
                let choices = (t - 1) as usize; // ids 1..=t-1
                match pick % (choices + 1) {
                    0 => -1,
                    k => k as i64,
                }
            };
            by_id
                .entry(t)
                .or_insert((t, op, child(a_pick), child(b_pick)));
        }
        by_id.into_values().collect()
    })
}

fn egraph_eqs_strategy() -> impl Strategy<Value = Vec<(i64, i64)>> {
    prop::collection::vec((1i64..7, 1i64..7), 0..5)
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

/// lt = { (x, y) | edge(x,y) and x < y } — comparison filter.
fn reference_lt(edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    edges
        .iter()
        .filter(|(x, y)| x < y)
        .map(|&(x, y)| vec![x, y])
        .collect()
}

/// selfloop = { x | edge(x,x) } — equality comparison in the body.
fn reference_selfloop(edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    edges
        .iter()
        .filter(|(x, y)| x == y)
        .map(|&(x, _)| vec![x])
        .collect()
}

/// succ = { (x, y + 1) | edge(x,y) } — arithmetic in the head.
fn reference_succ(edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    edges.iter().map(|&(x, y)| vec![x, y + 1]).collect()
}

/// mk = { (x, y, min z) | triple(x,y,z) } — aggregation with a composite key.
fn reference_multikey_min(triples: &HashSet<(i64, i64, i64)>) -> HashSet<Vec<i64>> {
    let mut by_key: HashMap<(i64, i64), i64> = HashMap::new();
    for &(x, y, z) in triples {
        by_key
            .entry((x, y))
            .and_modify(|m| {
                if z < *m {
                    *m = z
                }
            })
            .or_insert(z);
    }
    by_key
        .into_iter()
        .map(|((x, y), m)| vec![x, y, m])
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

/// Count over a PROJECTED intermediate: `fa` folds away `id`, so rule
/// derivations give it multiplicities > 1. The aggregate must see the SET
/// (regression: streaming mode skips the thresholding inspector, so counts
/// used to inflate to the number of derivations).
const PROJ_COUNT_PROGRAM: &str = "\
.in
.decl t(id: number, f: number, a: number)
.input t.facts

.printsize
.decl fa(f: number, a: number)
.decl n(f: number, c: number)

.rule
fa(F, A) :- t(I, F, A).
n(F, count(A)) :- fa(F, A).
";

/// All-pairs shortest path via a LATTICE MERGE: `path` is declared a function
/// from (x, y) to a length folded by `min`, so both rules contribute plain
/// candidate values and the relation keeps one row per pair. Weights are
/// positive in the generator — `min` over unbounded integers has infinite
/// descending chains, so a negative cycle would not converge.
const MERGE_MIN_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number, len: number)
.input edge.facts

.printsize
.decl path(x: number, y: number, len: number) merge(min)

.rule
path(X, Y, L) :- edge(X, Y, L).
path(X, Z, L1 + L2) :- path(X, Y, L1), edge(Y, Z, L2).
";

/// The same graph folded by `max` instead: widest-detour lengths, bounded by
/// the acyclic generator (see `dag_edges_strategy`).
const MERGE_MAX_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number, len: number)
.input edge.facts

.printsize
.decl path(x: number, y: number, len: number) merge(max)

.rule
path(X, Y, L) :- edge(X, Y, L).
path(X, Z, L1 + L2) :- path(X, Y, L1), edge(Y, Z, L2).
";

/// A congruence closure carrying NO union-find.
///
/// The e-graph is factored the way `avg` factors into `sum`+`count`: the
/// mutable union-find (which destroys the provenance needed to un-merge) is
/// replaced by pieces that are each a monotone VIEW over the asserted facts —
///   `leader`  : term -> class representative, a merge(min) lattice fold over
///               the equation graph;
///   `cnode`   : each e-node with its children canonicalized (a view);
///   `form_rep`: the hash-cons — representative term per canonical form.
/// Congruence links every node to its form's representative, which is linear in
/// nodes rather than quadratic in class size. Nothing is mutated, so deleting
/// an asserted equation splits the classes again.
/// `-1` is the "no child" sentinel and is itself a term (its own class).
const EGRAPH_PROGRAM: &str = "\
.in
.decl node(t: number, op: number, a: number, b: number)
.input node.facts
.decl eq_input(x: number, y: number)
.input eq_input.facts

.printsize
.decl term(t: number)
.decl eq_edge(x: number, y: number)
.decl cnode(t: number, op: number, la: number, lb: number)
.decl form_rep(op: number, la: number, lb: number, rep: number) merge(min)

.out
.decl leader(t: number, rep: number) merge(min)

.rule
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

const LT_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl lt(x: number, y: number)

.rule
lt(X, Y) :- edge(X, Y), X < Y.
";

const SELFLOOP_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl selfloop(x: number)

.rule
selfloop(X) :- edge(X, Y), X = Y.
";

// Arithmetic in the head: y + 1.
const SUCC_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl succ(x: number, y: number)

.rule
succ(X, Y + 1) :- edge(X, Y).
";

// Aggregation grouped by a composite (x, y) key over a 3-arity relation.
const MULTIKEY_MIN_PROGRAM: &str = "\
.in
.decl triple(x: number, y: number, z: number)
.input triple.facts

.printsize
.decl mk(x: number, y: number, m: number)

.rule
mk(X, Y, min(Z)) :- triple(X, Y, Z).
";

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn edges_strategy() -> impl Strategy<Value = Vec<(i64, i64)>> {
    prop::collection::vec((0i64..5, 0i64..5), 0..9)
}

fn triples_strategy() -> impl Strategy<Value = Vec<(i64, i64, i64)>> {
    prop::collection::vec((0i64..4, 0i64..4, 0i64..4), 0..9)
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

    /// count over a projected (multiplicity-carrying) intermediate.
    #[test]
    fn batch_proj_count_matches_reference(triples in triples_strategy()) {
        let triple_set: HashSet<(i64, i64, i64)> = triples.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = triple_set.iter().map(|&(i, f, a)| vec![i, f, a]).collect();
        let got = run_batch(PROJ_COUNT_PROGRAM, &[("t", rows)]);
        prop_assert_eq!(got["n"].clone(), reference_proj_count(&triple_set));
    }

    /// `merge(min)`: relation-level lattice fold, one row per key.
    #[test]
    fn batch_merge_min_matches_reference(edges in weighted_edges_strategy()) {
        let edge_set: HashSet<(i64, i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = edge_set.iter().map(|&(x, y, l)| vec![x, y, l]).collect();
        let got = run_batch(MERGE_MIN_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["path"].clone(), reference_merge_path(&edge_set, "min"));
    }

    /// `merge(max)` over a DAG — the other lattice join.
    #[test]
    fn batch_merge_max_matches_reference(edges in dag_edges_strategy()) {
        let edge_set: HashSet<(i64, i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = edge_set.iter().map(|&(x, y, l)| vec![x, y, l]).collect();
        let got = run_batch(MERGE_MAX_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["path"].clone(), reference_merge_path(&edge_set, "max"));
    }

    /// A merge relation is a FUNCTION: never two rows for one key.
    #[test]
    fn batch_merge_is_functional(edges in weighted_edges_strategy()) {
        let edge_set: HashSet<(i64, i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = edge_set.iter().map(|&(x, y, l)| vec![x, y, l]).collect();
        let got = run_batch(MERGE_MIN_PROGRAM, &[("edge", rows)]);
        let mut keys: HashSet<(i64, i64)> = HashSet::new();
        for row in &got["path"] {
            prop_assert!(keys.insert((row[0], row[1])), "duplicate key {:?}", row);
        }
    }

    /// Does pointer jumping close the cyclic-term gap in general, or only on
    /// the one hand-picked example? Compares against the union-find oracle over
    /// term tables that are allowed to be cyclic.
    #[test]
    fn cyclic_tables_vs_union_find(
        nodes in egraph_cyclic_nodes_strategy(),
        eqs in egraph_eqs_strategy(),
    ) {
        let node_set: HashSet<(i64, i64, i64, i64)> = nodes.iter().cloned().collect();
        let eq_set: HashSet<(i64, i64)> = eqs.iter().cloned().collect();
        let node_rows: Vec<Vec<i64>> = node_set.iter().map(|&(t, o, a, b)| vec![t, o, a, b]).collect();
        let eq_rows: Vec<Vec<i64>> = eq_set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(EGRAPH_PROGRAM, &[("node", node_rows), ("eq_input", eq_rows)]);
        prop_assert_eq!(got["leader"].clone(), reference_congruence(&node_set, &eq_set));
    }

    /// A cycle CREATED BY AN EQUATION is fine: `f(a) = a` puts a node into the
    /// class of its own child, but the merge rests on an asserted base fact, so
    /// it keeps its support and matches the union-find exactly.
    #[test]
    fn equation_induced_cycles_match_union_find(_seed in 0i64..1) {
        // 1 = a (leaf), 3 = f(a). Assert a = f(a).
        let node_set: HashSet<(i64, i64, i64, i64)> =
            [(1, 0, -1, -1), (3, 1, 1, -1)].into_iter().collect();
        let eq_set: HashSet<(i64, i64)> = [(1, 3)].into_iter().collect();
        let node_rows: Vec<Vec<i64>> =
            node_set.iter().map(|&(t, o, a, b)| vec![t, o, a, b]).collect();
        let eq_rows: Vec<Vec<i64>> = eq_set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(EGRAPH_PROGRAM, &[("node", node_rows), ("eq_input", eq_rows)]);
        prop_assert_eq!(got["leader"].clone(), reference_congruence(&node_set, &eq_set));
    }

    /// The factored e-graph computes exactly the congruence closure a real
    /// union-find does.
    #[test]
    fn batch_egraph_matches_union_find(
        nodes in egraph_nodes_strategy(),
        eqs in egraph_eqs_strategy(),
    ) {
        let node_set: HashSet<(i64, i64, i64, i64)> = nodes.iter().cloned().collect();
        let eq_set: HashSet<(i64, i64)> = eqs.iter().cloned().collect();
        let node_rows: Vec<Vec<i64>> = node_set.iter().map(|&(t, o, a, b)| vec![t, o, a, b]).collect();
        let eq_rows: Vec<Vec<i64>> = eq_set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(EGRAPH_PROGRAM, &[("node", node_rows), ("eq_input", eq_rows)]);
        prop_assert_eq!(
            got["leader"].clone(),
            reference_congruence(&node_set, &eq_set)
        );
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

    /// `<` comparison filter in the body.
    #[test]
    fn batch_lt_matches_reference(edges in edges_strategy()) {
        let edge_set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = edge_set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(LT_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["lt"].clone(), reference_lt(&edge_set));
    }

    /// `=` comparison filter in the body (self-loops).
    #[test]
    fn batch_selfloop_matches_reference(edges in edges_strategy()) {
        let edge_set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = edge_set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(SELFLOOP_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["selfloop"].clone(), reference_selfloop(&edge_set));
    }

    /// arithmetic in the head (`y + 1`).
    #[test]
    fn batch_succ_matches_reference(edges in edges_strategy()) {
        let edge_set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = edge_set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(SUCC_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["succ"].clone(), reference_succ(&edge_set));
    }

    /// aggregation with a composite (2-column) group key over a 3-arity relation.
    #[test]
    fn batch_multikey_min_matches_reference(triples in triples_strategy()) {
        let set: HashSet<(i64, i64, i64)> = triples.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = set.iter().map(|&(x, y, z)| vec![x, y, z]).collect();
        let got = run_batch(MULTIKEY_MIN_PROGRAM, &[("triple", rows)]);
        prop_assert_eq!(got["mk"].clone(), reference_multikey_min(&set));
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

    /// projected-intermediate count, incrementally (multiplicities rise and
    /// fall as duplicate-projecting rows come and go).
    #[test]
    fn streaming_proj_count_equals_batch(triples in triples_strategy(), to_delete in triples_strategy()) {
        let inserted: HashSet<(i64, i64, i64)> = triples.iter().cloned().collect();
        let deleted: HashSet<(i64, i64, i64)> = to_delete
            .iter()
            .cloned()
            .filter(|x| inserted.contains(x))
            .collect();
        let ins: Vec<(&str, Vec<i64>)> = inserted.iter().map(|&(i, f, a)| ("t", vec![i, f, a])).collect();
        let del: Vec<(&str, Vec<i64>)> = deleted.iter().map(|&(i, f, a)| ("t", vec![i, f, a])).collect();
        let streamed = run_streaming(PROJ_COUNT_PROGRAM, &["t"], &ins, &del);
        let survivors: HashSet<(i64, i64, i64)> = inserted.difference(&deleted).cloned().collect();
        prop_assert_eq!(
            streamed.get("n").cloned().unwrap_or_default(),
            reference_proj_count(&survivors)
        );
    }

    /// `merge(min)` incrementally: deleting the edge that justified the current
    /// best must RAISE the merged value back (the reduce retracts and refolds).
    #[test]
    fn streaming_merge_min_equals_batch(edges in weighted_edges_strategy(), to_delete in weighted_edges_strategy()) {
        let inserted: HashSet<(i64, i64, i64)> = edges.iter().cloned().collect();
        let deleted: HashSet<(i64, i64, i64)> = to_delete
            .iter()
            .cloned()
            .filter(|x| inserted.contains(x))
            .collect();
        let ins: Vec<(&str, Vec<i64>)> =
            inserted.iter().map(|&(x, y, l)| ("edge", vec![x, y, l])).collect();
        let del: Vec<(&str, Vec<i64>)> =
            deleted.iter().map(|&(x, y, l)| ("edge", vec![x, y, l])).collect();
        let streamed = run_streaming(MERGE_MIN_PROGRAM, &["edge"], &ins, &del);
        let survivors: HashSet<(i64, i64, i64)> = inserted.difference(&deleted).cloned().collect();
        prop_assert_eq!(
            streamed.get("path").cloned().unwrap_or_default(),
            reference_merge_path(&survivors, "min")
        );
    }

    /// Retraction over CYCLIC term tables, against the union-find oracle: the
    /// hard case for both the semantics and the incremental machinery.
    #[test]
    fn streaming_cyclic_retraction_equals_union_find(
        nodes in egraph_cyclic_nodes_strategy(),
        eqs in egraph_eqs_strategy(),
        to_delete in egraph_eqs_strategy(),
    ) {
        let node_set: HashSet<(i64, i64, i64, i64)> = nodes.iter().cloned().collect();
        let inserted: HashSet<(i64, i64)> = eqs.iter().cloned().collect();
        let deleted: HashSet<(i64, i64)> = to_delete
            .iter()
            .cloned()
            .filter(|e| inserted.contains(e))
            .collect();
        let mut ins: Vec<(&str, Vec<i64>)> = node_set
            .iter()
            .map(|&(t, o, a, b)| ("node", vec![t, o, a, b]))
            .collect();
        ins.extend(inserted.iter().map(|&(x, y)| ("eq_input", vec![x, y])));
        let del: Vec<(&str, Vec<i64>)> = deleted
            .iter()
            .map(|&(x, y)| ("eq_input", vec![x, y]))
            .collect();
        let streamed = run_streaming(EGRAPH_PROGRAM, &["node", "eq_input"], &ins, &del);
        let survivors: HashSet<(i64, i64)> = inserted.difference(&deleted).cloned().collect();
        prop_assert_eq!(
            streamed.get("leader").cloned().unwrap_or_default(),
            reference_congruence(&node_set, &survivors)
        );
    }

    /// THE claim: retracting arbitrary asserted equations splits the classes
    /// back to exactly what a union-find would compute from scratch over the
    /// surviving equations — including undoing congruences those equations had
    /// cascaded into. A union-find cannot do this; nothing is mutated here, so
    /// the classes are simply re-derived.
    #[test]
    fn streaming_egraph_retraction_equals_rebuild(
        nodes in egraph_nodes_strategy(),
        eqs in egraph_eqs_strategy(),
        to_delete in egraph_eqs_strategy(),
    ) {
        let node_set: HashSet<(i64, i64, i64, i64)> = nodes.iter().cloned().collect();
        let inserted: HashSet<(i64, i64)> = eqs.iter().cloned().collect();
        let deleted: HashSet<(i64, i64)> = to_delete
            .iter()
            .cloned()
            .filter(|e| inserted.contains(e))
            .collect();

        let mut ins: Vec<(&str, Vec<i64>)> = node_set
            .iter()
            .map(|&(t, o, a, b)| ("node", vec![t, o, a, b]))
            .collect();
        ins.extend(inserted.iter().map(|&(x, y)| ("eq_input", vec![x, y])));
        let del: Vec<(&str, Vec<i64>)> = deleted
            .iter()
            .map(|&(x, y)| ("eq_input", vec![x, y]))
            .collect();

        let streamed = run_streaming(EGRAPH_PROGRAM, &["node", "eq_input"], &ins, &del);
        let survivors: HashSet<(i64, i64)> = inserted.difference(&deleted).cloned().collect();
        prop_assert_eq!(
            streamed.get("leader").cloned().unwrap_or_default(),
            reference_congruence(&node_set, &survivors)
        );
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

    /// `<` comparison filter, incrementally.
    #[test]
    fn streaming_lt_equals_batch(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let (s, b) = stream_vs_batch(LT_PROGRAM, "lt", "edge", None, &ins, &del);
        prop_assert_eq!(s, b);
    }

    /// `=` comparison filter, incrementally.
    #[test]
    fn streaming_selfloop_equals_batch(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let (s, b) = stream_vs_batch(SELFLOOP_PROGRAM, "selfloop", "edge", None, &ins, &del);
        prop_assert_eq!(s, b);
    }

    /// arithmetic in the head (`y + 1`), incrementally.
    #[test]
    fn streaming_succ_equals_batch(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let (s, b) = stream_vs_batch(SUCC_PROGRAM, "succ", "edge", None, &ins, &del);
        prop_assert_eq!(s, b);
    }

    /// composite-key aggregation over a 3-arity relation, incrementally.
    #[test]
    fn streaming_multikey_min_equals_batch(
        triples in triples_strategy(),
        to_delete in triples_strategy(),
    ) {
        let inserted: HashSet<(i64, i64, i64)> = triples.iter().cloned().collect();
        let deleted: HashSet<(i64, i64, i64)> = to_delete
            .iter()
            .cloned()
            .filter(|t| inserted.contains(t))
            .collect();
        let final_t: HashSet<(i64, i64, i64)> = inserted.difference(&deleted).cloned().collect();

        let ins: Vec<(&str, Vec<i64>)> =
            inserted.iter().map(|&(x, y, z)| ("triple", vec![x, y, z])).collect();
        let del: Vec<(&str, Vec<i64>)> =
            deleted.iter().map(|&(x, y, z)| ("triple", vec![x, y, z])).collect();
        let streamed = run_streaming(MULTIKEY_MIN_PROGRAM, &["triple"], &ins, &del);

        let rows: Vec<Vec<i64>> = final_t.iter().map(|&(x, y, z)| vec![x, y, z]).collect();
        let batch = run_batch(MULTIKEY_MIN_PROGRAM, &[("triple", rows)]);
        prop_assert_eq!(streamed["mk"].clone(), batch["mk"].clone());
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

// ---------------------------------------------------------------------------
// String / float column properties
//
// These exercise the in-engine string + float codec end to end: facts and
// output are raw text (no caller-side interning), so they verify that the
// engine itself encodes `string`/`float` columns on input and decodes them on
// output, batch and incrementally.
// ---------------------------------------------------------------------------

/// Read a decoded CSV (cells joined by ", ") into a set of text rows.
fn read_csv_text(dir: &Path, rel: &str, set: &mut HashSet<Vec<String>>) {
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
                    set.insert(line.split(", ").map(|s| s.to_string()).collect());
                }
            }
        }
    }
}

/// Batch-run a program whose columns may be `string`/`float`. Facts and output
/// are raw text; string literals in the program are encoded by the engine.
fn run_batch_typed(
    program_raw: &str,
    edbs: &[(&str, Vec<Vec<String>>)],
) -> HashMap<String, HashSet<Vec<String>>> {
    let dir = tempfile::tempdir().unwrap();
    let facts_dir = dir.path().join("facts");
    let out_dir = dir.path().join("out");
    std::fs::create_dir_all(&facts_dir).unwrap();
    std::fs::create_dir_all(out_dir.join("csvs")).unwrap();

    for (rel, rows) in edbs {
        let mut s = String::new();
        for row in rows {
            s.push_str(&row.join(","));
            s.push('\n');
        }
        std::fs::write(facts_dir.join(format!("{}.facts", rel)), s).unwrap();
    }

    let prog_path = dir.path().join("program.dl");
    std::fs::write(&prog_path, program_raw).unwrap();
    let mut program = syntax::parse(program_raw)
        .unwrap_or_else(|d| panic!("{}", syntax::render("program.dl", program_raw, &d, false)));
    program.map_constants(intern_text_literals);
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

    let mut result: HashMap<String, HashSet<Vec<String>>> = HashMap::new();
    for decl in program.idbs() {
        let mut set = HashSet::new();
        read_csv_text(&out_dir.join("csvs"), decl.name(), &mut set);
        result.insert(decl.name().to_string(), set);
    }
    result
}

/// Stream a program whose columns may be `string`/`float`. Input cells are raw
/// text encoded via the engine codec (per `edb_types`); output is the engine's
/// decoded text. Returns each IDB's final row set.
fn run_streaming_typed(
    program_raw: &str,
    edb_types: &[(&str, Vec<DataType>)],
    inserts: &[(&str, Vec<String>)],
    deletes: &[(&str, Vec<String>)],
) -> HashMap<String, HashSet<Vec<String>>> {
    let dir = tempfile::tempdir().unwrap();
    let facts_dir = dir.path().join("facts");
    std::fs::create_dir_all(&facts_dir).unwrap();

    let prog_path = dir.path().join("program.dl");
    std::fs::write(&prog_path, program_raw).unwrap();
    let mut program = syntax::parse(program_raw)
        .unwrap_or_else(|d| panic!("{}", syntax::render("program.dl", program_raw, &d, false)));
    program.map_constants(intern_text_literals);
    let strata = Strata::from_parser(program.clone());
    let plan = ProgramQueryPlan::from_strata(&strata, false, None);
    let fat = plan.should_use_fat_mode(false, KV_MAX, ROW_MAX);
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

    let types: HashMap<String, Vec<DataType>> = edb_types
        .iter()
        .map(|(n, t)| (n.to_string(), t.clone()))
        .collect();
    let encode = |rel: &str, row: &[String]| -> Vec<i64> {
        let t = &types[rel];
        row.iter()
            .enumerate()
            .map(|(i, cell)| reading::encode_token(cell, t[i]).unwrap())
            .collect()
    };

    // Output arrives as raw encoded i64; decode it here using each IDB's column
    // types (the engine now defers decoding to the consumer).
    let out_types: HashMap<String, Vec<DataType>> = program
        .idbs()
        .iter()
        .map(|d| {
            (
                d.name().to_string(),
                d.attributes().iter().map(|a| *a.data_type()).collect(),
            )
        })
        .collect();

    let (tx, rx) =
        crossbeam_channel::bounded::<(Arc<str>, smallvec::SmallVec<[i64; 8]>, isize)>(100_000);
    let acc: Arc<Mutex<HashMap<(String, Vec<String>), isize>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let acc_cb = Arc::clone(&acc);
    let output_callback: Arc<dyn Fn(&str, smallvec::SmallVec<[i64; 8]>, isize) + Send + Sync> =
        Arc::new(
            move |rel: &str, row: smallvec::SmallVec<[i64; 8]>, diff: isize| {
                let t = out_types.get(rel).map(|v| v.as_slice()).unwrap_or(&[]);
                let vals = reading::decode_cells_i64(&row, t);
                *acc_cb
                    .lock()
                    .unwrap()
                    .entry((rel.to_string(), vals))
                    .or_insert(0) += diff;
            },
        );

    let shutdown = Arc::new(AtomicBool::new(false));
    let cfg = StreamingConfig {
        input: rx,
        output_callback,
        shutdown: Arc::clone(&shutdown),
        output_seq: Arc::new(AtomicU64::new(0)),
        publish: HashSet::new(),
        commands: CommandLog::default(),
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

    for (rel, row) in inserts {
        tx.send((Arc::from(*rel), encode(rel, row).into(), 1))
            .unwrap();
    }
    std::thread::sleep(Duration::from_millis(400));
    for (rel, row) in deletes {
        tx.send((Arc::from(*rel), encode(rel, row).into(), -1))
            .unwrap();
    }
    std::thread::sleep(Duration::from_millis(400));

    shutdown.store(true, Ordering::Relaxed);
    drop(tx);
    handle.join().unwrap();

    let mut result: HashMap<String, HashSet<Vec<String>>> = HashMap::new();
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

const STR_DOG_PROGRAM: &str = "\
.in
.decl pet(name: string, kind: string)
.input pet.facts

.printsize
.decl dog(name: string)

.rule
dog(N) :- pet(N, \"dog\").
";

const STR_JOIN_PROGRAM: &str = "\
.in
.decl owns(owner: string, pet: string)
.input owns.facts
.decl likes(pet: string, food: string)
.input likes.facts

.printsize
.decl feeds(owner: string, food: string)

.rule
feeds(O, F) :- owns(O, P), likes(P, F).
";

const FLOAT_MIN_PROGRAM: &str = "\
.in
.decl sensor(name: string, v: float)
.input sensor.facts

.printsize
.decl lowest(name: string, m: float)

.rule
lowest(S, min(V)) :- sensor(S, V).
";

fn names() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["alice", "bob", "carol"]).prop_map(|s| s.to_string())
}
fn kinds() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["dog", "cat", "fish"]).prop_map(|s| s.to_string())
}
fn pets_strategy() -> impl Strategy<Value = Vec<(String, String)>> {
    prop::collection::vec((names(), kinds()), 0..8)
}
/// Floats whose textual form round-trips exactly (so reference == decoded).
fn floats() -> impl Strategy<Value = f64> {
    prop::sample::select(vec![0.0f64, 1.5, 2.25, -3.5, 4.0])
}
fn readings_strategy() -> impl Strategy<Value = Vec<(String, f64)>> {
    prop::collection::vec((names(), floats()), 0..8)
}

fn ref_dog(pets: &HashSet<(String, String)>) -> HashSet<Vec<String>> {
    pets.iter()
        .filter(|(_, k)| k == "dog")
        .map(|(n, _)| vec![n.clone()])
        .collect()
}
fn ref_feeds(
    owns: &HashSet<(String, String)>,
    likes: &HashSet<(String, String)>,
) -> HashSet<Vec<String>> {
    let mut out = HashSet::new();
    for (o, p) in owns {
        for (p2, f) in likes {
            if p == p2 {
                out.insert(vec![o.clone(), f.clone()]);
            }
        }
    }
    out
}
/// Per-sensor minimum, formatted exactly as the engine decodes floats.
fn ref_float_min(readings: &[(String, f64)]) -> HashSet<Vec<String>> {
    let mut by: HashMap<String, f64> = HashMap::new();
    for (s, v) in readings {
        by.entry(s.clone())
            .and_modify(|m| {
                if v < m {
                    *m = *v
                }
            })
            .or_insert(*v);
    }
    by.into_iter()
        .map(|(s, m)| vec![s, format!("{}", m)])
        .collect()
}

/// Dedup readings by (sensor, bit pattern), since the EDB is a set.
fn dedup_readings(rs: &[(String, f64)]) -> Vec<(String, f64)> {
    let mut seen = HashSet::new();
    rs.iter()
        .filter(|(s, v)| seen.insert((s.clone(), v.to_bits())))
        .cloned()
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// String column + string literal filter (batch).
    #[test]
    fn batch_string_filter(pets in pets_strategy()) {
        let set: HashSet<(String, String)> = pets.iter().cloned().collect();
        let rows: Vec<Vec<String>> = set.iter().map(|(n, k)| vec![n.clone(), k.clone()]).collect();
        let got = run_batch_typed(STR_DOG_PROGRAM, &[("pet", rows)]);
        prop_assert_eq!(got["dog"].clone(), ref_dog(&set));
    }

    /// Join on a string key across two string relations (batch).
    #[test]
    fn batch_string_join(
        owns in pets_strategy(),
        likes in pets_strategy(),
    ) {
        let owns_set: HashSet<(String, String)> = owns.iter().cloned().collect();
        let likes_set: HashSet<(String, String)> = likes.iter().cloned().collect();
        let owns_rows: Vec<Vec<String>> = owns_set.iter().map(|(a, b)| vec![a.clone(), b.clone()]).collect();
        let likes_rows: Vec<Vec<String>> = likes_set.iter().map(|(a, b)| vec![a.clone(), b.clone()]).collect();
        let got = run_batch_typed(STR_JOIN_PROGRAM, &[("owns", owns_rows), ("likes", likes_rows)]);
        prop_assert_eq!(got["feeds"].clone(), ref_feeds(&owns_set, &likes_set));
    }

    /// Float column + per-key float aggregation (batch).
    #[test]
    fn batch_float_min(readings in readings_strategy()) {
        let rs = dedup_readings(&readings);
        let rows: Vec<Vec<String>> = rs.iter().map(|(s, v)| vec![s.clone(), v.to_string()]).collect();
        let got = run_batch_typed(FLOAT_MIN_PROGRAM, &[("sensor", rows)]);
        prop_assert_eq!(got["lowest"].clone(), ref_float_min(&rs));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    /// String filter, incrementally (insert pets, delete a subset).
    #[test]
    fn streaming_string_filter(pets in pets_strategy(), to_delete in pets_strategy()) {
        let inserted: HashSet<(String, String)> = pets.iter().cloned().collect();
        let deleted: HashSet<(String, String)> =
            to_delete.iter().cloned().filter(|p| inserted.contains(p)).collect();
        let final_pets: HashSet<(String, String)> = inserted.difference(&deleted).cloned().collect();

        let ins: Vec<(&str, Vec<String>)> =
            inserted.iter().map(|(n, k)| ("pet", vec![n.clone(), k.clone()])).collect();
        let del: Vec<(&str, Vec<String>)> =
            deleted.iter().map(|(n, k)| ("pet", vec![n.clone(), k.clone()])).collect();
        let edb_types = [("pet", vec![DataType::String, DataType::String])];
        let streamed = run_streaming_typed(STR_DOG_PROGRAM, &edb_types, &ins, &del);

        prop_assert_eq!(streamed["dog"].clone(), ref_dog(&final_pets));
    }

    /// Float aggregation, incrementally (deletes can raise the per-key minimum).
    #[test]
    fn streaming_float_min(readings in readings_strategy(), to_delete in readings_strategy()) {
        let inserted = dedup_readings(&readings);
        let ins_keys: HashSet<(String, u64)> =
            inserted.iter().map(|(s, v)| (s.clone(), v.to_bits())).collect();
        let deleted: Vec<(String, f64)> = dedup_readings(&to_delete)
            .into_iter()
            .filter(|(s, v)| ins_keys.contains(&(s.clone(), v.to_bits())))
            .collect();
        let del_keys: HashSet<(String, u64)> =
            deleted.iter().map(|(s, v)| (s.clone(), v.to_bits())).collect();
        let final_rs: Vec<(String, f64)> = inserted
            .iter()
            .filter(|(s, v)| !del_keys.contains(&(s.clone(), v.to_bits())))
            .cloned()
            .collect();

        let ins: Vec<(&str, Vec<String>)> =
            inserted.iter().map(|(s, v)| ("sensor", vec![s.clone(), v.to_string()])).collect();
        let del: Vec<(&str, Vec<String>)> =
            deleted.iter().map(|(s, v)| ("sensor", vec![s.clone(), v.to_string()])).collect();
        let edb_types = [("sensor", vec![DataType::String, DataType::Float])];
        let streamed = run_streaming_typed(FLOAT_MIN_PROGRAM, &edb_types, &ins, &del);

        prop_assert_eq!(streamed["lowest"].clone(), ref_float_min(&final_rs));
    }
}

// ---------------------------------------------------------------------------
// Repeated head variable + negation (regression for the antijoin flatten gap)
//
// A rule like `r(X, X) :- item(X), !removed(X).` makes the antijoin reconstruct
// an output with MORE columns than its key (the head repeats X). The flatten
// codegen used to only cover output-arity <= key-arity and panicked
// ("codegen_k_flatten unimplemented for 1, 2"). These pin both the key-only (k)
// and key+value (kv) antijoin shapes.
// ---------------------------------------------------------------------------

const SELF_PAIR_PROGRAM: &str = "\
.in
.decl item(x: number)
.input item.facts
.decl removed(x: number)
.input removed.facts

.printsize
.decl kept_pair(x: number, y: number)

.rule
kept_pair(X, X) :- item(X), !removed(X).
";

const KV_DUP_PROGRAM: &str = "\
.in
.decl item(x: number, v: number)
.input item.facts
.decl removed(x: number)
.input removed.facts

.printsize
.decl kept(x: number, v: number, x2: number)

.rule
kept(X, V, X) :- item(X, V), !removed(X).
";

fn ref_self_pair(items: &HashSet<i64>, removed: &HashSet<i64>) -> HashSet<Vec<i64>> {
    items
        .iter()
        .filter(|x| !removed.contains(x))
        .map(|&x| vec![x, x])
        .collect()
}

fn ref_kv_dup(items: &HashSet<(i64, i64)>, removed: &HashSet<i64>) -> HashSet<Vec<i64>> {
    items
        .iter()
        .filter(|(x, _)| !removed.contains(x))
        .map(|&(x, v)| vec![x, v, x])
        .collect()
}

fn small_ints() -> impl Strategy<Value = Vec<i64>> {
    prop::collection::vec(0i64..6, 0..8)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// `r(X, X) :- item(X), !removed(X)` — repeated head var + key-only antijoin.
    #[test]
    fn batch_self_pair_negation(items in small_ints(), removed in small_ints()) {
        let item_set: HashSet<i64> = items.iter().cloned().collect();
        let removed_set: HashSet<i64> = removed.iter().cloned().collect();
        let got = run_batch(
            SELF_PAIR_PROGRAM,
            &[
                ("item", item_set.iter().map(|&x| vec![x]).collect()),
                ("removed", removed_set.iter().map(|&x| vec![x]).collect()),
            ],
        );
        prop_assert_eq!(got["kept_pair"].clone(), ref_self_pair(&item_set, &removed_set));
    }

    /// `r(X, V, X) :- item(X, V), !removed(X)` — repeated head var + kv antijoin.
    #[test]
    fn batch_kv_dup_negation(
        items in prop::collection::vec((0i64..6, 0i64..6), 0..8),
        removed in small_ints(),
    ) {
        let item_set: HashSet<(i64, i64)> = items.iter().cloned().collect();
        let removed_set: HashSet<i64> = removed.iter().cloned().collect();
        let got = run_batch(
            KV_DUP_PROGRAM,
            &[
                ("item", item_set.iter().map(|&(x, v)| vec![x, v]).collect()),
                ("removed", removed_set.iter().map(|&x| vec![x]).collect()),
            ],
        );
        prop_assert_eq!(got["kept"].clone(), ref_kv_dup(&item_set, &removed_set));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// Same, incrementally: items inserted then a subset deleted, plus toggling
    /// `removed`, must match a batch run over the survivors.
    #[test]
    fn streaming_self_pair_negation(items in small_ints(), removed in small_ints()) {
        let item_set: HashSet<i64> = items.iter().cloned().collect();
        let removed_set: HashSet<i64> = removed.iter().cloned().collect();

        let mut ins: Vec<(&str, Vec<i64>)> = item_set.iter().map(|&x| ("item", vec![x])).collect();
        ins.extend(removed_set.iter().map(|&x| ("removed", vec![x])));
        let streamed = run_streaming(SELF_PAIR_PROGRAM, &["item", "removed"], &ins, &[]);

        prop_assert_eq!(streamed["kept_pair"].clone(), ref_self_pair(&item_set, &removed_set));
    }
}

// ---------------------------------------------------------------------------
// Tier 1: recursive aggregation (connected-components min label).
// Combines recursion x aggregation — each had an independent incremental bug,
// so their interaction is the highest-risk untested combination.
// ---------------------------------------------------------------------------

const CC_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl cc(node: number, comp: number)

.rule
cc(N, min(N)) :- edge(N, _).
cc(N, min(C)) :- edge(O, N), cc(O, C).
";

/// Least-fixpoint of the CC program: a node with an out-edge starts labelled
/// with itself; every edge O->N propagates min(label(O)) to N.
fn reference_cc(edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    let mut cc: HashMap<i64, i64> = HashMap::new();
    for &(o, _) in edges {
        cc.entry(o).or_insert(o); // min(N) over a single N is N
    }
    loop {
        let mut next = cc.clone();
        for &(o, n) in edges {
            if let Some(&co) = cc.get(&o) {
                let e = next.entry(n).or_insert(co);
                if co < *e {
                    *e = co;
                }
            }
        }
        if next == cc {
            break;
        }
        cc = next;
    }
    cc.into_iter().map(|(n, c)| vec![n, c]).collect()
}

/// Recursive aggregation under the `isize` semiring. Previously unsound: the
/// aggregate ran inside the recursive fixpoint loop and superseded labels were
/// not retracted (CC kept a stale `cc(2,2)` for edges {(0,2),(2,0)}).
///
/// Fixed by a planner-level **stratum split** (`strata::rewrite`): a self-
/// recursive aggregated head `cc(N, min(C))` is desugared into an un-aggregated
/// recursive helper plus a downstream non-recursive aggregation — both of which
/// the engine handles correctly. The minimal repro that exposed the bug:
#[test]
fn recursive_aggregation_cc_regression() {
    let edges: HashSet<(i64, i64)> = [(0, 2), (2, 0)].into_iter().collect();
    let rows: Vec<Vec<i64>> = edges.iter().map(|&(x, y)| vec![x, y]).collect();
    let got = run_batch(CC_PROGRAM, &[("edge", rows)]);
    assert_eq!(
        got["cc"],
        reference_cc(&edges),
        "expected a single min label per node"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Recursive aggregation (connected components) matches the least-fixpoint
    /// reference over arbitrary graphs, batch.
    #[test]
    fn batch_cc_matches_reference(edges in edges_strategy()) {
        let set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(CC_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["cc"].clone(), reference_cc(&set));
    }

    /// Recursive aggregation also survives streaming insert-then-delete churn:
    /// the incrementally maintained result equals a batch run over the survivors.
    #[test]
    fn streaming_cc_matches_batch(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let (s, b) = stream_vs_batch(CC_PROGRAM, "cc", "edge", None, &ins, &del);
        prop_assert_eq!(s, b);
    }

    /// Stress the split CC path on larger, denser graphs (more nodes, longer
    /// cycles) than the default strategy.
    #[test]
    fn batch_cc_large_matches_reference(
        edges in prop::collection::vec((0i64..8, 0i64..8), 0..24)
    ) {
        let set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(CC_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["cc"].clone(), reference_cc(&set));
    }
}

// ---------------------------------------------------------------------------
// Mutually-recursive aggregation: two aggregated heads in one recursion cycle.
// The stratum split must redirect each helper to the *other's* helper so both
// aggregated heads leave the recursive SCC.
// ---------------------------------------------------------------------------

const MUTUAL_MIN_PROGRAM: &str = "\
.in
.decl seed(n: number, c: number)
.input seed.facts
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl a(n: number, c: number)
.decl b(n: number, c: number)

.rule
a(N, min(C)) :- seed(N, C).
a(N, min(C)) :- edge(N, M), b(M, C).
b(N, min(C)) :- edge(N, M), a(M, C).
";

/// Least fixpoint of the mutual min-propagation rules: `a` is seeded directly
/// and lowered by neighbours' `b`; `b` is lowered by neighbours' `a`.
fn ref_mutual_min(
    seed: &HashSet<(i64, i64)>,
    edges: &HashSet<(i64, i64)>,
) -> (HashSet<Vec<i64>>, HashSet<Vec<i64>>) {
    let mut a: HashMap<i64, i64> = HashMap::new();
    let mut b: HashMap<i64, i64> = HashMap::new();
    for &(n, c) in seed {
        let e = a.entry(n).or_insert(c);
        if c < *e {
            *e = c;
        }
    }
    loop {
        let mut na = a.clone();
        let mut nb = b.clone();
        for &(n, m) in edges {
            if let Some(&c) = b.get(&m) {
                let e = na.entry(n).or_insert(c);
                if c < *e {
                    *e = c;
                }
            }
            if let Some(&c) = a.get(&m) {
                let e = nb.entry(n).or_insert(c);
                if c < *e {
                    *e = c;
                }
            }
        }
        if na == a && nb == b {
            break;
        }
        a = na;
        b = nb;
    }
    (
        a.into_iter().map(|(n, c)| vec![n, c]).collect(),
        b.into_iter().map(|(n, c)| vec![n, c]).collect(),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Both aggregated heads of a mutual-recursion cycle match the least-fixpoint
    /// reference, batch.
    #[test]
    fn batch_mutual_min_matches_reference(
        edges in edges_strategy(),
        seed in edges_strategy(),
    ) {
        let edge_set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let seed_set: HashSet<(i64, i64)> = seed.iter().cloned().collect();
        let got = run_batch(MUTUAL_MIN_PROGRAM, &[
            ("edge", edge_set.iter().map(|&(x, y)| vec![x, y]).collect()),
            ("seed", seed_set.iter().map(|&(x, y)| vec![x, y]).collect()),
        ]);
        let (ra, rb) = ref_mutual_min(&seed_set, &edge_set);
        prop_assert_eq!(got["a"].clone(), ra);
        prop_assert_eq!(got["b"].clone(), rb);
    }
}

// ---------------------------------------------------------------------------
// Tier 2: multiple negations, and cartesian product (batch + streaming).
// ---------------------------------------------------------------------------

const MULTI_NEG_PROGRAM: &str = "\
.in
.decl node(x: number)
.input node.facts
.decl a(x: number)
.input a.facts
.decl b(x: number)
.input b.facts

.printsize
.decl r(x: number)

.rule
r(X) :- node(X), !a(X), !b(X).
";

fn ref_multi_neg(nodes: &[i64], a: &HashSet<i64>, b: &HashSet<i64>) -> HashSet<Vec<i64>> {
    nodes
        .iter()
        .filter(|x| !a.contains(x) && !b.contains(x))
        .map(|&x| vec![x])
        .collect()
}

const CARTESIAN_PROGRAM: &str = "\
.in
.decl a(x: number)
.input a.facts
.decl b(y: number)
.input b.facts

.printsize
.decl prod(x: number, y: number)

.rule
prod(X, Y) :- a(X), b(Y).
";

/// Cartesian whose head repeats variables: the output arity (5) exceeds the
/// combined input arity (2), which the cartesian codegen must support (its
/// arm space used to be filtered to `iv0 + iv1 >= target` and panicked here).
const CARTESIAN_WIDE_PROGRAM: &str = "\
.in
.decl a(x: number)
.input a.facts
.decl b(y: number)
.input b.facts

.printsize
.decl wide(x1: number, x2: number, y1: number, y2: number, x3: number)

.rule
wide(X, X, Y, Y, X) :- a(X), b(Y).
";

fn ref_cartesian(a: &HashSet<i64>, b: &HashSet<i64>) -> HashSet<Vec<i64>> {
    let mut out = HashSet::new();
    for &x in a {
        for &y in b {
            out.insert(vec![x, y]);
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn batch_multi_neg(a in small_ints(), b in small_ints()) {
        let nodes = all_nodes();
        let a_set: HashSet<i64> = a.iter().cloned().collect();
        let b_set: HashSet<i64> = b.iter().cloned().collect();
        let got = run_batch(MULTI_NEG_PROGRAM, &[
            ("node", nodes.iter().map(|&x| vec![x]).collect()),
            ("a", a_set.iter().map(|&x| vec![x]).collect()),
            ("b", b_set.iter().map(|&x| vec![x]).collect()),
        ]);
        prop_assert_eq!(got["r"].clone(), ref_multi_neg(&nodes, &a_set, &b_set));
    }

    #[test]
    fn batch_cartesian(a in small_ints(), b in small_ints()) {
        let a_set: HashSet<i64> = a.iter().cloned().collect();
        let b_set: HashSet<i64> = b.iter().cloned().collect();
        let got = run_batch(CARTESIAN_PROGRAM, &[
            ("a", a_set.iter().map(|&x| vec![x]).collect()),
            ("b", b_set.iter().map(|&x| vec![x]).collect()),
        ]);
        prop_assert_eq!(got["prod"].clone(), ref_cartesian(&a_set, &b_set));
    }

    #[test]
    fn batch_cartesian_wider_than_inputs(a in small_ints(), b in small_ints()) {
        let a_set: HashSet<i64> = a.iter().cloned().collect();
        let b_set: HashSet<i64> = b.iter().cloned().collect();
        let got = run_batch(CARTESIAN_WIDE_PROGRAM, &[
            ("a", a_set.iter().map(|&x| vec![x]).collect()),
            ("b", b_set.iter().map(|&x| vec![x]).collect()),
        ]);
        let expected: HashSet<Vec<i64>> = a_set
            .iter()
            .flat_map(|&x| b_set.iter().map(move |&y| vec![x, x, y, y, x]))
            .collect();
        prop_assert_eq!(got["wide"].clone(), expected);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    /// Retraction through two chained antijoins: insert a/b, delete subsets;
    /// deleting from a or b can re-derive r.
    #[test]
    fn streaming_multi_neg(a in small_ints(), b in small_ints(), da in small_ints(), db in small_ints()) {
        let nodes = all_nodes();
        let a_set: HashSet<i64> = a.iter().cloned().collect();
        let b_set: HashSet<i64> = b.iter().cloned().collect();
        let da_set: HashSet<i64> = da.iter().cloned().filter(|x| a_set.contains(x)).collect();
        let db_set: HashSet<i64> = db.iter().cloned().filter(|x| b_set.contains(x)).collect();

        let mut ins: Vec<(&str, Vec<i64>)> = nodes.iter().map(|&x| ("node", vec![x])).collect();
        ins.extend(a_set.iter().map(|&x| ("a", vec![x])));
        ins.extend(b_set.iter().map(|&x| ("b", vec![x])));
        let mut del: Vec<(&str, Vec<i64>)> = da_set.iter().map(|&x| ("a", vec![x])).collect();
        del.extend(db_set.iter().map(|&x| ("b", vec![x])));
        let streamed = run_streaming(MULTI_NEG_PROGRAM, &["node", "a", "b"], &ins, &del);

        let fa: HashSet<i64> = a_set.difference(&da_set).cloned().collect();
        let fb: HashSet<i64> = b_set.difference(&db_set).cloned().collect();
        prop_assert_eq!(streamed["r"].clone(), ref_multi_neg(&nodes, &fa, &fb));
    }

    /// Cartesian product, incrementally as both sides change.
    #[test]
    fn streaming_cartesian(a in small_ints(), b in small_ints(), da in small_ints()) {
        let a_set: HashSet<i64> = a.iter().cloned().collect();
        let b_set: HashSet<i64> = b.iter().cloned().collect();
        let da_set: HashSet<i64> = da.iter().cloned().filter(|x| a_set.contains(x)).collect();

        let mut ins: Vec<(&str, Vec<i64>)> = a_set.iter().map(|&x| ("a", vec![x])).collect();
        ins.extend(b_set.iter().map(|&x| ("b", vec![x])));
        let del: Vec<(&str, Vec<i64>)> = da_set.iter().map(|&x| ("a", vec![x])).collect();
        let streamed = run_streaming(CARTESIAN_PROGRAM, &["a", "b"], &ins, &del);

        let fa: HashSet<i64> = a_set.difference(&da_set).cloned().collect();
        prop_assert_eq!(streamed["prod"].clone(), ref_cartesian(&fa, &b_set));
    }
}

// ---------------------------------------------------------------------------
// Tier 2/3: remaining comparison operators (>=, <=, >, !=).
// ---------------------------------------------------------------------------

fn cmp_program(op: &str, idb: &str) -> String {
    format!(
        ".in\n.decl edge(x: number, y: number)\n.input edge.facts\n\n\
         .printsize\n.decl {idb}(x: number, y: number)\n\n\
         .rule\n{idb}(X, Y) :- edge(X, Y), X {op} Y.\n"
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn batch_compare_ops(edges in edges_strategy()) {
        let set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = set.iter().map(|&(x, y)| vec![x, y]).collect();
        for (op, idb, keep) in [
            (">=", "ge", (|x: i64, y: i64| x >= y) as fn(i64, i64) -> bool),
            ("<=", "le", (|x, y| x <= y) as fn(i64, i64) -> bool),
            (">", "gt", (|x, y| x > y) as fn(i64, i64) -> bool),
            ("!=", "ne", (|x, y| x != y) as fn(i64, i64) -> bool),
        ] {
            let got = run_batch(&cmp_program(op, idb), &[("edge", rows.clone())]);
            let want: HashSet<Vec<i64>> =
                set.iter().filter(|&&(x, y)| keep(x, y)).map(|&(x, y)| vec![x, y]).collect();
            prop_assert_eq!(got[idb].clone(), want, "operator {}", op);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    /// `!=` filter, incrementally.
    #[test]
    fn streaming_ne(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let prog = cmp_program("!=", "ne");
        let (s, b) = stream_vs_batch(&prog, "ne", "edge", None, &ins, &del);
        prop_assert_eq!(s, b);
    }
}

// ---------------------------------------------------------------------------
// Tier 3: projection / column reordering.
// ---------------------------------------------------------------------------

const REORDER_PROGRAM: &str = "\
.in
.decl t(x: number, y: number, z: number)
.input t.facts

.printsize
.decl rev(z: number, x: number)

.rule
rev(Z, X) :- t(X, Y, Z).
";

fn ref_reorder(triples: &HashSet<(i64, i64, i64)>) -> HashSet<Vec<i64>> {
    triples.iter().map(|&(x, _, z)| vec![z, x]).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    #[test]
    fn batch_reorder(triples in triples_strategy()) {
        let set: HashSet<(i64, i64, i64)> = triples.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = set.iter().map(|&(x, y, z)| vec![x, y, z]).collect();
        let got = run_batch(REORDER_PROGRAM, &[("t", rows)]);
        prop_assert_eq!(got["rev"].clone(), ref_reorder(&set));
    }

    #[test]
    fn streaming_reorder(triples in triples_strategy(), to_delete in triples_strategy()) {
        let inserted: HashSet<(i64, i64, i64)> = triples.iter().cloned().collect();
        let deleted: HashSet<(i64, i64, i64)> =
            to_delete.iter().cloned().filter(|t| inserted.contains(t)).collect();
        let final_t: HashSet<(i64, i64, i64)> = inserted.difference(&deleted).cloned().collect();
        let ins: Vec<(&str, Vec<i64>)> =
            inserted.iter().map(|&(x, y, z)| ("t", vec![x, y, z])).collect();
        let del: Vec<(&str, Vec<i64>)> =
            deleted.iter().map(|&(x, y, z)| ("t", vec![x, y, z])).collect();
        let streamed = run_streaming(REORDER_PROGRAM, &["t"], &ins, &del);
        prop_assert_eq!(streamed["rev"].clone(), ref_reorder(&final_t));
    }
}

// ---------------------------------------------------------------------------
// Tier 3: NULL semantics (division by zero -> NULL; comparison with NULL).
// Uses the typed (text) harness so NULL renders/decodes as "NULL".
// ---------------------------------------------------------------------------

const DIV_PROGRAM: &str = "\
.in
.decl t(x: number, y: number, z: number)
.input t.facts

.printsize
.decl q(x: number, r: number)

.rule
q(X, Y / Z) :- t(X, Y, Z).
";

const NULLCMP_PROGRAM: &str = "\
.in
.decl t(x: number, v: number)
.input t.facts

.printsize
.decl big(x: number)

.rule
big(X) :- t(X, V), V > 2.
";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Division by zero yields NULL; otherwise integer division.
    #[test]
    fn batch_div_by_zero_null(
        triples in prop::collection::vec((0i64..5, 0i64..6, 0i64..4), 0..8),
    ) {
        let set: HashSet<(i64, i64, i64)> = triples.iter().cloned().collect();
        let rows: Vec<Vec<String>> =
            set.iter().map(|&(x, y, z)| vec![x.to_string(), y.to_string(), z.to_string()]).collect();
        let got = run_batch_typed(DIV_PROGRAM, &[("t", rows)]);
        let want: HashSet<Vec<String>> = set
            .iter()
            .map(|&(x, y, z)| {
                let r = if z == 0 { "NULL".to_string() } else { (y / z).to_string() };
                vec![x.to_string(), r]
            })
            .collect();
        prop_assert_eq!(got["q"].clone(), want);
    }

    /// A comparison whose operand is NULL is false (SQL-like). NULLs injected as
    /// empty fields.
    #[test]
    fn batch_compare_with_null(
        rows in prop::collection::vec((0i64..5, prop::option::of(0i64..6)), 0..8),
    ) {
        let set: HashSet<(i64, Option<i64>)> = rows.iter().cloned().collect();
        let facts: Vec<Vec<String>> = set
            .iter()
            .map(|(x, v)| vec![x.to_string(), v.map(|n| n.to_string()).unwrap_or_default()])
            .collect();
        let got = run_batch_typed(NULLCMP_PROGRAM, &[("t", facts)]);
        let want: HashSet<Vec<String>> = set
            .iter()
            .filter_map(|&(x, v)| match v {
                Some(n) if n > 2 => Some(vec![x.to_string()]),
                _ => None,
            })
            .collect();
        prop_assert_eq!(got["big"].clone(), want);
    }
}

// ---------------------------------------------------------------------------
// Wider coverage: multi-way joins, aggregation-over-join, self-antijoin,
// mutual recursion. (batch + streaming==batch)
// ---------------------------------------------------------------------------

const TRIANGLE_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl tri(x: number, y: number, z: number)

.rule
tri(X, Y, Z) :- edge(X, Y), edge(Y, Z), edge(Z, X).
";

const PATH3_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl p3(x: number, w: number)

.rule
p3(X, W) :- edge(X, Y), edge(Y, Z), edge(Z, W).
";

// min aggregation whose body is a 2-hop join (min is dup-insensitive, so the
// reference is unambiguous regardless of how the engine dedups join bindings).
const MIN_OVER_JOIN_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl m(z: number, lo: number)

.rule
m(Z, min(X)) :- edge(X, Y), edge(Y, Z).
";

// antijoin against the *same* relation: edges with no reverse.
const ONEWAY_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl oneway(x: number, y: number)

.rule
oneway(X, Y) :- edge(X, Y), !edge(Y, X).
";

// mutual recursion: a/b alternate over edges from `start`.
const MUTUAL_PROGRAM: &str = "\
.in
.decl start(x: number)
.input start.facts
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl a(x: number)
.decl b(x: number)

.rule
a(X) :- start(X).
b(Y) :- a(X), edge(X, Y).
a(Y) :- b(X), edge(X, Y).
";

fn ref_triangle(edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    let mut out = HashSet::new();
    for &(x, y) in edges {
        for &(y2, z) in edges {
            if y == y2 && edges.contains(&(z, x)) {
                out.insert(vec![x, y, z]);
            }
        }
    }
    out
}

fn ref_path3(edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    let mut out = HashSet::new();
    for &(x, y) in edges {
        for &(y2, z) in edges {
            if y != y2 {
                continue;
            }
            for &(z2, w) in edges {
                if z == z2 {
                    out.insert(vec![x, w]);
                }
            }
        }
    }
    out
}

fn ref_min_over_join(edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    let mut by: HashMap<i64, i64> = HashMap::new();
    for &(x, y) in edges {
        for &(y2, z) in edges {
            if y == y2 {
                by.entry(z)
                    .and_modify(|m| {
                        if x < *m {
                            *m = x
                        }
                    })
                    .or_insert(x);
            }
        }
    }
    by.into_iter().map(|(z, lo)| vec![z, lo]).collect()
}

fn ref_oneway(edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    edges
        .iter()
        .filter(|&&(x, y)| !edges.contains(&(y, x)))
        .map(|&(x, y)| vec![x, y])
        .collect()
}

/// (a, b): nodes reachable from `start` at even / odd distance (parity-aware,
/// so cyclic graphs can place a node in both).
fn ref_mutual(edges: &HashSet<(i64, i64)>, start: i64) -> (HashSet<Vec<i64>>, HashSet<Vec<i64>>) {
    let mut seen: HashSet<(i64, bool)> = HashSet::new();
    seen.insert((start, false));
    loop {
        let snap: Vec<(i64, bool)> = seen.iter().cloned().collect();
        let mut added = false;
        for (n, par) in snap {
            for &(o, m) in edges {
                if o == n && seen.insert((m, !par)) {
                    added = true;
                }
            }
        }
        if !added {
            break;
        }
    }
    let a = seen
        .iter()
        .filter(|(_, p)| !p)
        .map(|&(n, _)| vec![n])
        .collect();
    let b = seen
        .iter()
        .filter(|(_, p)| *p)
        .map(|&(n, _)| vec![n])
        .collect();
    (a, b)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn batch_triangle(edges in edges_strategy()) {
        let set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(TRIANGLE_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["tri"].clone(), ref_triangle(&set));
    }

    #[test]
    fn batch_path3(edges in edges_strategy()) {
        let set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(PATH3_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["p3"].clone(), ref_path3(&set));
    }

    #[test]
    fn batch_min_over_join(edges in edges_strategy()) {
        let set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(MIN_OVER_JOIN_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["m"].clone(), ref_min_over_join(&set));
    }

    #[test]
    fn batch_oneway(edges in edges_strategy()) {
        let set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(ONEWAY_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["oneway"].clone(), ref_oneway(&set));
    }

    #[test]
    fn batch_mutual(edges in edges_strategy()) {
        let set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(MUTUAL_PROGRAM, &[("start", vec![vec![0]]), ("edge", rows)]);
        let (a, b) = ref_mutual(&set, 0);
        prop_assert_eq!(got["a"].clone(), a);
        prop_assert_eq!(got["b"].clone(), b);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    #[test]
    fn streaming_triangle(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let (s, b) = stream_vs_batch(TRIANGLE_PROGRAM, "tri", "edge", None, &ins, &del);
        prop_assert_eq!(s, b);
    }

    #[test]
    fn streaming_min_over_join(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let (s, b) = stream_vs_batch(MIN_OVER_JOIN_PROGRAM, "m", "edge", None, &ins, &del);
        prop_assert_eq!(s, b);
    }

    /// Self-antijoin incrementally: deleting an edge can ADD a one-way edge (its
    /// reverse's antijoin partner disappears).
    #[test]
    fn streaming_oneway(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let (s, b) = stream_vs_batch(ONEWAY_PROGRAM, "oneway", "edge", None, &ins, &del);
        prop_assert_eq!(s, b);
    }

    /// Mutual recursion incrementally (check `a`).
    #[test]
    fn streaming_mutual_a(edges in edges_strategy(), to_delete in edges_strategy()) {
        let inserted: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let deleted: HashSet<(i64, i64)> =
            to_delete.iter().cloned().filter(|e| inserted.contains(e)).collect();
        let final_e: HashSet<(i64, i64)> = inserted.difference(&deleted).cloned().collect();

        let mut ins: Vec<(&str, Vec<i64>)> = vec![("start", vec![0])];
        ins.extend(inserted.iter().map(|&(x, y)| ("edge", vec![x, y])));
        let del: Vec<(&str, Vec<i64>)> = deleted.iter().map(|&(x, y)| ("edge", vec![x, y])).collect();
        let streamed = run_streaming(MUTUAL_PROGRAM, &["start", "edge"], &ins, &del);

        let (a, _b) = ref_mutual(&final_e, 0);
        prop_assert_eq!(streamed["a"].clone(), a);
    }
}

// ---------------------------------------------------------------------------
// Wider coverage 2: negation feeding recursion, comparison inside recursion,
// and a 4-arity relation. (batch + streaming==batch)
// ---------------------------------------------------------------------------

const ALLOWED_REACH_PROGRAM: &str = "\
.in
.decl node(x: number)
.input node.facts
.decl banned(x: number)
.input banned.facts
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl allowed(x: number)
.decl reach(x: number)

.rule
allowed(X) :- node(X), !banned(X).
reach(Y) :- edge(0, Y), allowed(Y).
reach(Y) :- reach(X), edge(X, Y), allowed(Y).
";

// comparison filter inside a recursive rule.
const BOUNDED_TC_PROGRAM: &str = "\
.in
.decl edge(x: number, y: number)
.input edge.facts

.printsize
.decl tcb(x: number, y: number)

.rule
tcb(X, Y) :- edge(X, Y), X < Y.
tcb(X, Y) :- tcb(X, Z), edge(Z, Y), X < Y.
";

// 4-arity relation with an equality filter on the middle columns.
const ARITY4_PROGRAM: &str = "\
.in
.decl t(a: number, b: number, c: number, d: number)
.input t.facts

.printsize
.decl q(a: number, d: number)

.rule
q(A, D) :- t(A, B, C, D), B = C.
";

/// reach from `start` stepping only through `allowed` (= node and not banned).
fn ref_allowed_reach(
    nodes: &[i64],
    banned: &HashSet<i64>,
    edges: &HashSet<(i64, i64)>,
    start: i64,
) -> HashSet<Vec<i64>> {
    let allowed: HashSet<i64> = nodes
        .iter()
        .filter(|n| !banned.contains(n))
        .cloned()
        .collect();
    let mut reach: HashSet<i64> = HashSet::new();
    for &(o, y) in edges {
        if o == start && allowed.contains(&y) {
            reach.insert(y);
        }
    }
    loop {
        let snap: Vec<i64> = reach.iter().cloned().collect();
        let mut added = false;
        for x in snap {
            for &(o, y) in edges {
                if o == x && allowed.contains(&y) && reach.insert(y) {
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

/// Mirrors the rule fixpoint, applying `X < Y` at *every* step (NOT the same as
/// transitive-closure-then-filter: a pair can only extend a `tcb(X,Z)` that
/// itself satisfied `X < Z`).
fn ref_bounded_tc(edges: &HashSet<(i64, i64)>) -> HashSet<Vec<i64>> {
    let mut tcb: HashSet<(i64, i64)> = edges.iter().filter(|&&(x, y)| x < y).cloned().collect();
    loop {
        let snap: Vec<(i64, i64)> = tcb.iter().cloned().collect();
        let mut added = false;
        for (x, z) in snap {
            for &(z2, y) in edges {
                if z == z2 && x < y && tcb.insert((x, y)) {
                    added = true;
                }
            }
        }
        if !added {
            break;
        }
    }
    tcb.into_iter().map(|(x, y)| vec![x, y]).collect()
}

fn ref_arity4(quads: &HashSet<(i64, i64, i64, i64)>) -> HashSet<Vec<i64>> {
    quads
        .iter()
        .filter(|&&(_, b, c, _)| b == c)
        .map(|&(a, _, _, d)| vec![a, d])
        .collect()
}

fn quads_strategy() -> impl Strategy<Value = Vec<(i64, i64, i64, i64)>> {
    prop::collection::vec((0i64..3, 0i64..3, 0i64..3, 0i64..3), 0..8)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn batch_allowed_reach(edges in edges_strategy(), banned in small_ints()) {
        let nodes = all_nodes();
        let edge_set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let banned_set: HashSet<i64> = banned.iter().cloned().collect();
        let got = run_batch(ALLOWED_REACH_PROGRAM, &[
            ("node", nodes.iter().map(|&x| vec![x]).collect()),
            ("banned", banned_set.iter().map(|&x| vec![x]).collect()),
            ("edge", edge_set.iter().map(|&(x, y)| vec![x, y]).collect()),
        ]);
        prop_assert_eq!(got["reach"].clone(), ref_allowed_reach(&nodes, &banned_set, &edge_set, 0));
    }

    #[test]
    fn batch_bounded_tc(edges in edges_strategy()) {
        let set: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = set.iter().map(|&(x, y)| vec![x, y]).collect();
        let got = run_batch(BOUNDED_TC_PROGRAM, &[("edge", rows)]);
        prop_assert_eq!(got["tcb"].clone(), ref_bounded_tc(&set));
    }

    #[test]
    fn batch_arity4(quads in quads_strategy()) {
        let set: HashSet<(i64, i64, i64, i64)> = quads.iter().cloned().collect();
        let rows: Vec<Vec<i64>> = set.iter().map(|&(a, b, c, d)| vec![a, b, c, d]).collect();
        let got = run_batch(ARITY4_PROGRAM, &[("t", rows)]);
        prop_assert_eq!(got["q"].clone(), ref_arity4(&set));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    /// Comparison inside recursion, incrementally.
    #[test]
    fn streaming_bounded_tc(edges in edges_strategy(), to_delete in edges_strategy()) {
        let (ins, del) = ins_del(&edges, &to_delete);
        let (s, b) = stream_vs_batch(BOUNDED_TC_PROGRAM, "tcb", "edge", None, &ins, &del);
        prop_assert_eq!(s, b);
    }

    /// Negation feeding recursion, incrementally (edge churn).
    #[test]
    fn streaming_allowed_reach(edges in edges_strategy(), to_delete in edges_strategy(), banned in small_ints()) {
        let nodes = all_nodes();
        let inserted: HashSet<(i64, i64)> = edges.iter().cloned().collect();
        let deleted: HashSet<(i64, i64)> =
            to_delete.iter().cloned().filter(|e| inserted.contains(e)).collect();
        let final_e: HashSet<(i64, i64)> = inserted.difference(&deleted).cloned().collect();
        let banned_set: HashSet<i64> = banned.iter().cloned().collect();

        let mut ins: Vec<(&str, Vec<i64>)> = nodes.iter().map(|&x| ("node", vec![x])).collect();
        ins.extend(banned_set.iter().map(|&x| ("banned", vec![x])));
        ins.extend(inserted.iter().map(|&(x, y)| ("edge", vec![x, y])));
        let del: Vec<(&str, Vec<i64>)> = deleted.iter().map(|&(x, y)| ("edge", vec![x, y])).collect();
        let streamed = run_streaming(ALLOWED_REACH_PROGRAM, &["node", "banned", "edge"], &ins, &del);

        prop_assert_eq!(streamed["reach"].clone(), ref_allowed_reach(&nodes, &banned_set, &final_e, 0));
    }

    /// 4-arity relation, incrementally.
    #[test]
    fn streaming_arity4(quads in quads_strategy(), to_delete in quads_strategy()) {
        let inserted: HashSet<(i64, i64, i64, i64)> = quads.iter().cloned().collect();
        let deleted: HashSet<(i64, i64, i64, i64)> =
            to_delete.iter().cloned().filter(|t| inserted.contains(t)).collect();
        let final_q: HashSet<(i64, i64, i64, i64)> = inserted.difference(&deleted).cloned().collect();
        let ins: Vec<(&str, Vec<i64>)> =
            inserted.iter().map(|&(a, b, c, d)| ("t", vec![a, b, c, d])).collect();
        let del: Vec<(&str, Vec<i64>)> =
            deleted.iter().map(|&(a, b, c, d)| ("t", vec![a, b, c, d])).collect();
        let streamed = run_streaming(ARITY4_PROGRAM, &["t"], &ins, &del);
        prop_assert_eq!(streamed["q"].clone(), ref_arity4(&final_q));
    }
}

// ---------------------------------------------------------------------------
// Head arithmetic over a shared body must not collide.
//
// Two rules with identical bodies but different head expressions previously got
// the same `HeadArith(<body>)` collection signature and one silently overwrote
// the other. Regression for that fix.
// ---------------------------------------------------------------------------

const HEAD_ARITH_COLLIDE_PROGRAM: &str = "\
.in
.decl e(x: number, y: number)
.input e.facts

.printsize
.decl a(x: number, z: number)
.decl b(x: number, z: number)

.rule
a(X, Y + 1) :- e(X, Y).
b(X, Y + 2) :- e(X, Y).
";

#[test]
fn head_arith_distinct_heads_same_body() {
    let got = run_batch(
        HEAD_ARITH_COLLIDE_PROGRAM,
        &[("e", vec![vec![1, 5], vec![2, 9]])],
    );
    assert_eq!(
        got["a"],
        [vec![1, 6], vec![2, 10]]
            .into_iter()
            .collect::<HashSet<_>>()
    );
    assert_eq!(
        got["b"],
        [vec![1, 7], vec![2, 11]]
            .into_iter()
            .collect::<HashSet<_>>()
    );
}

// ---------------------------------------------------------------------------
// String builtins: split_nth (value), starts_with / contains / str_before
// (boolean, used as `f(..) = 1`), over string columns.
// ---------------------------------------------------------------------------

const STRING_BUILTINS_PROGRAM: &str = "\
.in
.decl p(path: string)
.input p.facts

.printsize
.decl seg0(path: string, s: string)
.decl seg1(path: string, s: string)
.decl repl(path: string, s: string)
.decl pre(path: string)
.decl has(path: string)
.decl lt(path: string)

.rule
seg0(P, split_nth(P, \"/\", 0)) :- p(P).
seg1(P, split_nth(P, \"/\", 1)) :- p(P).
repl(P, replace(P, \"/\", \"_\")) :- p(P).
pre(P) :- p(P), starts_with(P, \"alpha/\") = 1.
has(P) :- p(P), contains(P, \"x\") = 1.
lt(P) :- p(P), str_before(P, \"beta\") = 1.
";

fn sset(rows: &[&[&str]]) -> HashSet<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect()
}

#[test]
fn string_builtins_match_expected() {
    let got = run_batch_typed(
        STRING_BUILTINS_PROGRAM,
        &[(
            "p",
            vec![vec!["alpha/x".to_string()], vec!["beta/y".to_string()]],
        )],
    );
    // split_nth: per-index segments, no cross-contamination between the two rules.
    assert_eq!(
        got["seg0"],
        sset(&[&["alpha/x", "alpha"], &["beta/y", "beta"]])
    );
    assert_eq!(got["seg1"], sset(&[&["alpha/x", "x"], &["beta/y", "y"]]));
    // replace: every separator rewritten.
    assert_eq!(
        got["repl"],
        sset(&[&["alpha/x", "alpha_x"], &["beta/y", "beta_y"]])
    );
    // starts_with / contains / str_before as `= 1` filters.
    assert_eq!(got["pre"], sset(&[&["alpha/x"]]));
    assert_eq!(got["has"], sset(&[&["alpha/x"]]));
    assert_eq!(got["lt"], sset(&[&["alpha/x"]]));
}

// ---------------------------------------------------------------------------
// Late-added queries (true incremental dataflow extension)
// ---------------------------------------------------------------------------
//
// A query added to a RUNNING engine imports the base program's published
// relations as traces: it must see the full pre-add history (with correct
// multiplicities) and then follow live updates, exactly as if its rules had
// been part of the program from the start. Each property compares the
// late-added query's final output against a batch run of the combined
// program over the final facts.

/// Base program publishing `publish`; stream `phase1`, add `query_dl` at
/// runtime, optionally drop it again, stream `phase2`, and return the QUERY's
/// IDB sets (rows with net positive multiplicity).
#[allow(clippy::too_many_arguments)]
fn run_streaming_with_late_query(
    base_dl: &str,
    publish: &[&str],
    query_dl: &str,
    phase1: &[(&str, Vec<i64>, isize)],
    phase2: &[(&str, Vec<i64>, isize)],
    drop_before_phase2: bool,
    workers: usize,
) -> HashMap<String, HashSet<Vec<i64>>> {
    let dir = tempfile::tempdir().unwrap();
    let facts_dir = dir.path().join("facts");
    std::fs::create_dir_all(&facts_dir).unwrap();
    let prog_path = dir.path().join("program.dl");
    std::fs::write(&prog_path, base_dl).unwrap();

    let (base_prog, strata, plan, fat) = build(base_dl);
    for decl in base_prog.edbs() {
        std::fs::write(facts_dir.join(format!("{}.facts", decl.name())), "").unwrap();
    }
    let idb_map = aggregation_catalog_from_program(&base_prog);
    let args = Args::new(
        prog_path.to_string_lossy().into_owned(),
        facts_dir.to_string_lossy().into_owned(),
        None,
        ",".to_string(),
        workers,
    );

    let (tx, rx) =
        crossbeam_channel::bounded::<(Arc<str>, smallvec::SmallVec<[i64; 8]>, isize)>(100_000);
    let base_callback: Arc<dyn Fn(&str, smallvec::SmallVec<[i64; 8]>, isize) + Send + Sync> =
        Arc::new(|_, _, _| {});
    let shutdown = Arc::new(AtomicBool::new(false));
    let commands = CommandLog::default();
    let cfg = StreamingConfig {
        input: rx,
        output_callback: base_callback,
        shutdown: Arc::clone(&shutdown),
        output_seq: Arc::new(AtomicU64::new(0)),
        publish: publish.iter().map(|s| s.to_string()).collect(),
        commands: commands.clone(),
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

    // Phase 1: history that exists BEFORE the query does.
    for (rel, row, diff) in phase1 {
        tx.send((Arc::from(*rel), row.iter().copied().collect(), *diff))
            .unwrap();
    }
    std::thread::sleep(Duration::from_millis(400));

    // Compile the query control-side and add it at runtime.
    let (query_prog, q_strata, q_plan, q_fat) = build(query_dl);
    assert_eq!(q_fat, fat, "query and base must agree on fat mode");
    let q_idb_map = aggregation_catalog_from_program(&query_prog);
    let acc: Arc<Mutex<HashMap<(String, Vec<i64>), isize>>> = Arc::new(Mutex::new(HashMap::new()));
    let acc_cb = Arc::clone(&acc);
    commands.push(QueryCommand::Add(Arc::new(CompiledQuery {
        id: "q".into(),
        strata: q_strata,
        plans: q_plan.program_plan().to_owned(),
        idb_map: q_idb_map,
        fat_mode: q_fat,
        output_callback: Arc::new(
            move |rel: &str, row: smallvec::SmallVec<[i64; 8]>, diff: isize| {
                *acc_cb
                    .lock()
                    .unwrap()
                    .entry((rel.to_string(), row.to_vec()))
                    .or_insert(0) += diff;
            },
        ),
    })));
    std::thread::sleep(Duration::from_millis(500));

    if drop_before_phase2 {
        commands.push(QueryCommand::Drop { id: "q".into() });
        std::thread::sleep(Duration::from_millis(300));
    }

    // Phase 2: live updates the (still-present or dropped) query must track.
    for (rel, row, diff) in phase2 {
        tx.send((Arc::from(*rel), row.iter().copied().collect(), *diff))
            .unwrap();
    }
    std::thread::sleep(Duration::from_millis(400));

    shutdown.store(true, Ordering::Relaxed);
    drop(tx);
    handle.join().unwrap();

    let mut result: HashMap<String, HashSet<Vec<i64>>> = HashMap::new();
    for decl in query_prog.idbs() {
        result.entry(decl.name().to_string()).or_default();
    }
    for ((rel, row), count) in acc.lock().unwrap().iter() {
        if *count > 0 {
            result.entry(rel.clone()).or_default().insert(row.clone());
        }
    }
    result
}

/// Split random edges into: unique phase-1 inserts, unique phase-2 inserts,
/// and phase-2 deletes drawn from everything inserted. Returns the update
/// sequences plus the final surviving edge set.
type LatePhases = (
    Vec<(&'static str, Vec<i64>, isize)>,
    Vec<(&'static str, Vec<i64>, isize)>,
    HashSet<(i64, i64)>,
);

fn late_phases(edges1: &[(i64, i64)], edges2: &[(i64, i64)], dels: &[(i64, i64)]) -> LatePhases {
    let p1: HashSet<(i64, i64)> = edges1.iter().cloned().collect();
    let p2: HashSet<(i64, i64)> = edges2.iter().cloned().filter(|e| !p1.contains(e)).collect();
    let all: HashSet<(i64, i64)> = p1.union(&p2).cloned().collect();
    let deleted: HashSet<(i64, i64)> = dels.iter().cloned().filter(|e| all.contains(e)).collect();
    let final_edges: HashSet<(i64, i64)> = all.difference(&deleted).cloned().collect();

    let phase1: Vec<(&str, Vec<i64>, isize)> =
        p1.iter().map(|&(x, y)| ("edge", vec![x, y], 1)).collect();
    let mut phase2: Vec<(&str, Vec<i64>, isize)> =
        p2.iter().map(|&(x, y)| ("edge", vec![x, y], 1)).collect();
    phase2.extend(deleted.iter().map(|&(x, y)| ("edge", vec![x, y], -1)));
    (phase1, phase2, final_edges)
}

/// Query over the base's PUBLISHED IDB (`tc`): a non-recursive join.
const LQ_TWO_HOP_QUERY: &str = "\
.in
.decl tc(x: number, y: number)

.printsize
.decl two_hop(x: number, y: number)

.rule
two_hop(X, Y) :- tc(X, Z), tc(Z, Y).
";

const LQ_TWO_HOP_COMBINED: &str = "\
.in
.decl edge(x: number, y: number)

.printsize
.decl tc(x: number, y: number)
.decl two_hop(x: number, y: number)

.rule
tc(X, Y) :- edge(X, Y).
tc(X, Y) :- tc(X, Z), edge(Z, Y).
two_hop(X, Y) :- tc(X, Z), tc(Z, Y).
";

/// Recursive query over the base's PUBLISHED EDB (`edge`).
const LQ_REACH_QUERY: &str = "\
.in
.decl edge(x: number, y: number)

.printsize
.decl reach(x: number, y: number)

.rule
reach(X, Y) :- edge(X, Y).
reach(X, Y) :- reach(X, Z), edge(Z, Y).
";

/// Aggregation query over the published IDB.
const LQ_DEG_QUERY: &str = "\
.in
.decl tc(x: number, y: number)

.printsize
.decl deg(x: number, c: number)

.rule
deg(X, count(Y)) :- tc(X, Y).
";

const LQ_DEG_COMBINED: &str = "\
.in
.decl edge(x: number, y: number)

.printsize
.decl tc(x: number, y: number)
.decl deg(x: number, c: number)

.rule
tc(X, Y) :- edge(X, Y).
tc(X, Y) :- tc(X, Z), edge(Z, Y).
deg(X, count(Y)) :- tc(X, Y).
";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(6))]

    /// Join query added at runtime == same rules compiled in from the start.
    #[test]
    fn late_query_two_hop_equals_batch(
        edges1 in edges_strategy(),
        edges2 in edges_strategy(),
        dels in edges_strategy(),
    ) {
        let (phase1, phase2, final_edges) = late_phases(&edges1, &edges2, &dels);
        let got = run_streaming_with_late_query(
            TC_PROGRAM, &["tc"], LQ_TWO_HOP_QUERY, &phase1, &phase2, false, 1,
        );
        let rows: Vec<Vec<i64>> = final_edges.iter().map(|&(x, y)| vec![x, y]).collect();
        let batch = run_batch(LQ_TWO_HOP_COMBINED, &[("edge", rows)]);
        prop_assert_eq!(got["two_hop"].clone(), batch["two_hop"].clone());
    }

    /// RECURSIVE query added at runtime over a published EDB.
    #[test]
    fn late_query_recursive_reach_equals_batch(
        edges1 in edges_strategy(),
        edges2 in edges_strategy(),
        dels in edges_strategy(),
    ) {
        let (phase1, phase2, final_edges) = late_phases(&edges1, &edges2, &dels);
        let got = run_streaming_with_late_query(
            TC_PROGRAM, &["edge"], LQ_REACH_QUERY, &phase1, &phase2, false, 1,
        );
        let rows: Vec<Vec<i64>> = final_edges.iter().map(|&(x, y)| vec![x, y]).collect();
        let batch = run_batch(LQ_REACH_QUERY, &[("edge", rows)]);
        prop_assert_eq!(got["reach"].clone(), batch["reach"].clone());
    }

    /// Aggregation query added at runtime (count over the published IDB).
    #[test]
    fn late_query_aggregation_equals_batch(
        edges1 in edges_strategy(),
        edges2 in edges_strategy(),
        dels in edges_strategy(),
    ) {
        let (phase1, phase2, final_edges) = late_phases(&edges1, &edges2, &dels);
        let got = run_streaming_with_late_query(
            TC_PROGRAM, &["tc"], LQ_DEG_QUERY, &phase1, &phase2, false, 1,
        );
        let rows: Vec<Vec<i64>> = final_edges.iter().map(|&(x, y)| vec![x, y]).collect();
        let batch = run_batch(LQ_DEG_COMBINED, &[("edge", rows)]);
        prop_assert_eq!(got["deg"].clone(), batch["deg"].clone());
    }

    /// The whole pipeline under MULTIPLE workers: every worker must construct
    /// the query dataflow, in the same order, from the shared command log.
    #[test]
    fn late_query_two_hop_equals_batch_two_workers(
        edges1 in edges_strategy(),
        edges2 in edges_strategy(),
        dels in edges_strategy(),
    ) {
        let (phase1, phase2, final_edges) = late_phases(&edges1, &edges2, &dels);
        let got = run_streaming_with_late_query(
            TC_PROGRAM, &["tc"], LQ_TWO_HOP_QUERY, &phase1, &phase2, false, 2,
        );
        let rows: Vec<Vec<i64>> = final_edges.iter().map(|&(x, y)| vec![x, y]).collect();
        let batch = run_batch(LQ_TWO_HOP_COMBINED, &[("edge", rows)]);
        prop_assert_eq!(got["two_hop"].clone(), batch["two_hop"].clone());
    }
}

/// A dropped query stops tracking: its result stays frozen at drop time even
/// as the base keeps changing.
#[test]
fn dropped_query_stops_tracking() {
    // Phase 1: chain 0 -> 1 -> 2 (tc gains (0,2): one two_hop row).
    let phase1: Vec<(&str, Vec<i64>, isize)> =
        vec![("edge", vec![0, 1], 1), ("edge", vec![1, 2], 1)];
    // Phase 2 (after the drop): extend the chain; two_hop WOULD grow.
    let phase2: Vec<(&str, Vec<i64>, isize)> = vec![("edge", vec![2, 3], 1)];

    let got = run_streaming_with_late_query(
        TC_PROGRAM,
        &["tc"],
        LQ_TWO_HOP_QUERY,
        &phase1,
        &phase2,
        true,
        1,
    );

    // Frozen at the phase-1 answer...
    let rows1: Vec<Vec<i64>> = vec![vec![0, 1], vec![1, 2]];
    let batch1 = run_batch(LQ_TWO_HOP_COMBINED, &[("edge", rows1)]);
    assert_eq!(got["two_hop"], batch1["two_hop"]);

    // ...which really is different from the final answer (guard the guard).
    let rows2: Vec<Vec<i64>> = vec![vec![0, 1], vec![1, 2], vec![2, 3]];
    let batch2 = run_batch(LQ_TWO_HOP_COMBINED, &[("edge", rows2)]);
    assert_ne!(batch1["two_hop"], batch2["two_hop"]);
}

/// A query added after MANY sealed epochs — with the seal loop compacting the
/// published traces along the way — still sees exactly the net history. The
/// pre-add history is deliberately churny (inserts later retracted) so the
/// consolidation actually merges opposing diffs rather than replaying them.
#[test]
fn late_query_after_compacted_history_equals_batch() {
    let dir = tempfile::tempdir().unwrap();
    let facts_dir = dir.path().join("facts");
    std::fs::create_dir_all(&facts_dir).unwrap();
    let prog_path = dir.path().join("program.dl");
    std::fs::write(&prog_path, TC_PROGRAM).unwrap();

    let (base_prog, strata, plan, fat) = build(TC_PROGRAM);
    for decl in base_prog.edbs() {
        std::fs::write(facts_dir.join(format!("{}.facts", decl.name())), "").unwrap();
    }
    let idb_map = aggregation_catalog_from_program(&base_prog);
    let args = Args::new(
        prog_path.to_string_lossy().into_owned(),
        facts_dir.to_string_lossy().into_owned(),
        None,
        ",".to_string(),
        1,
    );

    let (tx, rx) =
        crossbeam_channel::bounded::<(Arc<str>, smallvec::SmallVec<[i64; 8]>, isize)>(100_000);
    let base_callback: Arc<dyn Fn(&str, smallvec::SmallVec<[i64; 8]>, isize) + Send + Sync> =
        Arc::new(|_, _, _| {});
    let shutdown = Arc::new(AtomicBool::new(false));
    let commands = CommandLog::default();
    let cfg = StreamingConfig {
        input: rx,
        output_callback: base_callback,
        shutdown: Arc::clone(&shutdown),
        output_seq: Arc::new(AtomicU64::new(0)),
        publish: ["tc".to_string()].into_iter().collect(),
        commands: commands.clone(),
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

    // Many separately-sealed epochs of churn: every wave inserts a decoy edge
    // and retracts the previous wave's decoy; a stable chain edge accretes.
    // Sleeping between waves forces distinct seals, each downgrading the
    // published traces' compaction frontiers.
    for wave in 0i64..6 {
        tx.send((
            Arc::from("edge"),
            [wave, wave + 1].iter().copied().collect(),
            1,
        ))
        .unwrap();
        tx.send((
            Arc::from("edge"),
            [90 + wave, 90 + wave].iter().copied().collect(),
            1,
        ))
        .unwrap();
        if wave > 0 {
            let prev = 90 + wave - 1;
            tx.send((
                Arc::from("edge"),
                [prev, prev].iter().copied().collect(),
                -1,
            ))
            .unwrap();
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    // Add the two_hop query long after that history was compacted.
    let (query_prog, q_strata, q_plan, q_fat) = build(LQ_TWO_HOP_QUERY);
    assert_eq!(q_fat, fat);
    let q_idb_map = aggregation_catalog_from_program(&query_prog);
    let acc: Arc<Mutex<HashMap<(String, Vec<i64>), isize>>> = Arc::new(Mutex::new(HashMap::new()));
    let acc_cb = Arc::clone(&acc);
    commands.push(QueryCommand::Add(Arc::new(CompiledQuery {
        id: "q".into(),
        strata: q_strata,
        plans: q_plan.program_plan().to_owned(),
        idb_map: q_idb_map,
        fat_mode: q_fat,
        output_callback: Arc::new(
            move |rel: &str, row: smallvec::SmallVec<[i64; 8]>, diff: isize| {
                *acc_cb
                    .lock()
                    .unwrap()
                    .entry((rel.to_string(), row.to_vec()))
                    .or_insert(0) += diff;
            },
        ),
    })));
    std::thread::sleep(Duration::from_millis(500));

    // And it keeps following updates sealed after the add.
    tx.send((Arc::from("edge"), [6i64, 7].iter().copied().collect(), 1))
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));

    shutdown.store(true, Ordering::Relaxed);
    drop(tx);
    handle.join().unwrap();

    // Reference: batch over the surviving facts.
    let mut rows: Vec<Vec<i64>> = (0i64..7).map(|w| vec![w, w + 1]).collect();
    rows.push(vec![95, 95]); // the last decoy is never retracted
    let batch = run_batch(LQ_TWO_HOP_COMBINED, &[("edge", rows)]);

    let mut got: HashSet<Vec<i64>> = HashSet::new();
    for ((rel, row), count) in acc.lock().unwrap().iter() {
        if rel == "two_hop" && *count > 0 {
            got.insert(row.clone());
        }
    }
    assert_eq!(got, batch["two_hop"]);
}

// ---------------------------------------------------------------------------
// Late-added queries, in depth: add-time invariance, random lifecycles,
// negation under retraction, id reuse
// ---------------------------------------------------------------------------
//
// The tests above pin one shape (one query, one add point, final check).
// These generalize it: a query's result must be independent of WHEN it was
// added; exactness must hold at every observation point, not only at the
// end; several queries with arbitrary overlapping lifetimes (random add and
// drop points, under one and two workers) must each stay exact; and a
// dropped id must be reusable.

type QueryAcc = Arc<Mutex<HashMap<(String, Vec<i64>), isize>>>;

/// A running streaming engine plus its command log: feed updates, add and
/// drop queries at arbitrary points, snapshot any query's live output.
struct LiveHarness {
    tx: crossbeam_channel::Sender<(Arc<str>, smallvec::SmallVec<[i64; 8]>, isize)>,
    commands: CommandLog,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    fat: bool,
    /// Bumped on every output tuple (base and queries alike) so tests can
    /// wait for real quiescence instead of sleeping a fixed time.
    activity: Arc<AtomicU64>,
    /// The base program's own accumulated IDB output (rows keyed by relation).
    base_acc: QueryAcc,
    /// Per-query accumulators and the query's own IDB names.
    accs: HashMap<String, (QueryAcc, Vec<String>)>,
    _dir: tempfile::TempDir,
}

impl LiveHarness {
    fn start(base_dl: &str, publish: &[&str], workers: usize) -> Self {
        Self::start_with_facts(base_dl, publish, workers, &[])
    }

    /// Like [`LiveHarness::start`], staging non-empty facts files for the
    /// listed EDBs — the engine batch-loads those at epoch 0, a different
    /// path than channel-fed rows.
    fn start_with_facts(
        base_dl: &str,
        publish: &[&str],
        workers: usize,
        facts: &[(&str, &str)],
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let facts_dir = dir.path().join("facts");
        std::fs::create_dir_all(&facts_dir).unwrap();
        let prog_path = dir.path().join("program.dl");
        std::fs::write(&prog_path, base_dl).unwrap();

        let (base_prog, strata, plan, fat) = build(base_dl);
        for decl in base_prog.edbs() {
            // Mirror the loader's resolution: `.input <path>` when given, else
            // `<name>.facts` (corpus programs use explicit paths).
            let rel_file = decl
                .path()
                .unwrap_or_else(|| format!("{}.facts", decl.name()));
            let p = facts_dir.join(&rel_file);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            let content = facts
                .iter()
                .find(|(name, _)| *name == decl.name())
                .map(|(_, c)| *c)
                .unwrap_or("");
            std::fs::write(p, content).unwrap();
        }
        let idb_map = aggregation_catalog_from_program(&base_prog);
        let args = Args::new(
            prog_path.to_string_lossy().into_owned(),
            facts_dir.to_string_lossy().into_owned(),
            None,
            ",".to_string(),
            workers,
        );

        let (tx, rx) =
            crossbeam_channel::bounded::<(Arc<str>, smallvec::SmallVec<[i64; 8]>, isize)>(100_000);
        // The base program's own output, accumulated like a query's: corpus
        // tests use it as the oracle a copy-query must reproduce.
        let activity = Arc::new(AtomicU64::new(0));
        let base_acc: QueryAcc = Arc::new(Mutex::new(HashMap::new()));
        let base_acc_cb = Arc::clone(&base_acc);
        let base_activity = Arc::clone(&activity);
        let base_callback: Arc<dyn Fn(&str, smallvec::SmallVec<[i64; 8]>, isize) + Send + Sync> =
            Arc::new(
                move |rel: &str, row: smallvec::SmallVec<[i64; 8]>, diff: isize| {
                    base_activity.fetch_add(1, Ordering::Relaxed);
                    *base_acc_cb
                        .lock()
                        .unwrap()
                        .entry((rel.to_string(), row.to_vec()))
                        .or_insert(0) += diff;
                },
            );
        let shutdown = Arc::new(AtomicBool::new(false));
        let commands = CommandLog::default();
        let cfg = StreamingConfig {
            input: rx,
            output_callback: base_callback,
            shutdown: Arc::clone(&shutdown),
            output_seq: Arc::new(AtomicU64::new(0)),
            publish: publish.iter().map(|s| s.to_string()).collect(),
            commands: commands.clone(),
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

        LiveHarness {
            tx,
            commands,
            shutdown,
            handle: Some(handle),
            fat,
            activity,
            base_acc,
            accs: HashMap::new(),
            _dir: dir,
        }
    }

    fn feed(&self, rel: &str, row: &[i64], diff: isize) {
        self.tx
            .send((Arc::from(rel), row.iter().copied().collect(), diff))
            .unwrap();
    }

    /// Wait for in-flight epochs to seal and drain.
    fn settle(&self) {
        std::thread::sleep(Duration::from_millis(500));
    }

    /// Wait until NO output has been produced for a stability window — real
    /// quiescence, for bases whose fixpoints take arbitrarily long (the doop
    /// corpus programs over random inputs). Panics rather than hanging.
    fn quiesce(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let mut last = self.activity.load(Ordering::Relaxed);
        let mut stable_since = std::time::Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let now = self.activity.load(Ordering::Relaxed);
            if now != last {
                last = now;
                stable_since = std::time::Instant::now();
            } else if stable_since.elapsed() >= Duration::from_millis(600) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "engine did not quiesce within 60s"
            );
        }
    }

    fn add(&mut self, id: &str, query_dl: &str) -> QueryAcc {
        let (query_prog, q_strata, q_plan, q_fat_needed) = build(query_dl);
        // Run the query in the BASE's row mode: a fat base can execute thin
        // query programs (everything is FatRow under fat mode), but a thin
        // base cannot execute a query that NEEDS fat.
        assert!(
            !q_fat_needed || self.fat,
            "query needs fat mode but the base is thin"
        );
        let q_fat = self.fat;
        let q_idb_map = aggregation_catalog_from_program(&query_prog);
        let idbs: Vec<String> = query_prog
            .idbs()
            .iter()
            .map(|d| d.name().to_string())
            .collect();
        let acc: QueryAcc = Arc::new(Mutex::new(HashMap::new()));
        let acc_cb = Arc::clone(&acc);
        let acc_activity = Arc::clone(&self.activity);
        self.commands
            .push(QueryCommand::Add(Arc::new(CompiledQuery {
                id: id.to_string(),
                strata: q_strata,
                plans: q_plan.program_plan().to_owned(),
                idb_map: q_idb_map,
                fat_mode: q_fat,
                output_callback: Arc::new(
                    move |rel: &str, row: smallvec::SmallVec<[i64; 8]>, diff: isize| {
                        acc_activity.fetch_add(1, Ordering::Relaxed);
                        *acc_cb
                            .lock()
                            .unwrap()
                            .entry((rel.to_string(), row.to_vec()))
                            .or_insert(0) += diff;
                    },
                ),
            })));
        self.accs.insert(id.to_string(), (Arc::clone(&acc), idbs));
        acc
    }

    fn drop_query(&mut self, id: &str) {
        self.commands
            .push(QueryCommand::Drop { id: id.to_string() });
        self.accs.remove(id);
    }

    /// A query's current net-positive rows for `rel`, sorted. Also checks the
    /// no-cross-talk invariant: a query's callback only ever sees its own IDBs.
    fn snapshot(&self, id: &str, rel: &str) -> Vec<Vec<i64>> {
        let (acc, idbs) = &self.accs[id];
        let acc = acc.lock().unwrap();
        for (seen_rel, _) in acc.keys() {
            assert!(
                idbs.contains(seen_rel),
                "query '{}' saw a foreign relation '{}' (cross-talk between queries)",
                id,
                seen_rel
            );
        }
        let mut rows: Vec<Vec<i64>> = acc
            .iter()
            .filter(|((r, _), count)| r == rel && **count > 0)
            .map(|((_, row), _)| row.clone())
            .collect();
        rows.sort();
        rows
    }

    fn finish(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        drop(self.tx);
        self.handle.take().unwrap().join().unwrap();
    }
}

/// Deduped, phase-wise updates from raw random edges: inserts only of
/// never-present rows, deletes only of currently-present rows — net counts
/// stay 0/1, so set-based batch references stay valid. Returns each phase's
/// updates and the final surviving edges.
fn phased_updates(
    waves: &[Vec<(i64, i64)>],
    dels: &[(i64, i64)],
) -> (Vec<Vec<((i64, i64), isize)>>, HashSet<(i64, i64)>) {
    let mut present: HashSet<(i64, i64)> = HashSet::new();
    let mut del_iter = dels.iter();
    let mut phases = Vec::new();
    for (i, wave) in waves.iter().enumerate() {
        let mut updates: Vec<((i64, i64), isize)> = Vec::new();
        for &e in wave {
            if present.insert(e) {
                updates.push((e, 1));
            }
        }
        // From the second phase on, also retract a couple of present rows so
        // late-added queries face retractions, not only growth.
        if i > 0 {
            for _ in 0..2 {
                if let Some(&e) = del_iter.next() {
                    if present.remove(&e) {
                        updates.push((e, -1));
                    }
                }
            }
        }
        phases.push(updates);
    }
    (phases, present)
}

fn batch_rows(final_edges: &HashSet<(i64, i64)>) -> Vec<Vec<i64>> {
    final_edges.iter().map(|&(x, y)| vec![x, y]).collect()
}

/// Negation query over the published IDB: sinks of the closure. Negation plus
/// retraction is where incremental maintenance classically breaks — deleting
/// an edge can RE-derive dead_end rows.
const LQ_DEADEND_QUERY: &str = "\
.in
.decl tc(x: number, y: number)

.printsize
.decl has_out(x: number)
.decl dead_end(y: number)

.rule
has_out(X) :- tc(X, _).
dead_end(Y) :- tc(_, Y), !has_out(Y).
";

const LQ_DEADEND_COMBINED: &str = "\
.in
.decl edge(x: number, y: number)

.printsize
.decl tc(x: number, y: number)
.decl has_out(x: number)
.decl dead_end(y: number)

.rule
tc(X, Y) :- edge(X, Y).
tc(X, Y) :- tc(X, Z), edge(Z, Y).
has_out(X) :- tc(X, _).
dead_end(Y) :- tc(_, Y), !has_out(Y).
";

/// The query pool for lifecycle tests: (query program, combined reference
/// program, the output relation to compare).
const LQ_POOL: [(&str, &str, &str); 5] = [
    (LQ_TWO_HOP_QUERY, LQ_TWO_HOP_COMBINED, "two_hop"),
    (LQ_REACH_QUERY, LQ_REACH_QUERY, "reach"),
    (LQ_DEG_QUERY, LQ_DEG_COMBINED, "deg"),
    (LQ_DEADEND_QUERY, LQ_DEADEND_COMBINED, "dead_end"),
    (LQ_WSUM_QUERY, LQ_WSUM_COMBINED, "wsum"),
];

/// Sum aggregation in a query (count is covered elsewhere; sum exercises the
/// value-carrying aggregation path).
const LQ_WSUM_QUERY: &str = "\
.in
.decl tc(x: number, y: number)

.printsize
.decl wsum(x: number, s: number)

.rule
wsum(X, sum(Y)) :- tc(X, Y).
";

const LQ_WSUM_COMBINED: &str = "\
.in
.decl edge(x: number, y: number)

.printsize
.decl tc(x: number, y: number)
.decl wsum(x: number, s: number)

.rule
tc(X, Y) :- edge(X, Y).
tc(X, Y) :- tc(X, Z), edge(Z, Y).
wsum(X, sum(Y)) :- tc(X, Y).
";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(6))]

    /// A query's result is independent of WHEN it was added — and it is exact
    /// at every observation point, not only at the end: the early copy must
    /// match a batch over phase-1 facts BEFORE phase 2 arrives, and both
    /// copies must match the final batch (and each other) afterwards.
    #[test]
    fn late_query_result_is_independent_of_add_time(
        wave1 in edges_strategy(),
        wave2 in edges_strategy(),
        dels in edges_strategy(),
    ) {
        let (phases, final_edges) = phased_updates(&[wave1, wave2], &dels);
        let phase1_edges: HashSet<(i64, i64)> =
            phases[0].iter().map(|&(e, _)| e).collect();

        let mut h = LiveHarness::start(TC_PROGRAM, &["tc"], 1);
        for (e, diff) in &phases[0] {
            h.feed("edge", &[e.0, e.1], *diff);
        }
        h.settle();

        h.add("early", LQ_TWO_HOP_QUERY);
        h.settle();
        let batch1 = run_batch(LQ_TWO_HOP_COMBINED, &[("edge", batch_rows(&phase1_edges))]);
        let mut expect1: Vec<Vec<i64>> = batch1["two_hop"].iter().cloned().collect();
        expect1.sort();
        prop_assert_eq!(
            h.snapshot("early", "two_hop"), expect1,
            "exact at the intermediate observation point"
        );

        for (e, diff) in &phases[1] {
            h.feed("edge", &[e.0, e.1], *diff);
        }
        h.settle();
        h.add("late", LQ_TWO_HOP_QUERY);
        h.settle();

        let batch = run_batch(LQ_TWO_HOP_COMBINED, &[("edge", batch_rows(&final_edges))]);
        let mut expect: Vec<Vec<i64>> = batch["two_hop"].iter().cloned().collect();
        expect.sort();
        let early = h.snapshot("early", "two_hop");
        let late = h.snapshot("late", "two_hop");
        prop_assert_eq!(&early, &late, "add time changed the result");
        prop_assert_eq!(early, expect);
        h.finish();
    }

    /// Negation under live retraction: deleting a base edge can RE-derive
    /// dead_end rows in a query that was added mid-run.
    #[test]
    fn late_query_negation_equals_batch(
        wave1 in edges_strategy(),
        wave2 in edges_strategy(),
        dels in edges_strategy(),
    ) {
        let (phases, final_edges) = phased_updates(&[wave1, wave2], &dels);
        let mut h = LiveHarness::start(TC_PROGRAM, &["tc"], 1);
        for (e, diff) in &phases[0] {
            h.feed("edge", &[e.0, e.1], *diff);
        }
        h.settle();
        h.add("q", LQ_DEADEND_QUERY);
        h.settle();
        for (e, diff) in &phases[1] {
            h.feed("edge", &[e.0, e.1], *diff);
        }
        h.settle();

        let batch = run_batch(LQ_DEADEND_COMBINED, &[("edge", batch_rows(&final_edges))]);
        let mut expect: Vec<Vec<i64>> = batch["dead_end"].iter().cloned().collect();
        expect.sort();
        prop_assert_eq!(h.snapshot("q", "dead_end"), expect);
        h.finish();
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    /// Random overlapping query lifecycles, under one and two workers: any
    /// number of queries from the pool, each added at a random phase and
    /// possibly dropped at a random later phase. Every query still alive at
    /// the end must exactly match a batch of its combined program over the
    /// final facts; dropped ones must not disturb the others.
    #[test]
    fn late_query_random_lifecycles_stay_exact(
        workers in 1usize..=2,
        wave1 in edges_strategy(),
        wave2 in edges_strategy(),
        wave3 in edges_strategy(),
        dels in edges_strategy(),
        // (pool query, phase it is added after, phase it is dropped after —
        // values past the last phase mean "never dropped").
        lifecycles in prop::collection::vec((0usize..5, 0u8..3, 1u8..5), 0..5),
    ) {
        let (phases, final_edges) = phased_updates(&[wave1, wave2, wave3], &dels);
        let mut h = LiveHarness::start(TC_PROGRAM, &["tc", "edge"], workers);

        let mut alive: Vec<(String, usize)> = Vec::new();
        for (phase_idx, updates) in phases.iter().enumerate() {
            for (e, diff) in updates {
                h.feed("edge", &[e.0, e.1], *diff);
            }
            h.settle();

            for (i, &(pool_idx, add_at, _)) in lifecycles.iter().enumerate() {
                if add_at as usize == phase_idx {
                    let id = format!("q{}", i);
                    h.add(&id, LQ_POOL[pool_idx].0);
                    alive.push((id, pool_idx));
                }
            }
            for (i, &(_, add_at, drop_at)) in lifecycles.iter().enumerate() {
                if drop_at as usize == phase_idx && drop_at > add_at {
                    let id = format!("q{}", i);
                    if alive.iter().any(|(a, _)| a == &id) {
                        h.drop_query(&id);
                        alive.retain(|(a, _)| a != &id);
                    }
                }
            }
            h.settle();
        }

        for (id, pool_idx) in &alive {
            let (_, combined, out_rel) = LQ_POOL[*pool_idx];
            let batch = run_batch(combined, &[("edge", batch_rows(&final_edges))]);
            let mut expect: Vec<Vec<i64>> = batch[out_rel].iter().cloned().collect();
            expect.sort();
            prop_assert_eq!(
                h.snapshot(id, out_rel), expect,
                "query {} ({}) diverged (workers={})", id, out_rel, workers
            );
        }
        h.finish();
    }
}

/// A dropped query's id is reusable: the re-added query is a fresh dataflow
/// that replays the (grown) history and is exact — no stale state leaks from
/// its predecessor.
#[test]
fn readded_query_id_is_fresh_and_exact() {
    let mut h = LiveHarness::start(TC_PROGRAM, &["tc"], 1);
    h.feed("edge", &[0, 1], 1);
    h.feed("edge", &[1, 2], 1);
    h.settle();

    h.add("q", LQ_TWO_HOP_QUERY);
    h.settle();
    assert_eq!(h.snapshot("q", "two_hop"), vec![vec![0, 2]]);

    h.drop_query("q");
    h.settle();
    h.feed("edge", &[2, 3], 1);
    h.settle();

    h.add("q", LQ_TWO_HOP_QUERY);
    h.settle();
    let batch = run_batch(
        LQ_TWO_HOP_COMBINED,
        &[("edge", vec![vec![0, 1], vec![1, 2], vec![2, 3]])],
    );
    let mut expect: Vec<Vec<i64>> = batch["two_hop"].iter().cloned().collect();
    expect.sort();
    assert_eq!(h.snapshot("q", "two_hop"), expect);
    h.finish();
}

/// Net-positive rows of `rel` in a raw accumulator (for watching an
/// accumulator after its query was replaced or dropped).
fn acc_rows(acc: &QueryAcc, rel: &str) -> Vec<Vec<i64>> {
    let mut rows: Vec<Vec<i64>> = acc
        .lock()
        .unwrap()
        .iter()
        .filter(|((r, _), count)| r == rel && **count > 0)
        .map(|((_, row), _)| row.clone())
        .collect();
    rows.sort();
    rows
}

/// A duplicate Add for an id must tear the predecessor down: the old
/// dataflow's output freezes (its unpressed buttons used to leak it forever,
/// leaving both callbacks live), and the successor is exact.
#[test]
fn duplicate_add_replaces_the_predecessor() {
    let mut h = LiveHarness::start(TC_PROGRAM, &["tc"], 1);
    h.feed("edge", &[0, 1], 1);
    h.feed("edge", &[1, 2], 1);
    h.settle();

    let old_acc = h.add("q", LQ_TWO_HOP_QUERY);
    h.settle();
    assert_eq!(acc_rows(&old_acc, "two_hop"), vec![vec![0, 2]]);

    // Same id again: the predecessor must stop, the successor must be exact.
    let new_acc = h.add("q", LQ_TWO_HOP_QUERY);
    h.settle();
    h.feed("edge", &[2, 3], 1);
    h.settle();

    assert_eq!(
        acc_rows(&old_acc, "two_hop"),
        vec![vec![0, 2]],
        "replaced query kept receiving updates (leaked dataflow)"
    );
    let batch = run_batch(
        LQ_TWO_HOP_COMBINED,
        &[("edge", vec![vec![0, 1], vec![1, 2], vec![2, 3]])],
    );
    let mut expect: Vec<Vec<i64>> = batch["two_hop"].iter().cloned().collect();
    expect.sort();
    assert_eq!(acc_rows(&new_acc, "two_hop"), expect);
    h.finish();
}

// ---------------------------------------------------------------------------
// Coverage: multi-relation imports, the fat-row path, add-under-load, churn
// ---------------------------------------------------------------------------

/// A query importing TWO published relations (the raw EDB and a derived IDB)
/// and joining them — every earlier test imported exactly one relation, so
/// two import sources in one query dataflow were unexercised.
const LQ_SKIP_QUERY: &str = "\
.in
.decl edge(x: number, y: number)
.decl tc(x: number, y: number)

.printsize
.decl skip(x: number, y: number)

.rule
skip(X, Y) :- edge(X, Z), tc(Z, Y).
";

const LQ_SKIP_COMBINED: &str = "\
.in
.decl edge(x: number, y: number)

.printsize
.decl tc(x: number, y: number)
.decl skip(x: number, y: number)

.rule
tc(X, Y) :- edge(X, Y).
tc(X, Y) :- tc(X, Z), edge(Z, Y).
skip(X, Y) :- edge(X, Z), tc(Z, Y).
";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(6))]

    /// Join across two imported relations, added mid-run, under retractions.
    #[test]
    fn late_query_multi_import_join_equals_batch(
        wave1 in edges_strategy(),
        wave2 in edges_strategy(),
        dels in edges_strategy(),
    ) {
        let (phases, final_edges) = phased_updates(&[wave1, wave2], &dels);
        let mut h = LiveHarness::start(TC_PROGRAM, &["tc", "edge"], 1);
        for (e, diff) in &phases[0] {
            h.feed("edge", &[e.0, e.1], *diff);
        }
        h.settle();
        h.add("q", LQ_SKIP_QUERY);
        h.settle();
        for (e, diff) in &phases[1] {
            h.feed("edge", &[e.0, e.1], *diff);
        }
        h.settle();

        let batch = run_batch(LQ_SKIP_COMBINED, &[("edge", batch_rows(&final_edges))]);
        let mut expect: Vec<Vec<i64>> = batch["skip"].iter().cloned().collect();
        expect.sort();
        prop_assert_eq!(h.snapshot("q", "skip"), expect);
        h.finish();
    }

    /// A query added with NO quiescence around it: history still feeding when
    /// the Add lands, more updates immediately after. The settles in every
    /// other test paper over the command-at-epoch-boundary race; this one
    /// hunts it.
    #[test]
    fn late_query_added_under_load_equals_batch(
        wave1 in edges_strategy(),
        wave2 in edges_strategy(),
        dels in edges_strategy(),
    ) {
        let (phases, final_edges) = phased_updates(&[wave1, wave2], &dels);
        let mut h = LiveHarness::start(TC_PROGRAM, &["tc"], 1);
        for (e, diff) in &phases[0] {
            h.feed("edge", &[e.0, e.1], *diff);
        }
        // No settle: the Add races the in-flight phase-1 epochs.
        h.add("q", LQ_TWO_HOP_QUERY);
        for (e, diff) in &phases[1] {
            h.feed("edge", &[e.0, e.1], *diff);
        }
        h.settle();
        h.settle();

        let batch = run_batch(LQ_TWO_HOP_COMBINED, &[("edge", batch_rows(&final_edges))]);
        let mut expect: Vec<Vec<i64>> = batch["two_hop"].iter().cloned().collect();
        expect.sort();
        prop_assert_eq!(h.snapshot("q", "two_hop"), expect);
        h.finish();
    }
}

/// The fat-row path end to end: an arity-9 relation forces fat mode for the
/// whole program, so publishing uses TraceSetFat and a late query imports fat
/// traces — for the wide relation AND for a narrow one (all relations are
/// FatRow under fat mode). Nothing else exercises this path.
#[test]
fn late_query_over_fat_relations_equals_expected() {
    const FAT_BASE: &str = "\
.in
.decl wide(a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number)

.printsize
.decl first_last(a: number, i: number)

.rule
first_last(A, I) :- wide(A, _, _, _, _, _, _, _, I).
";
    const FAT_QUERY: &str = "\
.in
.decl wide(a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number)
.decl first_last(a: number, i: number)

.printsize
.decl mid(e: number)
.decl fl(a: number, i: number)

.rule
mid(E) :- wide(_, _, _, _, E, _, _, _, _).
fl(A, I) :- first_last(A, I).
";
    // Sanity: this base genuinely trips fat mode (else the test tests nothing).
    let (_, _, _, fat) = build(FAT_BASE);
    assert!(fat, "arity-9 base must run in fat mode");

    let mut h = LiveHarness::start(FAT_BASE, &["wide", "first_last"], 1);
    let r1: Vec<i64> = (1..=9).collect();
    let r2: Vec<i64> = (11..=19).collect();
    h.feed("wide", &r1, 1);
    h.feed("wide", &r2, 1);
    h.settle();

    h.add("q", FAT_QUERY);
    h.settle();
    assert_eq!(h.snapshot("q", "mid"), vec![vec![5], vec![15]]);
    assert_eq!(h.snapshot("q", "fl"), vec![vec![1, 9], vec![11, 19]]);

    // Retraction through the fat imports.
    h.feed("wide", &r2, -1);
    h.settle();
    assert_eq!(h.snapshot("q", "mid"), vec![vec![5]]);
    assert_eq!(h.snapshot("q", "fl"), vec![vec![1, 9]]);
    h.finish();
}

/// Rapid add/drop churn: fifty queries built and torn down back to back
/// (add and drop often land in the same command batch, so a dataflow is
/// pressed the moment it is built). The engine must stay healthy and a final
/// query must still be exact.
#[test]
fn rapid_add_drop_churn_stays_healthy() {
    let mut h = LiveHarness::start(TC_PROGRAM, &["tc"], 1);
    h.feed("edge", &[0, 1], 1);
    h.feed("edge", &[1, 2], 1);
    h.feed("edge", &[2, 3], 1);
    h.settle();

    for i in 0..50 {
        let id = format!("c{}", i);
        h.add(&id, LQ_TWO_HOP_QUERY);
        h.drop_query(&id);
        if i % 10 == 9 {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    h.add("final", LQ_TWO_HOP_QUERY);
    h.settle();
    let batch = run_batch(
        LQ_TWO_HOP_COMBINED,
        &[("edge", vec![vec![0, 1], vec![1, 2], vec![2, 3]])],
    );
    let mut expect: Vec<Vec<i64>> = batch["two_hop"].iter().cloned().collect();
    expect.sort();
    assert_eq!(h.snapshot("final", "two_hop"), expect);
    h.finish();
}

// ---------------------------------------------------------------------------
// Corpus sweep + remaining edges: negated imports, add-before-run, shutdown
// races, fat under multiple workers
// ---------------------------------------------------------------------------

/// Negation directly against an IMPORTED relation (dead_end negates a
/// query-internal helper; this antijoins the import itself).
const LQ_NOLOOP_QUERY: &str = "\
.in
.decl edge(x: number, y: number)
.decl tc(x: number, y: number)

.printsize
.decl noloop(x: number, y: number)

.rule
noloop(X, Y) :- tc(X, Y), !edge(X, X).
";

const LQ_NOLOOP_COMBINED: &str = "\
.in
.decl edge(x: number, y: number)

.printsize
.decl tc(x: number, y: number)
.decl noloop(x: number, y: number)

.rule
tc(X, Y) :- edge(X, Y).
tc(X, Y) :- tc(X, Z), edge(Z, Y).
noloop(X, Y) :- tc(X, Y), !edge(X, X).
";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(6))]

    /// Negation against the imported relation itself, under retractions —
    /// deleting a self-loop edge must RE-derive noloop rows through the import.
    #[test]
    fn late_query_negated_import_equals_batch(
        wave1 in edges_strategy(),
        wave2 in edges_strategy(),
        dels in edges_strategy(),
    ) {
        let (phases, final_edges) = phased_updates(&[wave1, wave2], &dels);
        let mut h = LiveHarness::start(TC_PROGRAM, &["tc", "edge"], 1);
        for (e, diff) in &phases[0] {
            h.feed("edge", &[e.0, e.1], *diff);
        }
        h.settle();
        h.add("q", LQ_NOLOOP_QUERY);
        h.settle();
        for (e, diff) in &phases[1] {
            h.feed("edge", &[e.0, e.1], *diff);
        }
        h.settle();

        let batch = run_batch(LQ_NOLOOP_COMBINED, &[("edge", batch_rows(&final_edges))]);
        let mut expect: Vec<Vec<i64>> = batch["noloop"].iter().cloned().collect();
        expect.sort();
        prop_assert_eq!(h.snapshot("q", "noloop"), expect);
        h.finish();
    }
}

/// A query added BEFORE the engine starts must apply at the first loop
/// iteration and be exact — every other test adds to an already-running
/// engine, leaving the pre-run ordering (cursor at zero, registry fresh)
/// unexercised.
#[test]
fn query_added_before_run_applies_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let facts_dir = dir.path().join("facts");
    std::fs::create_dir_all(&facts_dir).unwrap();
    let prog_path = dir.path().join("program.dl");
    std::fs::write(&prog_path, TC_PROGRAM).unwrap();

    let (base_prog, strata, plan, fat) = build(TC_PROGRAM);
    for decl in base_prog.edbs() {
        std::fs::write(facts_dir.join(format!("{}.facts", decl.name())), "").unwrap();
    }
    let idb_map = aggregation_catalog_from_program(&base_prog);
    let args = Args::new(
        prog_path.to_string_lossy().into_owned(),
        facts_dir.to_string_lossy().into_owned(),
        None,
        ",".to_string(),
        1,
    );

    let (tx, rx) =
        crossbeam_channel::bounded::<(Arc<str>, smallvec::SmallVec<[i64; 8]>, isize)>(100_000);
    let shutdown = Arc::new(AtomicBool::new(false));
    let commands = CommandLog::default();

    // Push the Add BEFORE the engine exists.
    let (query_prog, q_strata, q_plan, q_fat) = build(LQ_TWO_HOP_QUERY);
    assert_eq!(q_fat, fat);
    let q_idb_map = aggregation_catalog_from_program(&query_prog);
    let acc: Arc<Mutex<HashMap<(String, Vec<i64>), isize>>> = Arc::new(Mutex::new(HashMap::new()));
    let acc_cb = Arc::clone(&acc);
    commands.push(QueryCommand::Add(Arc::new(CompiledQuery {
        id: "early-bird".into(),
        strata: q_strata,
        plans: q_plan.program_plan().to_owned(),
        idb_map: q_idb_map,
        fat_mode: q_fat,
        output_callback: Arc::new(
            move |rel: &str, row: smallvec::SmallVec<[i64; 8]>, diff: isize| {
                *acc_cb
                    .lock()
                    .unwrap()
                    .entry((rel.to_string(), row.to_vec()))
                    .or_insert(0) += diff;
            },
        ),
    })));

    let cfg = StreamingConfig {
        input: rx,
        output_callback: Arc::new(|_, _, _| {}),
        shutdown: Arc::clone(&shutdown),
        output_seq: Arc::new(AtomicU64::new(0)),
        publish: ["tc".to_string()].into_iter().collect(),
        commands,
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

    for row in [[0i64, 1], [1, 2]] {
        tx.send((Arc::from("edge"), row.iter().copied().collect(), 1))
            .unwrap();
    }
    std::thread::sleep(Duration::from_millis(600));
    shutdown.store(true, Ordering::Relaxed);
    drop(tx);
    handle.join().unwrap();

    let rows: Vec<Vec<i64>> = acc
        .lock()
        .unwrap()
        .iter()
        .filter(|((r, _), c)| r == "two_hop" && **c > 0)
        .map(|((_, row), _)| row.clone())
        .collect();
    assert_eq!(rows, vec![vec![0, 2]]);
}

/// Shutdown with a command still in flight (pushed, never settled) must not
/// hang or panic — the loop may or may not process it before exiting.
#[test]
fn shutdown_with_pending_command_is_clean() {
    let mut h = LiveHarness::start(TC_PROGRAM, &["tc"], 1);
    h.feed("edge", &[0, 1], 1);
    h.add("pending", LQ_TWO_HOP_QUERY);
    h.finish(); // no settle: the add races the shutdown
}

/// The fat-row path under TWO workers: fat exchange plus fat imports is a
/// combination nothing else hits.
#[test]
fn late_query_over_fat_relations_two_workers() {
    const FAT_BASE: &str = "\
.in
.decl wide(a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number)

.printsize
.decl first_last(a: number, i: number)

.rule
first_last(A, I) :- wide(A, _, _, _, _, _, _, _, I).
";
    const FAT_QUERY: &str = "\
.in
.decl wide(a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number)

.printsize
.decl mid(e: number)

.rule
mid(E) :- wide(_, _, _, _, E, _, _, _, _).
";
    let mut h = LiveHarness::start(FAT_BASE, &["wide"], 2);
    let r1: Vec<i64> = (1..=9).collect();
    let r2: Vec<i64> = (11..=19).collect();
    h.feed("wide", &r1, 1);
    h.feed("wide", &r2, 1);
    h.settle();
    h.add("q", FAT_QUERY);
    h.settle();
    assert_eq!(h.snapshot("q", "mid"), vec![vec![5], vec![15]]);
    h.feed("wide", &r1, -1);
    h.settle();
    assert_eq!(h.snapshot("q", "mid"), vec![vec![15]]);
    h.finish();
}

// ---------------------------------------------------------------------------
// Corpus copy-query sweep
// ---------------------------------------------------------------------------
//
// For EVERY corpus program: run it as the base with synthetic typed facts,
// publish every declared relation, add one copy-query per relation, and
// assert each copy exactly reproduces the base's own rows (fed facts for
// EDBs, the base's live output for IDBs). This sweeps the whole grammar
// surface AS PUBLISH MATERIAL — recursive strata, aggregations, negation,
// string/float columns, fat programs — combinations no hand-picked base hits.

const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/flowlog_programs");

fn type_name(dt: &DataType) -> &'static str {
    match dt {
        DataType::Integer => "number",
        DataType::String => "string",
        DataType::Float => "float",
    }
}

/// A query that copies `decl`'s relation verbatim.
fn copy_query_for(decl: &parsing::decl::RelDecl) -> String {
    let cols: Vec<String> = decl
        .attributes()
        .iter()
        .enumerate()
        .map(|(i, a)| format!("c{}: {}", i, type_name(a.data_type())))
        .collect();
    let vars: Vec<String> = (0..decl.arity()).map(|i| format!("V{}", i)).collect();
    format!(
        ".in\n.decl {}({})\n.printsize\n.decl copyq({})\n.rule\ncopyq({}) :- {}({}).\n",
        decl.name(),
        cols.join(", "),
        cols.join(", "),
        vars.join(", "),
        decl.name(),
        vars.join(", ")
    )
}

/// Deterministic synthetic facts for a decl: small typed value pools, varied
/// per row/column so joins and closures actually fire.
fn synth_rows(decl: &parsing::decl::RelDecl, rows: usize) -> Vec<Vec<i64>> {
    let strings = ["a", "b", "c"];
    let floats = [0.5f64, 1.5, 2.5];
    (0..rows)
        .map(|j| {
            decl.attributes()
                .iter()
                .enumerate()
                .map(|(k, attr)| {
                    let pick = (j * (k + 3) + k * 7 + j / 2) % 3;
                    match attr.data_type() {
                        DataType::Integer => ((j + k * 2 + pick) % 4) as i64,
                        DataType::String => reading::intern(strings[pick]),
                        DataType::Float => reading::float_to_i64(floats[pick]),
                    }
                })
                .collect()
        })
        .collect()
}

fn corpus_copy_case(path: &std::path::Path, workers: usize) {
    let src = std::fs::read_to_string(path).unwrap();
    let (program, _, _, _) = build(&src);

    let publish: Vec<String> = program
        .edbs()
        .iter()
        .chain(program.idbs().iter())
        .map(|d| d.name().to_string())
        .collect();
    let publish_refs: Vec<&str> = publish.iter().map(|s| s.as_str()).collect();
    let mut h = LiveHarness::start(&src, &publish_refs, workers);

    // Feed synthetic facts and remember each EDB's (deduped) rows.
    let mut fed: HashMap<String, HashSet<Vec<i64>>> = HashMap::new();
    for decl in program.edbs() {
        let rows: HashSet<Vec<i64>> = synth_rows(decl, 8).into_iter().collect();
        for row in &rows {
            h.feed(decl.name(), row, 1);
        }
        fed.insert(decl.name().to_string(), rows);
    }
    h.settle();
    h.quiesce();

    // One copy-query per declared relation, all added at once (100+ query
    // dataflows on the doop programs), then wait for real quiescence.
    for decl in program.edbs().iter().chain(program.idbs().iter()) {
        h.add(&format!("copy_{}", decl.name()), &copy_query_for(decl));
    }
    h.settle();
    h.quiesce();

    // Every copy must reproduce the original exactly. Collect ALL divergences
    // (not assert-first) so one run shows the whole failure map.
    let heads: HashSet<&str> = program
        .rules()
        .iter()
        .map(|r| r.head().name().as_str())
        .collect();
    let mut diffs: Vec<String> = Vec::new();
    for decl in program.edbs() {
        // An EDB that is also a rule head publishes its FULL contents (input
        // plus derived), matching what the base's own rules see; the fed rows
        // are only a lower bound there.
        let got = h.snapshot(&format!("copy_{}", decl.name()), "copyq");
        let mut expect: Vec<Vec<i64>> = fed[decl.name()].iter().cloned().collect();
        expect.sort();
        if heads.contains(decl.name()) {
            let got_set: HashSet<&Vec<i64>> = got.iter().collect();
            if !expect.iter().all(|r| got_set.contains(r)) {
                diffs.push(format!(
                    "EDB+head '{}': copy {:?} missing fed rows {:?}",
                    decl.name(),
                    got,
                    expect
                ));
            }
        } else if got != expect {
            diffs.push(format!(
                "EDB '{}': copy {:?} != fed {:?}",
                decl.name(),
                got,
                expect
            ));
        }
    }
    let base = h.base_acc.lock().unwrap().clone();
    for decl in program.idbs() {
        let mut expect: Vec<Vec<i64>> = base
            .iter()
            .filter(|((r, _), c)| r == decl.name() && **c > 0)
            .map(|((_, row), _)| row.clone())
            .collect();
        expect.sort();
        let got = h.snapshot(&format!("copy_{}", decl.name()), "copyq");
        if got != expect {
            diffs.push(format!(
                "IDB '{}': copy {:?} != base {:?}",
                decl.name(),
                got,
                expect
            ));
        }
    }
    h.finish();
    assert!(
        diffs.is_empty(),
        "{}: {} copies diverged:\n{}",
        path.display(),
        diffs.len(),
        diffs.join("\n")
    );
}

#[test]
fn corpus_bases_accept_copy_queries() {
    // No DEP2_MAX_ITER pin: recursive min/max now aggregates inside the
    // fixpoint loop, so sssp converges exactly even over cyclic random data.
    // (This sweep used to need a small bound to truncate its divergence.)
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(CORPUS_DIR)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "dl"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "corpus dir is empty");

    let mut failures = Vec::new();
    for path in &paths {
        if let Err(e) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| corpus_copy_case(path, 1)))
        {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "non-string panic".to_string());
            failures.push(format!("{}: {}", path.display(), msg));
        }
    }
    assert!(
        failures.is_empty(),
        "{} corpus program(s) failed the copy-query sweep:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// A corpus subset under TWO workers: the shapes that found bugs at one
/// worker (rule-less published IDBs, EDB-also-head, recursive aggregation,
/// heavy strata) re-checked with data exchange in play.
#[test]
fn corpus_subset_copy_queries_two_workers() {
    // sssp.dl included: its recursive min used to diverge (bug 5); in-loop
    // aggregation converges it exactly, no iteration-bound pin needed.
    let subset = ["batik.dl", "borrow.dl", "crdt_slow.dl", "cc.dl", "sssp.dl"];
    let mut failures = Vec::new();
    for name in subset {
        let path = std::path::Path::new(CORPUS_DIR).join(name);
        if let Err(e) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| corpus_copy_case(&path, 2)))
        {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "non-string panic".to_string());
            failures.push(format!("{}: {}", name, msg));
        }
    }
    assert!(
        failures.is_empty(),
        "{} program(s) failed under two workers:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Base facts loaded from FILES at epoch 0 (a different path than channel
/// feeding: batch-read, sealed specially, compacted first) must replay into a
/// late query like anything else — and mix correctly with channel rows fed
/// after.
#[test]
fn late_query_replays_epoch_zero_file_facts() {
    let mut h = LiveHarness::start_with_facts(TC_PROGRAM, &["tc"], 1, &[("edge", "0,1\n1,2\n")]);
    h.settle();
    h.quiesce();

    h.add("q", LQ_TWO_HOP_QUERY);
    h.settle();
    h.quiesce();
    let batch1 = run_batch(
        LQ_TWO_HOP_COMBINED,
        &[("edge", vec![vec![0, 1], vec![1, 2]])],
    );
    let mut expect1: Vec<Vec<i64>> = batch1["two_hop"].iter().cloned().collect();
    expect1.sort();
    assert_eq!(
        h.snapshot("q", "two_hop"),
        expect1,
        "epoch-0 file facts replay into the query"
    );

    // Channel rows on top of file facts.
    h.feed("edge", &[2, 3], 1);
    h.settle();
    h.quiesce();
    let batch2 = run_batch(
        LQ_TWO_HOP_COMBINED,
        &[("edge", vec![vec![0, 1], vec![1, 2], vec![2, 3]])],
    );
    let mut expect2: Vec<Vec<i64>> = batch2["two_hop"].iter().cloned().collect();
    expect2.sort();
    assert_eq!(h.snapshot("q", "two_hop"), expect2);
    h.finish();
}

/// Data exchange at volume: thousands of edges under TWO workers, then a
/// recursive query added mid-run whose import replay crosses worker exchange
/// channels with real batch sizes (every other multi-worker test uses a
/// handful of rows).
#[test]
fn late_query_at_volume_two_workers() {
    // A pass-through base: publish the raw edges, keep base compute small.
    const THIN_BASE: &str = "\
.in
.decl edge(x: number, y: number)

.printsize
.decl touched(x: number)

.rule
touched(X) :- edge(X, _).
";
    // Single-source reachability: output stays linear in the node count.
    const REACH1: &str = "\
.in
.decl edge(x: number, y: number)

.printsize
.decl reach1(y: number)

.rule
reach1(Y) :- edge(0, Y).
reach1(Y) :- reach1(X), edge(X, Y).
";

    // Deterministic pseudo-random sparse graph: 5000 edges over 2000 nodes.
    let mut edges: HashSet<(i64, i64)> = HashSet::new();
    let mut state: u64 = 0x243f6a8885a308d3;
    while edges.len() < 5000 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let x = ((state >> 20) % 2000) as i64;
        let y = ((state >> 44) % 2000) as i64;
        edges.insert((x, y));
    }

    let mut h = LiveHarness::start(THIN_BASE, &["edge"], 2);
    for &(x, y) in &edges {
        h.feed("edge", &[x, y], 1);
    }
    h.settle();
    h.quiesce();

    h.add("q", REACH1);
    h.settle();
    h.quiesce();

    let rows: Vec<Vec<i64>> = edges.iter().map(|&(x, y)| vec![x, y]).collect();
    let batch = run_batch(REACH1, &[("edge", rows)]);
    let mut expect: Vec<Vec<i64>> = batch["reach1"].iter().cloned().collect();
    expect.sort();
    assert!(!expect.is_empty(), "graph must reach something from node 0");
    assert_eq!(h.snapshot("q", "reach1"), expect);
    h.finish();
}

/// Recursive MIN over cyclic weighted random data at two workers. Under the
/// old aggsrc desugar this DIVERGED (bug 5: values kept growing around
/// positive cycles) and wedged the workers; with the aggregation running
/// inside the fixpoint loop it converges exactly, with no iteration-bound
/// pin. (`DEP2_MAX_ITER` remains a default-100k safety net in the engine.)
#[test]
fn base_recursive_aggregation_two_workers_stays_responsive() {
    let path = std::path::Path::new(CORPUS_DIR).join("sssp.dl");
    let src = std::fs::read_to_string(&path).unwrap();
    let h = LiveHarness::start(&src, &[], 2);
    // The exact synthetic data the corpus sweep uses: cyclic, positive
    // self-loop weights reachable from sources.
    let (program, _, _, _) = build(&src);
    let arc_decl = program.edbs().iter().find(|d| d.name() == "arc").unwrap();
    let id_decl = program.edbs().iter().find(|d| d.name() == "id").unwrap();
    for row in synth_rows(arc_decl, 8) {
        h.feed("arc", &row, 1);
    }
    for row in synth_rows(id_decl, 8) {
        h.feed("id", &row, 1);
    }
    h.settle();
    h.quiesce();
    let before = h.activity.load(Ordering::Relaxed);
    assert!(before > 0, "base derived nothing at all");
    h.feed("id", &[7777], 1);
    h.settle();
    h.quiesce();
    let last = h.activity.load(Ordering::Relaxed);
    assert!(last > before, "wedged (divergent fixpoint)");
    h.finish();
}

/// The bug-5 ddmin-minimal input (a weighted self-loop on a live source, a
/// zero-weight tie arriving in a second epoch) — the exact shape that used to
/// wedge the worker forever, then was merely truncated at the iteration
/// bound. With min aggregating inside the fixpoint loop it CONVERGES: no
/// DEP2_MAX_ITER pin, and the final sssp relation is pinned exactly.
#[test]
fn bug5_minimal_input_converges_exactly() {
    let path = std::path::Path::new(CORPUS_DIR).join("sssp.dl");
    let src = std::fs::read_to_string(&path).unwrap();
    let h = LiveHarness::start(&src, &[], 1);

    // Epoch 1: source 1 with a weighted self-loop (the divergence driver).
    for (rel, row) in [
        ("id", vec![1i64]),
        ("arc", vec![0, 3, 3]),
        ("arc", vec![1, 1, 2]),
        ("arc", vec![2, 1, 0]),
    ] {
        h.feed(rel, &row, 1);
    }
    h.settle();
    h.quiesce();
    for (rel, row) in [
        ("id", vec![2i64]),
        ("id", vec![3]),
        ("arc", vec![2, 3, 0]),
        ("arc", vec![3, 1, 3]),
    ] {
        h.feed(rel, &row, 1);
    }
    h.settle();
    h.quiesce();
    h.feed("id", &[7777], 1);
    h.settle();
    h.quiesce();

    // Exact convergent result: every node with an id is at distance 0 (it is
    // its own source) and nothing else is reachable; the self-loop-generated
    // larger distances must all have been retracted by the in-loop min.
    let base = h.base_acc.lock().unwrap().clone();
    let mut sssp: Vec<(Vec<i64>, isize)> = base
        .iter()
        .filter(|((rel, _), c)| rel == "sssp" && **c != 0)
        .map(|((_, row), c)| (row.clone(), *c))
        .collect();
    sssp.sort();
    let expect: Vec<(Vec<i64>, isize)> = vec![
        (vec![1, 0], 1),
        (vec![2, 0], 1),
        (vec![3, 0], 1),
        (vec![7777, 0], 1),
    ];
    assert_eq!(sssp, expect, "in-loop min must converge to exact minima");
    h.finish();
}

/// Shortest paths over a graph with a positive cycle and a self-loop, where
/// the true distances are non-trivial — the convergence prize the in-loop
/// rewrite was built for. Batch run pinned against hand-computed distances,
/// then the same program fed incrementally with a SECOND epoch adding a
/// cheaper edge, which must retract previously-emitted minima.
#[test]
fn cyclic_sssp_converges_to_exact_distances() {
    // The corpus sssp program, minus its `.input <file>` directives so
    // run_batch's `<rel>.facts` fixtures are picked up.
    let src = r#"
.in
.decl arc(src: number, dest: number, weight: number)
.decl id(src: number)

.printsize
.decl sssp2(x: number, y: number)
.decl sssp(x: number, y: number)

.rule
sssp2(x, min(0)) :- id(x).
sssp2(y, min(d1 + d2)) :- sssp2(x, d1), arc(x, y, d2).
sssp(x, min(d)) :- sssp2(x, d).
"#;

    // 0 -5-> 1, 1 -1-> 2, 2 -1-> 1 (positive cycle), 1 -2-> 1 (self-loop),
    // 2 -10-> 3, 0 -20-> 3. From source 0: d(1)=5, d(2)=6, d(3)=16.
    let arcs1 = vec![
        vec![0i64, 1, 5],
        vec![1, 2, 1],
        vec![2, 1, 1],
        vec![1, 1, 2],
        vec![2, 3, 10],
        vec![0, 3, 20],
    ];
    let batch = run_batch(src, &[("arc", arcs1.clone()), ("id", vec![vec![0]])]);
    let mut got: Vec<Vec<i64>> = batch["sssp"].iter().cloned().collect();
    got.sort();
    assert_eq!(
        got,
        vec![vec![0, 0], vec![1, 5], vec![2, 6], vec![3, 16]],
        "batch cyclic sssp must be exact"
    );

    // Incrementally: same graph, then a cheaper 0 -1-> 2 arrives in a later
    // epoch. d(2) drops 6 -> 1, which feeds back around the 2 -1-> 1 cycle
    // edge so d(1) drops 5 -> 2, and d(3) drops 16 -> 11; every stale
    // minimum must be retracted, not shadowed.
    let h = LiveHarness::start(src, &[], 1);
    h.feed("id", &[0], 1);
    for arc in &arcs1 {
        h.feed("arc", arc, 1);
    }
    h.settle();
    h.quiesce();
    h.feed("arc", &[0, 2, 1], 1);
    h.settle();
    h.quiesce();

    let base = h.base_acc.lock().unwrap().clone();
    let mut sssp: Vec<(Vec<i64>, isize)> = base
        .iter()
        .filter(|((rel, _), c)| rel == "sssp" && **c != 0)
        .map(|((_, row), c)| (row.clone(), *c))
        .collect();
    sssp.sort();
    let expect: Vec<(Vec<i64>, isize)> = vec![
        (vec![0, 0], 1),
        (vec![1, 2], 1),
        (vec![2, 1], 1),
        (vec![3, 11], 1),
    ];
    assert_eq!(sssp, expect, "improving edge must retract stale minima");
    h.finish();
}

/// Delta-debug the bug-5 input down to a minimal wedging case (run manually:
/// DEP2_BUG5_MIN=1 DEP2_DEBUG_STUCK=1, needs the stuck-detector escape hatch
/// so wedged engines can be torn down).
#[test]
fn bug5_minimize() {
    if std::env::var("DEP2_BUG5_MIN").is_err() {
        return;
    }
    let path = std::path::Path::new(CORPUS_DIR).join("sssp.dl");
    let src = std::fs::read_to_string(&path).unwrap();
    let (program, _, _, _) = build(&src);
    let arc_decl = program.edbs().iter().find(|d| d.name() == "arc").unwrap();
    let id_decl = program.edbs().iter().find(|d| d.name() == "id").unwrap();
    let arcs: Vec<Vec<i64>> = {
        let s: HashSet<Vec<i64>> = synth_rows(arc_decl, 8).into_iter().collect();
        let mut v: Vec<Vec<i64>> = s.into_iter().collect();
        v.sort();
        v
    };
    let ids: Vec<Vec<i64>> = {
        let s: HashSet<Vec<i64>> = synth_rows(id_decl, 8).into_iter().collect();
        let mut v: Vec<Vec<i64>> = s.into_iter().collect();
        v.sort();
        v
    };

    // Element = (relation, row, phase). Reproduce the corpus split.
    let mut elems: Vec<(&str, Vec<i64>, u8)> = Vec::new();
    for (i, r) in ids.iter().enumerate() {
        elems.push(("id", r.clone(), if i < 2 { 0 } else { 1 }));
    }
    for (i, r) in arcs.iter().enumerate() {
        elems.push(("arc", r.clone(), if i < 4 { 0 } else { 1 }));
    }

    let wedges = |elems: &[(&str, Vec<i64>, u8)]| -> bool {
        let h = LiveHarness::start(&src, &[], 1);
        for phase in 0..=1u8 {
            for (rel, row, p) in elems {
                if *p == phase {
                    h.feed(rel, row, 1);
                }
            }
            h.settle();
            h.quiesce();
        }
        let out = h.activity.load(Ordering::Relaxed);
        // Novel source, never present in any candidate data: a responsive
        // engine MUST derive sssp2(7777, 0) and bump activity.
        h.feed("id", &[7777], 1);
        h.settle();
        h.quiesce();
        let after = h.activity.load(Ordering::Relaxed);
        let wedged = after <= out;
        h.finish(); // escape hatch makes join succeed even when wedged
        wedged
    };

    assert!(wedges(&elems), "starting point must wedge");
    // Greedy ddmin: try dropping each element; keep drops that preserve the wedge.
    let mut i = 0;
    while i < elems.len() {
        let mut candidate = elems.clone();
        candidate.remove(i);
        if candidate.len() > 1 && wedges(&candidate) {
            elems = candidate;
        } else {
            i += 1;
        }
    }
    eprintln!("[bug5-min] minimal wedge ({} elems):", elems.len());
    for (rel, row, phase) in &elems {
        eprintln!("[bug5-min]   phase{} {}{:?}", phase, rel, row);
    }
}

/// Bug-5 characterization: WHICH epoch-2 increments wedge sssp?
#[test]
fn bug5_characterize() {
    if std::env::var("DEP2_BUG5_MIN").is_err() {
        return;
    }
    let path = std::path::Path::new(CORPUS_DIR).join("sssp.dl");
    let src = std::fs::read_to_string(&path).unwrap();

    let case = |name: &str, p0: &[(&str, Vec<i64>)], p1: &[(&str, Vec<i64>)]| {
        let h = LiveHarness::start(&src, &[], 1);
        for (rel, row) in p0 {
            h.feed(rel, row, 1);
        }
        h.settle();
        h.quiesce();
        for (rel, row) in p1 {
            h.feed(rel, row, 1);
        }
        h.settle();
        h.quiesce();
        let out = h.activity.load(Ordering::Relaxed);
        h.feed("id", &[7777], 1);
        h.settle();
        h.quiesce();
        let wedged = h.activity.load(Ordering::Relaxed) <= out;
        eprintln!("[bug5-char] {:<44} wedged={}", name, wedged);
        h.finish();
    };

    case(
        "id(1) | arc(3,3,0)  self-loop w0 unreachable",
        &[("id", vec![1])],
        &[("arc", vec![3, 3, 0])],
    );
    case(
        "id(1) | arc(0,1,2)  plain arc unreachable",
        &[("id", vec![1])],
        &[("arc", vec![0, 1, 2])],
    );
    case(
        "id(1) | arc(1,2,3)  plain arc REACHABLE",
        &[("id", vec![1])],
        &[("arc", vec![1, 2, 3])],
    );
    case(
        "id(1) | arc(3,3,1)  self-loop w1 unreachable",
        &[("id", vec![1])],
        &[("arc", vec![3, 3, 1])],
    );
    case(
        "id(1) | id(2)       id only",
        &[("id", vec![1])],
        &[("id", vec![2])],
    );
    case(
        "arc(5,6,1) | arc(3,3,0)  no ids at all",
        &[("arc", vec![5, 6, 1])],
        &[("arc", vec![3, 3, 0])],
    );
    case(
        "id(1),arc(3,3,0) together | nothing",
        &[("id", vec![1]), ("arc", vec![3, 3, 0])],
        &[],
    );

    // Round 2: self-loops on LIVE sources arriving in a later epoch — the
    // shape the ddmin oracle accidentally used as its responsiveness probe.
    case(
        "id(1) | id(99)+arc(99,99,1) fresh source+loop",
        &[("id", vec![1])],
        &[("id", vec![99]), ("arc", vec![99, 99, 1])],
    );
    case(
        "id(1) | arc(1,1,2) loop on EXISTING source",
        &[("id", vec![1])],
        &[("arc", vec![1, 1, 2])],
    );
    case(
        "id(99)+arc(99,99,1) single epoch (control)",
        &[("id", vec![99]), ("arc", vec![99, 99, 1])],
        &[],
    );
    case(
        "arc(5,6,1) | id(99)+arc(99,99,1)",
        &[("arc", vec![5, 6, 1])],
        &[("id", vec![99]), ("arc", vec![99, 99, 1])],
    );

    if std::env::var("DEP2_BUG5_MINCASE").is_ok() {
        // The ddmin-minimal 8-element wedge, alone (for stream dumps).
        case(
            "MINIMAL",
            &[
                ("id", vec![1]),
                ("arc", vec![0, 3, 3]),
                ("arc", vec![1, 1, 2]),
                ("arc", vec![2, 1, 0]),
            ],
            &[
                ("id", vec![2]),
                ("id", vec![3]),
                ("arc", vec![2, 3, 0]),
                ("arc", vec![3, 1, 3]),
            ],
        );
    }
}

/// In-loop aggregation shape matrix: which head shapes assemble and compute?
#[test]
fn inloop_agg_shape_matrix() {
    let cases: &[(&str, &str)] = &[
        (
            "seed-const + rec-var",
            "\
.in
.decl id(x: number)
.decl edge(x: number, y: number)
.printsize
.decl s(x: number, c: number)
.rule
s(X, min(0)) :- id(X).
s(Y, min(C)) :- s(X, C), edge(X, Y).
",
        ),
        (
            "seed-var + rec-expr",
            "\
.in
.decl id(x: number)
.decl edge(x: number, y: number)
.printsize
.decl s(x: number, c: number)
.rule
s(X, min(X)) :- id(X).
s(Y, min(C + 1)) :- s(X, C), edge(X, Y).
",
        ),
        (
            "seed-const + rec-expr (sssp shape)",
            "\
.in
.decl id(x: number)
.decl edge(x: number, y: number)
.printsize
.decl s(x: number, c: number)
.rule
s(X, min(0)) :- id(X).
s(Y, min(C + 1)) :- s(X, C), edge(X, Y).
",
        ),
    ];
    for (name, prog) in cases {
        let result = std::panic::catch_unwind(|| {
            run_batch(
                prog,
                &[
                    ("id", vec![vec![0]]),
                    ("edge", vec![vec![0, 1], vec![1, 2]]),
                ],
            )
        });
        let got = result.unwrap_or_else(|_| panic!("{name}: assembly panicked"));
        let mut v: Vec<Vec<i64>> = got["s"].iter().cloned().collect();
        v.sort();
        let expect: Vec<Vec<i64>> = if name.contains("rec-var") {
            // min propagates the seed constant everywhere.
            vec![vec![0, 0], vec![1, 0], vec![2, 0]]
        } else {
            // hop counts along 0 -> 1 -> 2.
            vec![vec![0, 0], vec![1, 1], vec![2, 2]]
        };
        assert_eq!(v, expect, "{name}");
    }
}

// ---------------------------------------------------------------------------
// Computed join keys (strata::rewrite::materialize_computed_join_keys):
// expression-equality predicates must plan as joins, with semantics identical
// to the per-pair compare they replace.
// ---------------------------------------------------------------------------

/// `before_last(D, ".") = P` connects src and files only through a computed
/// string — the rewrite joins them on a materialized key column.
const JK_STEM_PROGRAM: &str = "\
.in
.decl src(f: string, p: string)
.decl files(d: string)

.printsize
.decl link(f: string, d: string)

.rule
link(F, D) :- src(F, P), files(D), before_last(D, \".\") = P.
";

/// Both sides computed: sibling files joined on their directory.
const JK_SIBLING_PROGRAM: &str = "\
.in
.decl fa(a: string)
.decl fb(b: string)

.printsize
.decl sib(a: string, b: string)

.rule
sib(A, B) :- fa(A), fb(B), before_last(A, \"/\") = before_last(B, \"/\").
";

fn ref_before_last(s: &str, sep: &str) -> String {
    match s.rfind(sep) {
        Some(i) => s[..i].to_string(),
        None => s.to_string(),
    }
}

fn path_strategy() -> impl Strategy<Value = Vec<String>> {
    // Slash/dot-structured names from a tiny alphabet so computed keys collide
    // often (the interesting case for a join).
    let seg = prop::sample::select(vec!["a", "b", "c"]);
    let path = (
        seg.clone(),
        seg.clone(),
        prop::sample::select(vec!["x", "y"]),
    )
        .prop_map(|(d, f, e)| format!("{}/{}.{}", d, f, e));
    prop::collection::vec(path, 0..12)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Engine result == per-pair reference for the stem-join shape.
    #[test]
    fn batch_computed_key_join_matches_reference(
        srcs in prop::collection::vec(("[abc]", "[abc]/[abc]"), 0..10),
        files in path_strategy(),
    ) {
        let src_set: HashSet<(String, String)> = srcs.iter().cloned().collect();
        let file_set: HashSet<String> = files.iter().cloned().collect();
        let src_rows: Vec<Vec<String>> =
            src_set.iter().map(|(f, p)| vec![f.clone(), p.clone()]).collect();
        let file_rows: Vec<Vec<String>> = file_set.iter().map(|d| vec![d.clone()]).collect();
        let got = run_batch_typed(JK_STEM_PROGRAM, &[("src", src_rows), ("files", file_rows)]);
        let expect: HashSet<Vec<String>> = src_set
            .iter()
            .flat_map(|(f, p)| {
                file_set.iter().filter_map(move |d| {
                    (ref_before_last(d, ".") == *p).then(|| vec![f.clone(), d.clone()])
                })
            })
            .collect();
        prop_assert_eq!(got["link"].clone(), expect);
    }

    /// Engine result == per-pair reference when BOTH sides are computed.
    #[test]
    fn batch_computed_key_both_sides_matches_reference(
        fa in path_strategy(),
        fb in path_strategy(),
    ) {
        let fa_set: HashSet<String> = fa.iter().cloned().collect();
        let fb_set: HashSet<String> = fb.iter().cloned().collect();
        let fa_rows: Vec<Vec<String>> = fa_set.iter().map(|a| vec![a.clone()]).collect();
        let fb_rows: Vec<Vec<String>> = fb_set.iter().map(|b| vec![b.clone()]).collect();
        let got = run_batch_typed(JK_SIBLING_PROGRAM, &[("fa", fa_rows), ("fb", fb_rows)]);
        let expect: HashSet<Vec<String>> = fa_set
            .iter()
            .flat_map(|a| {
                fb_set.iter().filter_map(move |b| {
                    (ref_before_last(a, "/") == ref_before_last(b, "/"))
                        .then(|| vec![a.clone(), b.clone()])
                })
            })
            .collect();
        prop_assert_eq!(got["sib"].clone(), expect);
    }
}

/// The NULL guard end to end: `X / 0` is NULL, and a comparison involving
/// NULL is ALWAYS false — even against a stored NULL sentinel. Without the
/// helper's null filter the materialized key would be the sentinel value and
/// JOIN with a stored sentinel, resurrecting exactly the pairs the original
/// compare rejected.
#[test]
fn computed_key_null_never_joins() {
    let program = "\
.in
.decl a(x: number)
.decl b(y: number)

.printsize
.decl q(x: number, y: number)

.rule
q(X, Y) :- a(X), b(Y), X / 0 = Y.
";
    let got = run_batch(
        program,
        &[
            ("a", vec![vec![4], vec![7]]),
            // i64::MIN is the engine's NULL sentinel; a stored one must still
            // never match a computed NULL.
            ("b", vec![vec![i64::MIN], vec![3]]),
        ],
    );
    assert!(
        got["q"].is_empty(),
        "NULL keys must never join, got {:?}",
        got["q"]
    );
}

// ---------------------------------------------------------------------------
// Antijoin multiplicities under duplicate pre-negation keys (the shape of
// upstream flowlog's 51cdf0b fix: distinct must come AFTER projection).
// A derived relation is a SET: q(X) :- e(X, Y), !f(X) must carry X exactly
// once no matter how many Y witnesses exist. Set-level comparisons cannot
// see a violation, so observe through count(X) grouped by X — under set
// semantics every count is exactly 1.
// ---------------------------------------------------------------------------

const ANTIJOIN_DUP_PROGRAM: &str = "\
.in
.decl e(x: number, y: number)
.decl g(y: number)
.decl f(x: number)

.printsize
.decl q(x: number)
.decl q2(x: number)
.decl qc(x: number, c: number)
.decl q2c(x: number, c: number)

.rule
q(X) :- e(X, _), !f(X).
q2(X) :- e(X, Y), g(Y), !f(X).
qc(X, count(X)) :- q(X).
q2c(X, count(X)) :- q2(X).
";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn batch_antijoin_dup_keys_have_multiplicity_one(
        e in prop::collection::vec((0i64..5, 0i64..6), 0..14),
        g in prop::collection::vec(0i64..6, 0..5),
        f in prop::collection::vec(0i64..5, 0..4),
    ) {
        let e_set: HashSet<(i64, i64)> = e.iter().cloned().collect();
        let g_set: HashSet<i64> = g.iter().cloned().collect();
        let f_set: HashSet<i64> = f.iter().cloned().collect();
        let got = run_batch(
            ANTIJOIN_DUP_PROGRAM,
            &[
                ("e", e_set.iter().map(|&(x, y)| vec![x, y]).collect()),
                ("g", g_set.iter().map(|&y| vec![y]).collect()),
                ("f", f_set.iter().map(|&x| vec![x]).collect()),
            ],
        );
        let q_ref: HashSet<i64> = e_set
            .iter()
            .filter(|(x, _)| !f_set.contains(x))
            .map(|&(x, _)| x)
            .collect();
        let q2_ref: HashSet<i64> = e_set
            .iter()
            .filter(|(x, y)| g_set.contains(y) && !f_set.contains(x))
            .map(|&(x, _)| x)
            .collect();
        prop_assert_eq!(
            got["q"].clone(),
            q_ref.iter().map(|&x| vec![x]).collect::<HashSet<_>>()
        );
        // The multiplicity oracle: count-per-key over a SET is always 1.
        prop_assert_eq!(
            got["qc"].clone(),
            q_ref.iter().map(|&x| vec![x, 1]).collect::<HashSet<_>>(),
            "q must carry each key exactly once (dup Y witnesses leaked?)"
        );
        prop_assert_eq!(
            got["q2c"].clone(),
            q2_ref.iter().map(|&x| vec![x, 1]).collect::<HashSet<_>>(),
            "join-induced duplicates must not leak through the antijoin"
        );
    }
}

/// Retraction dynamics with duplicate witnesses: removing ONE of several Y
/// witnesses must not retract q(X); removing the last one must; a late f(X)
/// must retract it too. The harness's raw diff accumulator is the
/// multiplicity-visible channel (a set-semantics head accumulates to exactly
/// +1 while any witness remains).
#[test]
fn antijoin_partial_retraction_keeps_the_row() {
    let h = LiveHarness::start(ANTIJOIN_DUP_PROGRAM, &["e", "g", "f"], 1);
    h.feed("e", &[1, 10], 1);
    h.feed("e", &[1, 20], 1);
    h.feed("e", &[2, 30], 1);
    h.settle();
    h.quiesce();

    let count_of = |h: &LiveHarness, rel: &str, row: &[i64]| -> isize {
        h.base_acc
            .lock()
            .unwrap()
            .get(&(rel.to_string(), row.to_vec()))
            .copied()
            .unwrap_or(0)
    };
    assert_eq!(count_of(&h, "q", &[1]), 1, "set head must accumulate to +1");
    assert_eq!(count_of(&h, "q", &[2]), 1);

    // Retract one of two witnesses: q(1) stays, still exactly once.
    h.feed("e", &[1, 10], -1);
    h.settle();
    h.quiesce();
    assert_eq!(
        count_of(&h, "q", &[1]),
        1,
        "one remaining witness must keep q(1) at exactly +1"
    );

    // Retract the last witness: q(1) gone.
    h.feed("e", &[1, 20], -1);
    h.settle();
    h.quiesce();
    assert_eq!(count_of(&h, "q", &[1]), 0, "last witness gone => row gone");

    // A late f(2) retracts q(2) even though e(2, 30) remains.
    h.feed("f", &[2], 1);
    h.settle();
    h.quiesce();
    assert_eq!(
        count_of(&h, "q", &[2]),
        0,
        "negation must retract on late f"
    );
    h.finish();
}

const AVG_PROGRAM: &str = "\
.in
.decl m(s: number, v: number)
.input m.facts

.printsize
.decl a(s: number, m: number)

.rule
a(S, avg(V)) :- m(S, V).
";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Grouped avg through the whole engine == i128-widened truncated mean.
    #[test]
    fn batch_avg_matches_reference(
        rows in prop::collection::vec((0i64..5, -1000i64..1000), 1..30),
    ) {
        let set: HashSet<(i64, i64)> = rows.iter().cloned().collect();
        let got = run_batch(
            AVG_PROGRAM,
            &[("m", set.iter().map(|&(s, v)| vec![s, v]).collect())],
        );
        let mut groups: HashMap<i64, Vec<i64>> = HashMap::new();
        for &(s, v) in &set {
            groups.entry(s).or_default().push(v);
        }
        let expect: HashSet<Vec<i64>> = groups
            .into_iter()
            .map(|(s, vs)| {
                let sum: i128 = vs.iter().map(|&v| v as i128).sum();
                vec![s, (sum / vs.len() as i128) as i64]
            })
            .collect();
        prop_assert_eq!(got["a"].clone(), expect);
    }
}

/// Two "spanning equalities" in one rule — two variables that each join a pair
/// of otherwise-unconnected atoms — used to panic every worker.
///
/// The cause was a budget mismatch in the fat-mode decision, not the join
/// planner proper. A plain row may be `ROW_MAX` wide, but a key/value
/// arrangement is backed by a generated `dict_K_V` type and those are generated
/// over `KV_MAX` squared, so its VALUE is bounded by `KV_MAX` as well.
/// `should_use_fat_mode` compared a kv's value against the row budget, so a
/// join output of arity (1, 5) passed: wide enough to build via
/// `arrange_double`, with no `codegen_jn` arm to join it. The next join over it
/// panicked with "codegen_jn unimplemented for 1, 5, 1, 6".
///
/// One spanning equality stays under the budget, which is why it worked and
/// made this look like a planner bug at first.
#[test]
fn two_spanning_equalities_join_correctly() {
    const PROG: &str = "\
.in
.decl node(t: number, op: number, a: number, b: number)
.input node.facts
.decl lead(t: number, rep: number)
.input lead.facts

.printsize
.decl c(s: number, t: number)

.rule
c(S, T) :- node(S, Op, A1, A2), node(T, Op, B1, B2),
    lead(A1, L1), lead(B1, L1),
    lead(A2, L2), lead(B2, L2).
";
    let nodes = vec![vec![1, 0, 3, 4], vec![2, 0, 3, 4]];
    let leads = vec![vec![3, 3], vec![4, 4]];
    let got = run_batch(PROG, &[("node", nodes), ("lead", leads)]);
    // Both nodes share op and pointwise-equal children, so each pairs with both.
    let mut pairs: Vec<(i64, i64)> = got["c"].iter().map(|r| (r[0], r[1])).collect();
    pairs.sort();
    assert_eq!(pairs, vec![(1, 1), (1, 2), (2, 1), (2, 2)]);
}

/// The `examples/` programs are written for `--source`; the batch harness loads
/// EDBs from `.facts` files, so add those annotations here rather than clutter
/// the examples with test scaffolding.
fn with_facts_inputs(src: &str, edbs: &[&str]) -> String {
    let mut out = String::new();
    for line in src.lines() {
        out.push_str(line);
        out.push('\n');
        for e in edbs {
            if line.trim_start().starts_with(&format!(".decl {}(", e)) {
                out.push_str(&format!(".input {}.facts\n", e));
            }
        }
    }
    out
}

/// Steensgaard points-to analysis, from `examples/egraph/steensgaard.dl`, run
/// forwards and then BACKWARDS. This is the motivating use case: a unification
/// analysis that responds to source edits, which a union-find cannot do.
///
/// Program (a=1 b=2 c=3 d=4 x=5 y=6):  a = &x;  b = &y;  c = a;  d = *c
/// then `b = a` is added and retracted.
#[test]
fn steensgaard_points_to_survives_editing_the_program() {
    const EDBS: [&str; 4] = ["stmt_addr", "stmt_assign", "stmt_load", "stmt_store"];
    let prog = with_facts_inputs(
        include_str!("../../../examples/egraph/steensgaard.dl"),
        &EDBS,
    );
    let prog = prog.as_str();
    let addr = vec![vec![1, 5], vec![2, 6]];
    let load = vec![vec![4, 3]];
    let aliases = |got: &HashMap<String, HashSet<Vec<i64>>>| -> Vec<(i64, i64)> {
        let mut v: Vec<(i64, i64)> = got["may_alias"].iter().map(|r| (r[0], r[1])).collect();
        v.sort();
        v
    };

    // Baseline: `c = a` only. `a`/`c` alias; the load makes `d`/`x` share a
    // pointee class.
    let base = run_batch(
        prog,
        &[
            ("stmt_addr", addr.clone()),
            ("stmt_assign", vec![vec![3, 1]]),
            ("stmt_load", load.clone()),
            ("stmt_store", vec![]),
        ],
    );
    assert_eq!(aliases(&base), vec![(1, 3), (4, 5)], "a~c and d~x");

    // Adding `b = a` unifies x with y, and Steensgaard's imprecision collapses
    // everything into one alias set.
    let collapsed = run_batch(
        prog,
        &[
            ("stmt_addr", addr.clone()),
            ("stmt_assign", vec![vec![3, 1], vec![2, 1]]),
            ("stmt_load", load.clone()),
            ("stmt_store", vec![]),
        ],
    );
    assert_eq!(
        aliases(&collapsed),
        vec![(1, 2), (1, 3), (2, 3), (4, 5), (4, 6), (5, 6)],
        "one statement collapses the whole alias set"
    );

    // Retract `b = a` incrementally: the classes must split back to the
    // baseline, not merely stop growing.
    let mut ins: Vec<(&str, Vec<i64>)> = addr.iter().map(|r| ("stmt_addr", r.clone())).collect();
    ins.extend(load.iter().map(|r| ("stmt_load", r.clone())));
    ins.push(("stmt_assign", vec![3, 1]));
    ins.push(("stmt_assign", vec![2, 1]));
    let streamed = run_streaming(prog, &EDBS, &ins, &[("stmt_assign", vec![2, 1])]);
    let mut after: Vec<(i64, i64)> = streamed
        .get("may_alias")
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|r| (r[0], r[1]))
        .collect();
    after.sort();
    assert_eq!(
        after,
        vec![(1, 3), (4, 5)],
        "retracting `b = a` splits the classes back to the baseline"
    );
}

/// Why the `leader` fold carries a pointer-jumping rule that is logically
/// REDUNDANT — and what happens without it.
///
/// With a cyclic term table the congruence justifying a merge can rest on that
/// merge. Term 4 is `op1(4, 5)` and term 1 is `op1(4, 5)`: they look congruent,
/// but the instant they merge, 4's canonical form changes (its child `4` now
/// canonicalizes to `1`), so the form that justified the merge is gone —
/// replaced by one that justifies it again. The support is circular, and this
/// recursion is not monotone (lowering a leader RETRACTS the old `cnode` row),
/// so "the least fixpoint" does not pin down an answer. More than one stable
/// state exists.
///
/// Propagating one hop at a time settles on the state where the merge is
/// refused. Adding `leader(X,L) :- leader(X,M), leader(M,L)` — which derives
/// nothing new in a monotone reading — settles on the state where it holds,
/// which is what a destructive union-find computes. So the jump rule is not
/// only an optimization (it also takes retraction from O(N²) to O(N) on long
/// chains); it selects the classical e-graph's answer.
///
/// Pinned in both directions so neither can drift silently.
#[test]
fn pointer_jumping_selects_the_union_find_fixpoint() {
    // 4 = op1(4, 5) is its own child; 1 = op1(4, 5) has the same form.
    let node_set: HashSet<(i64, i64, i64, i64)> =
        [(4, 1, 4, 5), (1, 1, 4, 5)].into_iter().collect();
    let node_rows: Vec<Vec<i64>> = node_set
        .iter()
        .map(|&(t, o, a, b)| vec![t, o, a, b])
        .collect();

    let sorted = |got: &HashMap<String, HashSet<Vec<i64>>>| -> Vec<(i64, i64)> {
        let mut v: Vec<(i64, i64)> = got["leader"].iter().map(|r| (r[0], r[1])).collect();
        v.sort();
        v
    };

    // The shipped program, which carries the jump rule.
    let with_jump = run_batch(
        EGRAPH_PROGRAM,
        &[("node", node_rows.clone()), ("eq_input", vec![])],
    );

    // The same program with the jump rule removed.
    let one_hop_src = EGRAPH_PROGRAM.replace("leader(X, L) :- leader(X, M), leader(M, L).\n", "");
    assert_ne!(
        one_hop_src, EGRAPH_PROGRAM,
        "jump rule should be present to remove"
    );
    let one_hop = run_batch(&one_hop_src, &[("node", node_rows), ("eq_input", vec![])]);

    let mut oracle: Vec<(i64, i64)> = reference_congruence(&node_set, &HashSet::new())
        .iter()
        .map(|r| (r[0], r[1]))
        .collect();
    oracle.sort();

    assert_eq!(
        oracle,
        vec![(1, 1), (4, 1), (5, 5)],
        "union-find merges them"
    );
    assert_eq!(
        sorted(&with_jump),
        oracle,
        "with pointer jumping the encoding agrees with the union-find"
    );
    assert_eq!(
        sorted(&one_hop),
        vec![(1, 1), (4, 4), (5, 5)],
        "one hop at a time settles on the state that refuses the self-supporting merge"
    );
}
