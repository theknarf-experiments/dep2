//! Real JS/TS grammar -> semantic rules -> live call graph, including deletions.
//! Run `mise run setup` first to build the JS/TS wasm grammars.
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
#[path = "common/call_analysis.rs"]
mod call_analysis;
use call_analysis::Running;

#[test]
fn scoped_points_to_calls_and_live_retraction() {
    let dir = tempfile::tempdir().unwrap();
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/calls");
    let mut definitions = BTreeMap::new();
    let mut expected = BTreeMap::new();
    let mut callers = BTreeMap::new();
    for name in [
        "main.ts",
        "lib.ts",
        "barrel.ts",
        "star.ts",
        "plain.js",
        "view.tsx",
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
        std::fs::write(dir.path().join(name), source).unwrap();
    }
    let engine = Running::start(
        dir.path(),
        &[("js", "javascript"), ("ts", "typescript"), ("tsx", "tsx")],
    );
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

    // Same structural sites, different pointee: edges to left must retract,
    // including indirect aliases and destructuring, while b stays independent.
    let main = dir.path().join("main.ts");
    let source = std::fs::read_to_string(&main).unwrap();
    std::fs::write(
        &main,
        source.replace("const a = { run: left }", "const a = { run: right }"),
    )
    .unwrap();
    let mut changed = expected.clone();
    for (i, line) in source.lines().enumerate() {
        if line.contains("a.run();")
            || line.contains("alias.run();")
            || line.contains("extracted();")
        {
            // Only the object literal's a.run, not the class instance's call.
            let key = ("main.ts".to_owned(), (i + 1).to_string());
            if changed.get(&key).is_some_and(|v| v.contains("left")) {
                changed.insert(key, BTreeSet::from(["right".into()]));
            }
        }
    }
    engine.wait_for(|| verify(&changed));
    std::fs::write(&main, &source).unwrap();
    engine.wait_for(|| verify(&expected));

    // Removing an inner binding exposes the outer one. Re-introducing it
    // must retract that resolution, even though the outer function survives.
    std::fs::write(
        &main,
        source.replace(
            "function same() {} // @fn inner_same",
            "function renamed() {} // @fn inner_same",
        ),
    )
    .unwrap();
    let mut unshadowed = expected.clone();
    for targets in unshadowed.values_mut() {
        if targets.remove("inner_same") {
            targets.insert("outer_same".into());
        }
    }
    engine.wait_for(|| verify(&unshadowed));
    std::fs::write(&main, &source).unwrap();
    engine.wait_for(|| verify(&expected));
}
