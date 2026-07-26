//! Plugin-level tests against real temp repositories (shell `git` fixtures).

use std::collections::HashMap;
use std::path::Path;

use dep2_plugin::{DataValue, Plugin, PluginContext, ValueSink};

mod common;
use common::git;

fn fixture_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "add a"]);
    std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/c.txt"), "c\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(
        dir.path(),
        &["commit", "-q", "-m", "change a, add b and sub/c"],
    );
    dir
}

#[derive(Default)]
struct Collect {
    rows: Vec<(String, Vec<DataValue>, isize)>,
}
impl ValueSink for Collect {
    fn push(&mut self, relation: &str, row: &[DataValue], diff: isize) {
        self.rows.push((relation.to_string(), row.to_vec(), diff));
    }
}

fn open_source(root: &Path) -> Box<dyn dep2_plugin::StreamingDataSource> {
    let mut ctx = PluginContext::new();
    dep2_plugin_git::GitPlugin.setup(&mut ctx);
    let provider = ctx.get_streaming_data_provider("git").unwrap();
    let mut config = HashMap::new();
    config.insert("root".to_string(), root.display().to_string());
    provider.open_stream(&config).unwrap()
}

fn s(v: &DataValue) -> String {
    match v {
        DataValue::String(s) => s.clone(),
        DataValue::Str(s) => s.to_string(),
        other => format!("{:?}", other),
    }
}

#[test]
fn seeds_history_and_emits_commit_rows() {
    let dir = fixture_repo();
    let source = open_source(dir.path());

    let units = source.seed_units();
    assert_eq!(units.len(), 2, "two commits, newest first");

    let mut sink = Collect::default();
    let mut worker = source.open();
    for unit in &units {
        worker.ingest(unit, &mut sink);
    }

    let commits: Vec<_> = sink.rows.iter().filter(|(r, _, _)| r == "commit").collect();
    assert_eq!(commits.len(), 2);
    // Every commit row carries the fixture author and a subject line.
    for (_, row, diff) in &commits {
        assert_eq!(*diff, 1);
        assert_eq!(s(&row[1]), "Ada");
        assert_eq!(s(&row[2]), "ada@example.com");
        assert!(matches!(row[3], DataValue::Integer(t) if t > 0));
        assert!(!s(&row[4]).is_empty());
    }

    // The DAG: newest commit has one parent (the oldest has none).
    let parents: Vec<_> = sink
        .rows
        .iter()
        .filter(|(r, _, _)| r == "commit_parent")
        .collect();
    assert_eq!(parents.len(), 1);
    assert_eq!(s(&parents[0].1[0]), units[0]);
    assert_eq!(s(&parents[0].1[1]), units[1]);

    // File changes: root commit adds a.txt; second modifies a.txt + adds b.txt.
    let files: Vec<(String, String, String)> = sink
        .rows
        .iter()
        .filter(|(r, _, _)| r == "commit_file")
        .map(|(_, row, _)| (s(&row[0]), s(&row[1]), s(&row[2])))
        .collect();
    assert!(files.contains(&(units[1].clone(), "a.txt".into(), "A".into())));
    assert!(files.contains(&(units[0].clone(), "a.txt".into(), "M".into())));
    assert!(files.contains(&(units[0].clone(), "b.txt".into(), "A".into())));
    // Blobs only: nested files appear by full path, the directory itself never.
    assert!(files.contains(&(units[0].clone(), "sub/c.txt".into(), "A".into())));
    assert!(!files.iter().any(|(_, f, _)| f == "sub"));
    assert_eq!(files.len(), 4);
}

#[test]
fn set_wanted_skips_tree_diffs() {
    let dir = fixture_repo();
    let mut source = open_source(dir.path());
    source.set_wanted(&["commit".to_string()].into_iter().collect());

    let mut sink = Collect::default();
    let mut worker = source.open();
    for unit in &source.seed_units() {
        worker.ingest(unit, &mut sink);
    }
    assert!(sink.rows.iter().any(|(r, _, _)| r == "commit"));
    assert!(
        !sink.rows.iter().any(|(r, _, _)| r == "commit_file"),
        "unwanted commit_file rows must not be built"
    );
}

#[test]
fn polling_streams_new_commits_append_only() {
    let dir = fixture_repo();
    let source = open_source(dir.path());
    let units = source.seed_units();
    let mut worker = source.open();
    let mut sink = Collect::default();
    for unit in &units {
        worker.ingest(unit, &mut sink);
    }
    assert_eq!(worker.poll_changes().len(), 0, "no news, no units");

    std::fs::write(dir.path().join("c.txt"), "c\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "add c"]);

    let fresh = worker.poll_changes();
    assert_eq!(fresh.len(), 1, "exactly the new commit");
    assert!(!units.contains(&fresh[0]));
    assert_eq!(worker.poll_changes().len(), 0, "seen once, not repeated");
}

#[test]
fn bad_root_is_rejected_at_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = PluginContext::new();
    dep2_plugin_git::GitPlugin.setup(&mut ctx);
    let provider = ctx.get_streaming_data_provider("git").unwrap();
    let mut config = HashMap::new();
    config.insert("root".to_string(), dir.path().display().to_string());
    assert!(provider.open_stream(&config).is_err(), "not a repo");
}
