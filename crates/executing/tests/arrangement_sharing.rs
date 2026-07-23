//! Arrangement sharing: transformation output signatures are
//! content-canonical, so the dataflow assembly reuses an already-built
//! collection/arrangement instead of rebuilding it (dataflow.rs sharing
//! guards). Correctness of shared plans is covered end-to-end by the corpus
//! copy-query sweep (borrow/z3/cvc5/doop-family all contain duplicates);
//! these tests pin (a) that the corpus really exercises the path and (b) an
//! exact-result case with shared leaves in both stratum kinds.

use planning::program::ProgramQueryPlan;
use std::collections::{HashMap, HashSet};
use strata::stratification::Strata;

fn duplicate_count(path: &std::path::Path) -> usize {
    let src = std::fs::read_to_string(path).unwrap();
    let program = syntax::parse(&src).unwrap();
    let strata = Strata::from_parser(program);
    let plan = ProgramQueryPlan::from_strata(&strata, false, None);
    let mut by_name: HashMap<String, usize> = HashMap::new();
    for group in plan.program_plan() {
        for t in group.strata_plan() {
            *by_name
                .entry(t.output().signature().name().to_string())
                .or_default() += 1;
        }
    }
    by_name.values().filter(|c| **c > 1).map(|c| *c - 1).sum()
}

/// The corpus contains real duplicate transformations (identical join leaves
/// planned by several rules), so the sweep genuinely executes the sharing
/// guards. If planning ever de-duplicates upstream of assembly, this count
/// dropping to zero is fine — the assertion then documents that the guards
/// became dead and can go.
#[test]
fn corpus_plans_contain_shared_arrangements() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/flowlog_programs");
    let interesting = ["borrow.dl", "z3.dl", "cvc5.dl", "batik.dl", "crdt_slow.dl"];
    let total: usize = interesting
        .iter()
        .map(|n| duplicate_count(&std::path::Path::new(dir).join(n)))
        .sum();
    assert!(
        total >= 10,
        "expected the corpus to exercise arrangement sharing, got {total} duplicates"
    );
}

/// Exact results with shared leaves: two non-recursive rules share identical
/// e/f join leaves, and a recursive stratum re-arranges e in-scope (the
/// entered-from-outer exemption path).
#[test]
fn shared_leaves_produce_exact_results() {
    let program = "\
.in
.decl e(x: number, y: number)
.decl f(x: number, y: number)

.printsize
.decl a(x: number, y: number)
.decl b(x: number, y: number)
.decl tc(x: number, y: number)

.rule
a(X, Z) :- e(X, Y), f(Y, Z).
b(X, Z) :- e(X, Y), f(Y, Z), X != Z.
tc(X, Y) :- e(X, Y).
tc(X, Z) :- tc(X, Y), e(Y, Z).
";
    let e: HashSet<(i64, i64)> = [(1, 2), (2, 3), (3, 1), (4, 4)].into();
    let f: HashSet<(i64, i64)> = [(2, 5), (3, 1), (1, 4)].into();

    let a_ref: HashSet<Vec<i64>> = e
        .iter()
        .flat_map(|&(x, y)| {
            f.iter()
                .filter(move |&&(fy, _)| fy == y)
                .map(move |&(_, z)| vec![x, z])
        })
        .collect();
    let b_ref: HashSet<Vec<i64>> = a_ref.iter().filter(|r| r[0] != r[1]).cloned().collect();
    let mut tc_ref: HashSet<(i64, i64)> = e.clone();
    loop {
        let next: HashSet<(i64, i64)> = tc_ref
            .iter()
            .flat_map(|&(x, y)| {
                e.iter()
                    .filter(move |&&(ey, _)| ey == y)
                    .map(move |&(_, z)| (x, z))
            })
            .collect();
        let before = tc_ref.len();
        tc_ref.extend(next);
        if tc_ref.len() == before {
            break;
        }
    }

    // Reuse the shared batch harness from properties.rs is not possible across
    // integration-test binaries; a minimal local runner mirrors it.
    let got = run_batch_local(
        program,
        &[
            ("e", e.iter().map(|&(x, y)| vec![x, y]).collect()),
            ("f", f.iter().map(|&(x, y)| vec![x, y]).collect()),
        ],
    );
    assert_eq!(got["a"], a_ref);
    assert_eq!(got["b"], b_ref);
    assert_eq!(
        got["tc"],
        tc_ref
            .iter()
            .map(|&(x, y)| vec![x, y])
            .collect::<HashSet<_>>()
    );
}

fn run_batch_local(
    program_dl: &str,
    edbs: &[(&str, Vec<Vec<i64>>)],
) -> HashMap<String, HashSet<Vec<i64>>> {
    use catalog::head::aggregation_catalog_from_program;
    use executing::arg::Args;
    use executing::dataflow::program_execution;
    use reading::config::{KV_MAX, ROW_MAX};

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
    let program = syntax::parse(program_dl).unwrap();
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

    let mut out: HashMap<String, HashSet<Vec<i64>>> = HashMap::new();
    let csvs = out_dir.join("csvs");
    for decl in program.idbs() {
        let name = decl.name().to_string();
        let prefix = format!("{}.csv", name);
        let mut set = HashSet::new();
        for entry in std::fs::read_dir(&csvs).unwrap().flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname == prefix || fname.starts_with(&prefix) {
                for line in std::fs::read_to_string(entry.path())
                    .unwrap()
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                {
                    set.insert(line.split(',').map(|v| v.trim().parse().unwrap()).collect());
                }
            }
        }
        out.insert(name, set);
    }
    out
}
