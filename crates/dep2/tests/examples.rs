//! Every program in `examples/` must still load.
//!
//! The examples are documentation that executes, which is exactly why they rot
//! without anyone noticing: a dialect change or a renamed plugin relation
//! breaks a program that nobody runs again for months.
//!
//! This drives the real `dep2 check` binary rather than building an engine in
//! process. It used to do the latter with no plugins registered, on the
//! reasoning that loading validates a program against its own `.in`
//! declarations and plugins only matter once a source is bound. `.require` made
//! that false: a program now declares which plugins it needs and loading fails
//! without them. Registering a plugin list here would duplicate the binary's
//! and drift from it — and would miss the feature-gated ones entirely, since
//! whether `duckdb` is compiled in is a property of the binary. Running the
//! binary tests what a user actually runs, with whatever features it was built
//! with.

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn dl_files(dir: &PathBuf) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {}", dir.display(), e))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "dl"))
        .collect();
    out.sort();
    out
}

#[test]
fn every_example_program_still_loads() {
    let examples = repo_root().join("examples");
    let mut programs = dl_files(&examples);
    programs.extend(dl_files(&examples.join("egraph")));

    assert!(
        programs.len() > 20,
        "expected to find the example programs, got {} in {} — if this \
         directory moved, this test is silently checking nothing",
        programs.len(),
        examples.display()
    );

    let out = Command::new(env!("CARGO_BIN_EXE_dep2"))
        .arg("check")
        .args(&programs)
        .output()
        .expect("dep2 check runs");

    assert!(
        out.status.success(),
        "{} example programs failed to load:\n{}\n{}",
        programs.len(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
