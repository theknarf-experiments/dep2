//! Every `.dl` file in the repo, walked recursively, must still parse.
//!
//! `corpus.rs` covers the same ground but reads only the TOP LEVEL of
//! `examples/` and `flowlog_programs/`. This test walks the whole checkout, so
//! it is the only thing guarding the programs that live in subdirectories
//! (`examples/import_graph/`, `examples/lang/`, `examples/egraph/`, ...) and
//! one-offs like `data/quotes/record.dl`.
//!
//! A handful of those subdirectory files are module FRAGMENTS: syntactically
//! fine, but they name relations another file declares, so parsing them alone
//! stops at "unknown relation". Those are pinned in [`FRAGMENTS`] and asserted
//! to fail *that specific way* — which is what makes this a syntax guard for
//! them too. Anything else that fails, or a fragment that fails differently,
//! is a regression.
//!
//! It doubles as a *diffing* harness. With `DL_AST_DUMP` set it also writes a
//! byte-stable `{:#?}` dump of every parse, so a grammar change (e.g. giving
//! arithmetic real operator precedence) can be proven semantically invisible
//! to the corpus by dumping before and after and comparing:
//!
//! ```text
//! DL_AST_DUMP=/abs/path/out.txt cargo test -p syntax --test ast_snapshot -- --nocapture
//! ```
//!
//! Determinism rules the dump obeys: files are visited in sorted order, paths
//! are emitted repo-relative, the AST is `{:#?}`-formatted (the parsing AST
//! carries no spans and no hash-ordered collections, so `Debug` is stable),
//! and any absolute path leaking out of an error report or a panic message is
//! rewritten to a repo-relative one. Nothing timestamps. Without the env var
//! nothing is written — this test never touches the repo.

use std::path::{Path, PathBuf};

/// Files that cannot parse standalone because they are fragments of a larger
/// program: they reference relations a sibling file declares. Each MUST fail
/// with an "unknown relation" error and nothing else — that failure proves the
/// syntax is still good while the missing declaration is expected.
///
/// If one of these starts parsing cleanly, delete it from the list. If a new
/// file needs to be added here, make sure it is genuinely a fragment and not a
/// typo'd relation name.
const FRAGMENTS: &[&str] = &[
    "examples/import_graph/analysis.dl",
    "examples/import_graph/go.dl",
    "examples/import_graph/java.dl",
    "examples/import_graph/javascript.dl",
    "examples/import_graph/kotlin.dl",
    "examples/import_graph/linking.dl",
    "examples/import_graph/modules.dl",
    "examples/import_graph/python.dl",
    "examples/import_graph/rust.dl",
    "examples/lang/common.dl",
    "examples/lang/spans.dl",
];

fn repo_root() -> PathBuf {
    // crates/syntax -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// Every `.dl` file under `root`, sorted, skipping build/vendor trees.
fn dl_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
            Err(_) => return,
        };
        entries.sort();
        for path in entries {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                if matches!(name, "node_modules" | "target" | ".git") {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "dl") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

/// Rewrite any absolute path into the repo into a repo-relative one, so the
/// dump does not depend on where the checkout lives.
fn scrub(text: &str, root: &Path) -> String {
    let root = root.to_string_lossy();
    let with_slash = format!("{}/", root);
    text.replace(&with_slash, "").replace(root.as_ref(), ".")
}

enum Outcome {
    /// Parsed. `empty` means the program declared nothing at all — no rules,
    /// no inputs, no derived relations — which would mean the parser silently
    /// swallowed the file. A file with `.decl`s and no rules is fine and
    /// common: `examples/git/common.dl` and `examples/lang/ast.dl` are shared
    /// declaration headers that other programs import.
    Parsed {
        empty: bool,
    },
    Error(String),
    Panic(String),
}

#[test]
fn every_corpus_program_still_parses() {
    let root = repo_root();
    let files = dl_files(&root);
    assert!(
        files.len() >= 20,
        "corpus went missing? found {} .dl files under {}",
        files.len(),
        root.display()
    );

    // A failure must not abort the walk: every file is reported together, and
    // the fragments below are *expected* to fail. Panics are caught for the
    // same reason, and the hook is silenced so a caught panic does not spray
    // the test output — the payload is surfaced in the assertion instead.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let mut results: Vec<(String, Outcome)> = Vec::new();
    for path in &files {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        let path_owned = path.clone();
        // parse_file, not parse: programs may use `.import` (resolved relative
        // to the file), and it behaves identically for import-free files.
        let outcome = match std::panic::catch_unwind(move || syntax::parse_file(&path_owned, false))
        {
            Ok(Ok(program)) => Outcome::Parsed {
                empty: program.rules().is_empty()
                    && program.edbs().is_empty()
                    && program.idbs().is_empty(),
            },
            Ok(Err(report)) => Outcome::Error(scrub(&report, &root)),
            Err(payload) => Outcome::Panic(scrub(
                &payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "<non-string panic payload>".to_string()),
                &root,
            )),
        };
        results.push((rel, outcome));
    }

    std::panic::set_hook(prev_hook);

    if let Ok(out_path) = std::env::var("DL_AST_DUMP") {
        write_dump(&out_path, &root, &files);
    }

    let mut problems: Vec<String> = Vec::new();
    let mut fixed_fragments: Vec<String> = Vec::new();

    for (rel, outcome) in &results {
        let is_fragment = FRAGMENTS.contains(&rel.as_str());
        match outcome {
            Outcome::Parsed { empty } => {
                if is_fragment {
                    fixed_fragments.push(rel.clone());
                } else if *empty {
                    problems.push(format!(
                        "{}: parsed into an empty program — no rules, no inputs, no derived \
                         relations. The parser swallowed the file.",
                        rel
                    ));
                }
            }
            Outcome::Error(report) => {
                if !is_fragment {
                    problems.push(format!("{}: failed to parse\n{}", rel, report));
                } else if !report.contains("unknown relation") {
                    problems.push(format!(
                        "{}: pinned as a module fragment, so the only expected failure is \
                         `unknown relation`, but it failed differently — this is a real \
                         parser regression:\n{}",
                        rel, report
                    ));
                }
            }
            Outcome::Panic(msg) => {
                problems.push(format!("{}: PANICKED while parsing: {}", rel, msg));
            }
        }
    }

    for rel in FRAGMENTS {
        if !results.iter().any(|(r, _)| r == rel) {
            problems.push(format!(
                "{}: pinned in FRAGMENTS but no such file — remove the stale entry",
                rel
            ));
        }
    }
    if !fixed_fragments.is_empty() {
        problems.push(format!(
            "these are pinned in FRAGMENTS but now parse cleanly; remove them from the list: {}",
            fixed_fragments.join(", ")
        ));
    }

    assert!(
        problems.is_empty(),
        "{} of {} corpus program(s) are not in the expected state:\n\n{}",
        problems.len(),
        files.len(),
        problems.join("\n\n")
    );
}

/// Opt-in `{:#?}` dump of every parse, for before/after grammar diffing.
fn write_dump(out_path: &str, root: &Path, files: &[PathBuf]) {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let mut dump = String::new();
    let mut ok = 0usize;
    let mut failed: Vec<String> = Vec::new();

    for path in files {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        let path_owned = path.clone();
        let body = match std::panic::catch_unwind(move || syntax::parse_file(&path_owned, false)) {
            Ok(Ok(program)) => {
                ok += 1;
                format!("OK\n{:#?}\n", program)
            }
            Ok(Err(report)) => {
                failed.push(rel.clone());
                format!("PARSE-ERROR\n{}\n", report)
            }
            Err(payload) => {
                failed.push(rel.clone());
                let msg = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "<non-string panic payload>".to_string());
                format!("PANIC\n{}\n", msg)
            }
        };

        dump.push_str("===== FILE ");
        dump.push_str(&rel);
        dump.push_str(" =====\n");
        dump.push_str(&scrub(&body, root));
    }

    std::panic::set_hook(prev_hook);

    dump.push_str(&format!(
        "===== SUMMARY =====\nfiles: {}\nparsed: {}\nfailed: {}\n",
        files.len(),
        ok,
        failed.len()
    ));
    for f in &failed {
        dump.push_str(&format!("failed-file: {}\n", f));
    }

    if let Some(parent) = Path::new(out_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(out_path, &dump).unwrap_or_else(|e| panic!("write {}: {}", out_path, e));
    eprintln!(
        "ast_snapshot: {} files, {} parsed, {} failed -> {}",
        files.len(),
        ok,
        failed.len(),
        out_path
    );
}
