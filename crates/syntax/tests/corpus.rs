//! Every `.dl` program in the repo's corpora must parse (they are also run
//! end-to-end by `executing`'s pipeline tests, which parse through this crate
//! — this test just gives a fast, direct failure when the parser regresses on
//! a real program).

fn corpus() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for dir in [
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../executing/tests/flowlog_programs"
        ),
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples"),
    ] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "dl") {
                    paths.push(path);
                }
            }
        }
    }
    paths.sort();
    paths
}

#[test]
fn the_whole_corpus_parses() {
    let paths = corpus();
    assert!(paths.len() >= 20, "corpus went missing? found {:?}", paths);

    for path in &paths {
        // parse_file, not parse: programs may use `.import` (resolved
        // relative to the file), and behaves identically for import-free
        // files.
        let program = syntax::parse_file(path, false)
            .unwrap_or_else(|report| panic!("failed to parse {}:\n{}", path.display(), report));
        assert!(
            !program.rules().is_empty(),
            "{}: parsed program has no rules",
            path.display()
        );
        assert!(
            !program.edbs().is_empty(),
            "{}: parsed program has no input relations",
            path.display()
        );
    }
}
