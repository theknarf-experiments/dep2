//! Every program in `examples/` must still load.
//!
//! The examples are documentation that executes, which is exactly why they rot
//! without anyone noticing: a dialect change or a renamed plugin relation
//! breaks a program that nobody runs again for months. `dep2 check` makes
//! validating them cheap, and this makes it automatic — the whole front end
//! (parser, decl-driven typing, rule safety, stratification, planning) runs
//! before any source is bound or any worker starts, so the cost is a couple of
//! seconds and no network.
//!
//! What this cannot catch is a source whose schema disagrees with a decl, since
//! no source is bound here. `dep2 run` reports that at startup.

use std::path::PathBuf;

use dep2_core::engine::{Dep2, Dep2Config};

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples")
}

#[test]
fn every_example_program_still_loads() {
    let dir = examples_dir();
    let mut programs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {}", dir.display(), e))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "dl"))
        .collect();
    programs.sort();

    assert!(
        programs.len() > 20,
        "expected to find the example programs, got {} in {} — if this \
         directory moved, this test is silently checking nothing",
        programs.len(),
        dir.display()
    );

    let mut failed = Vec::new();
    for path in &programs {
        // A fresh engine per program: loading mutates the catalog, and sharing
        // one would let a later program pass on an earlier program's decls.
        let mut engine = Dep2::with_config(Dep2Config {
            workers: 1,
            print_updates: false,
            publish: false,
        });
        // No plugins are registered, and none are needed: loading validates a
        // program against its own `.in` declarations, and plugins only matter
        // once a source is bound. Registering them here would couple this test
        // to the binary's plugin list for no gain.
        if let Err(e) = engine.load_program_file(path) {
            failed.push(format!(
                "{}: {}",
                path.file_name().unwrap().to_string_lossy(),
                e
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} example programs failed to load (labelled reports above):\n  {}",
        failed.len(),
        programs.len(),
        failed.join("\n  ")
    );
}
