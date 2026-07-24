//! `.import "file.dl"` — multi-file programs. Imports are resolved relative
//! to the importing file, merged import-once by canonical path (diamonds and
//! cycles are harmless), identical duplicate declarations collapse, and
//! conflicting ones are errors. Text-only parsing (`syntax::parse`) has no
//! file context, so imports there are rejected — which is also what keeps
//! runtime queries (POSTed source text) import-free.

use std::fs;

fn write(dir: &std::path::Path, name: &str, src: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&p, src).unwrap();
    p
}

const LIB: &str = "\
.in
.decl edge(x: number, y: number)

.printsize
.decl tc(x: number, y: number)

.rule
tc(X, Y) :- edge(X, Y).
tc(X, Z) :- tc(X, Y), edge(Y, Z).
";

#[test]
fn import_merges_decls_and_rules() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "lib/tc.dl", LIB);
    let main = write(
        dir.path(),
        "main.dl",
        ".import \"lib/tc.dl\"\n.printsize\n.decl two_step(x: number, y: number)\n.rule\ntwo_step(X, Z) :- edge(X, Y), edge(Y, Z).\n",
    );
    let program = syntax::parse_file(&main, false).expect("import must resolve");
    assert!(program.edbs().iter().any(|d| d.name() == "edge"));
    let idbs: Vec<&str> = program.idbs().iter().map(|d| d.name()).collect();
    assert!(idbs.contains(&"tc") && idbs.contains(&"two_step"));
    assert_eq!(program.rules().len(), 3);
}

#[test]
fn nested_diamond_and_cycle_imports_resolve_once() {
    let dir = tempfile::tempdir().unwrap();
    // base <- a, base <- b, main <- a + b  (diamond); c <-> d (cycle).
    write(dir.path(), "base.dl", ".in\n.decl e(x: number)\n");
    write(
        dir.path(),
        "a.dl",
        ".import \"base.dl\"\n.printsize\n.decl a(x: number)\n.rule\na(X) :- e(X).\n",
    );
    write(
        dir.path(),
        "b.dl",
        ".import \"base.dl\"\n.printsize\n.decl b(x: number)\n.rule\nb(X) :- e(X).\n",
    );
    let main = write(
        dir.path(),
        "main.dl",
        ".import \"a.dl\"\n.import \"b.dl\"\n",
    );
    let program = syntax::parse_file(&main, false).expect("diamond must resolve once");
    assert_eq!(
        program.edbs().iter().filter(|d| d.name() == "e").count(),
        1,
        "diamond import must not duplicate the shared decl"
    );

    write(
        dir.path(),
        "c.dl",
        ".import \"d.dl\"\n.in\n.decl c(x: number)\n",
    );
    write(
        dir.path(),
        "d.dl",
        ".import \"c.dl\"\n.in\n.decl d(x: number)\n",
    );
    let cyc = write(dir.path(), "cycmain.dl", ".import \"c.dl\"\n");
    let program = syntax::parse_file(&cyc, false).expect("cycles are harmless (import-once)");
    assert!(program.edbs().iter().any(|d| d.name() == "c"));
    assert!(program.edbs().iter().any(|d| d.name() == "d"));
}

#[test]
fn conflicting_and_missing_imports_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    // Conflicting decl: same name, different schema.
    write(dir.path(), "one.dl", ".in\n.decl e(x: number)\n");
    let main = write(
        dir.path(),
        "clash.dl",
        ".import \"one.dl\"\n.in\n.decl e(x: number, y: number)\n",
    );
    let err = syntax::parse_file(&main, false).expect_err("conflicting decls must fail");
    assert!(
        err.contains('e') && err.to_lowercase().contains("conflict"),
        "got: {err}"
    );

    // Missing file names both the path and the importer.
    let main = write(dir.path(), "missing.dl", ".import \"nope.dl\"\n");
    let err = syntax::parse_file(&main, false).expect_err("missing import must fail");
    assert!(err.contains("nope.dl"), "got: {err}");

    // Errors INSIDE an imported file point at that file.
    write(dir.path(), "broken.dl", ".in\n.decl bad(x: numbr)\n");
    let main = write(dir.path(), "usebroken.dl", ".import \"broken.dl\"\n");
    let err = syntax::parse_file(&main, false).expect_err("imported file's error must surface");
    assert!(
        err.contains("broken.dl"),
        "error must name the imported file, got: {err}"
    );
}

#[test]
fn identical_redeclaration_across_files_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "lib.dl", ".in\n.decl e(x: number)\n");
    // The importer redeclares e identically (a common pattern for readability).
    let main = write(
        dir.path(),
        "main.dl",
        ".import \"lib.dl\"\n.in\n.decl e(x: number)\n.printsize\n.decl a(x: number)\n.rule\na(X) :- e(X).\n",
    );
    let program = syntax::parse_file(&main, false).expect("identical redecl must merge");
    assert_eq!(program.edbs().iter().filter(|d| d.name() == "e").count(), 1);
}

#[test]
fn text_only_parse_rejects_imports() {
    let err = syntax::parse(".import \"x.dl\"\n.in\n.decl e(x: number)\n")
        .expect_err("imports need a file context");
    let text = format!("{:?}", err);
    assert!(text.to_lowercase().contains("file"), "got: {text}");
}
