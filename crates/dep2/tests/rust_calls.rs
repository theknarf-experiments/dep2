//! Real Rust grammar -> semantic rules -> live call graph, including deletions.
//! Run `mise run setup` first to build the Rust wasm grammar.
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
#[path = "common/call_analysis.rs"]
mod call_analysis;
use call_analysis::Running;

#[test]
fn rust_scopes_receivers_modules_and_live_retraction() {
    let dir = tempfile::tempdir().unwrap();
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_calls");
    let mut definitions = BTreeMap::new();
    let mut expected = BTreeMap::new();
    let mut callers = BTreeMap::new();
    for name in [
        "lib.rs",
        "nested.rs",
        "nested/child.rs",
        "Cargo.toml",
        "buddy/Cargo.toml",
        "buddy/src/lib.rs",
    ] {
        let source = std::fs::read_to_string(fixtures.join(name)).unwrap();
        for (i, line) in source.lines().enumerate() {
            if let Some((_, rest)) = line.split_once("@fn ") {
                definitions.insert(
                    rest.split_whitespace().next().unwrap().to_owned(),
                    (name.to_owned(), (i + 1).to_string()),
                );
            }
            if let Some((_, rest)) = line.split_once("@caller ") {
                callers.insert(
                    (name.to_owned(), (i + 1).to_string()),
                    rest.trim().to_owned(),
                );
            }
            if let Some((_, rest)) = line.split_once("@call ") {
                let targets = rest.split_whitespace().next().unwrap();
                expected.insert(
                    (name.to_owned(), (i + 1).to_string()),
                    if targets == "?" {
                        BTreeSet::new()
                    } else {
                        targets.split(',').map(str::to_owned).collect()
                    },
                );
            }
        }
        std::fs::create_dir_all(dir.path().join(name).parent().unwrap()).unwrap();
        std::fs::write(dir.path().join(name), source).unwrap();
    }
    let engine = Running::start(dir.path(), &[("rs", "rust"), ("toml", "toml")]);
    let verify = |expect: &BTreeMap<(String, String), BTreeSet<String>>| -> Result<(), String> {
        let nodes = engine.rows("function_node");
        let mut ids = BTreeMap::new();
        for (label, (file, line)) in &definitions {
            let matches: Vec<_> = nodes
                .iter()
                .filter(|r| &r[2] == file && &r[3] == line && r[1] != "<module>")
                .collect();
            if matches.len() != 1 {
                return Err(format!("definition {label}: {matches:?}"));
            }
            ids.insert(matches[0][0].clone(), label.clone());
        }
        for node in &nodes {
            if node[1] == "<module>" {
                ids.insert(node[0].clone(), "<module>".into());
            }
        }
        let calls = engine.rows("call_at");
        for ((file, line), want) in &callers {
            let got: BTreeSet<_> = calls
                .iter()
                .filter(|r| &r[0] == file && &r[3] == line)
                .map(|r| ids.get(&r[1]).cloned().unwrap_or_else(|| r[1].clone()))
                .collect();
            if got != BTreeSet::from([want.clone()]) {
                return Err(format!(
                    "{file}:{line}: expected caller {want}, got {got:?}"
                ));
            }
        }
        let unresolved = engine.rows("unresolved_call");
        for ((file, line), want) in expect {
            let got: BTreeSet<String> = calls
                .iter()
                .filter(|r| &r[0] == file && &r[3] == line)
                .map(|r| ids.get(&r[2]).cloned().unwrap_or_else(|| r[2].clone()))
                .collect();
            if &got != want {
                return Err(format!("{file}:{line}: expected {want:?}, got {got:?}"));
            }
            if want.is_empty() && !unresolved.iter().any(|r| &r[0] == file && &r[3] == line) {
                return Err(format!("{file}:{line}: missing unresolved call"));
            }
        }
        Ok(())
    };
    engine.wait_for(|| verify(&expected));
    assert!(engine.rows("unresolved_import").is_empty());
    assert!(!engine.rows("binding").is_empty());
    assert!(!engine.rows("resolves").is_empty());
    assert!(!engine.rows("points_to").is_empty());

    // Replace a constructor argument: the field and its indirect call target
    // must retract, while method dispatch retains the same receiver type.
    let lib = dir.path().join("lib.rs");
    let source = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, source.replace("A::new(left)", "A::new(right)")).unwrap();
    let mut changed = expected.clone();
    for (i, line) in source.lines().enumerate() {
        if line.contains("(self.callback)();") {
            changed.insert(
                ("lib.rs".into(), (i + 1).to_string()),
                BTreeSet::from(["right".into()]),
            );
        }
    }
    engine.wait_for(|| verify(&changed));
    std::fs::write(&lib, &source).unwrap();
    engine.wait_for(|| verify(&expected));

    // Rebinding one import must not retarget other paths into the module.
    std::fs::write(
        &lib,
        source.replace("chosen as imported", "alternate as imported"),
    )
    .unwrap();
    let mut changed = expected.clone();
    for (i, line) in source.lines().enumerate() {
        if line.contains("imported();") {
            changed.insert(
                ("lib.rs".into(), (i + 1).to_string()),
                BTreeSet::from(["nested_alternate".into()]),
            );
        }
    }
    engine.wait_for(|| verify(&changed));
    std::fs::write(&lib, &source).unwrap();
    engine.wait_for(|| verify(&expected));
}
