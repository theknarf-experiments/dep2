//! Pipeline regression tests over FlowLog's example programs.
//!
//! These are real program-analysis Datalog programs (Andersen/context-sensitive
//! pointer analysis, CRDTs, transitive closure, ...). Their input facts aren't
//! bundled, so we run each through the FULL engine pipeline (parse -> stratify ->
//! optimize -> plan -> dataflow assembly) with EMPTY facts and assert it builds
//! and runs to completion without error. This guards the engine against
//! regressions on a corpus of rich, real-world programs.

use std::path::Path;

use catalog::head::aggregation_catalog_from_program;
use executing::arg::Args;
use executing::dataflow::program_execution;
use parsing::parser::Program;
use planning::program::ProgramQueryPlan;
use reading::{KV_MAX, ROW_MAX};
use strata::stratification::Strata;

const PROGRAMS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/flowlog_programs");

/// Run one example program through the full batch pipeline with empty inputs.
fn run_example(dl_path: &Path) {
    let dir = tempfile::tempdir().unwrap();
    let facts_dir = dir.path().join("facts");
    let out_dir = dir.path().join("out");
    std::fs::create_dir_all(&facts_dir).unwrap();
    std::fs::create_dir_all(out_dir.join("csvs")).unwrap();

    let program = Program::parse_from(dl_path.to_str().unwrap());
    // A real program: it declares rules. (Catches a silently-empty parse.)
    assert!(
        !program.rules().is_empty(),
        "{}: parsed program has no rules",
        dl_path.display()
    );

    // Empty fact file per EDB so the batch loader finds each input. The loader
    // resolves an EDB to `<facts>/<.input path>` when the program gives one (e.g.
    // `.input Arc.csv`), else `<facts>/<name>.facts` — mirror that here.
    for decl in program.edbs() {
        let rel_file = decl
            .path()
            .unwrap_or_else(|| format!("{}.facts", decl.name()));
        let p = facts_dir.join(&rel_file);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, "").unwrap();
    }

    let strata = Strata::from_parser(program.clone());
    let plan = ProgramQueryPlan::from_strata(&strata, false, None);
    let fat = plan.should_use_fat_mode(false, KV_MAX, ROW_MAX);
    let idb_map = aggregation_catalog_from_program(&program);

    let args = Args::new(
        dl_path.to_string_lossy().into_owned(),
        facts_dir.to_string_lossy().into_owned(),
        Some(out_dir.to_string_lossy().into_owned()),
        ",".to_string(),
        1,
    );
    program_execution(args, strata, plan.program_plan().to_owned(), fat, idb_map);
}

/// Every example program must run through the full pipeline. Each is isolated with
/// `catch_unwind` so a regression reports *which* programs broke rather than
/// aborting at the first one.
#[test]
fn all_flowlog_examples_run() {
    let mut entries: Vec<_> = std::fs::read_dir(PROGRAMS_DIR)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "dl").unwrap_or(false))
        .collect();
    entries.sort();
    assert!(
        !entries.is_empty(),
        "no example programs found in {PROGRAMS_DIR}"
    );

    let failed: Vec<String> = entries
        .iter()
        .filter(|p| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_example(p))).is_err()
        })
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();

    assert!(
        failed.is_empty(),
        "{} example program(s) failed the pipeline: {}",
        failed.len(),
        failed.join(", ")
    );
}
