//! End-to-end: git history streams into the engine and drives a churn
//! aggregation, updating live as new commits land.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use dep2_core::engine::{Dep2, Dep2Config};
use dep2_plugin_git::GitPlugin;

const PROG: &str = "\
.in
.decl commit(id: string, author: string, email: string, time: number, message: string)
.decl commit_file(id: string, file: string, change: string)

.out
.decl churn(file: string, n: number)
.decl authored(author: string, n: number)

.rule
churn(F, count(Id)) :- commit_file(Id, F, _).
authored(A, count(Id)) :- commit(Id, A, _, _, _).
";

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {:?}: {:?}", args, out);
}

fn rows(
    state: &Arc<std::sync::Mutex<dep2_core::engine::RelationState>>,
    rel: &str,
) -> Vec<Vec<i64>> {
    state
        .lock()
        .unwrap()
        .get(rel)
        .map(|m| m.keys().map(|r| r.to_vec()).collect())
        .unwrap_or_default()
}

fn wait_for<F: Fn() -> bool>(cond: F, secs: u64) -> bool {
    for _ in 0..(secs * 20) {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn git_history_feeds_live_churn() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "add a"]);
    std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "touch a and b"]);

    let mut engine = Dep2::with_config(Dep2Config {
        workers: 1,
        print_updates: false,
        publish: true,
    });
    engine.add_plugin(Box::new(GitPlugin));
    let mut config = std::collections::HashMap::new();
    config.insert("root".to_string(), dir.path().display().to_string());
    engine.add_source(None, "git", config);
    engine.load_program(PROG).unwrap();

    let state = engine.state();
    let types = engine.relation_types();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || engine.run(sd));

    let decoded = |rel: &str| -> Vec<(String, i64)> {
        let ty = &types[rel];
        let mut out: Vec<(String, i64)> = rows(&state, rel)
            .iter()
            .map(|r| {
                let d = dep2_core::engine::decode_state_row(r, ty);
                (d[0].clone(), d[1].parse().unwrap())
            })
            .collect();
        out.sort();
        out
    };

    assert!(
        wait_for(|| rows(&state, "churn").len() == 2, 15),
        "expected churn for a.txt and b.txt, got {:?}",
        decoded("churn")
    );
    assert_eq!(
        decoded("churn"),
        vec![("a.txt".into(), 2), ("b.txt".into(), 1)]
    );
    assert_eq!(decoded("authored"), vec![("Ada".into(), 2)]);

    // A new commit lands after the engine is live: poll_changes must pick it
    // up and the aggregates must move.
    std::fs::write(dir.path().join("b.txt"), "bb\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "touch b again"]);

    assert!(
        wait_for(|| decoded("churn").contains(&("b.txt".into(), 2)), 15),
        "live commit should bump b.txt churn, got {:?}",
        decoded("churn")
    );
    assert_eq!(decoded("authored"), vec![("Ada".into(), 3)]);

    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
}
