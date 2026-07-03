//! Cross-parser equivalence: every `.dl` program in the repo's corpora must
//! parse to the same AST through the chumsky front-end as through the pest
//! parser (compared via `Program`'s `Display`, which covers decls, force-serve
//! grouping is not printed but rules and attributes are).

use parsing::parser::Program;

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
fn chumsky_matches_pest_on_the_whole_corpus() {
    let paths = corpus();
    assert!(paths.len() >= 20, "corpus went missing? found {:?}", paths);

    let mut checked = 0;
    for path in &paths {
        let src = std::fs::read_to_string(path).unwrap();
        // Some corpus programs may only be valid in-engine; compare only when
        // pest accepts them standalone (typing runs in both constructors).
        let pest = std::panic::catch_unwind(|| Program::parse_from(&path.to_string_lossy()));
        match pest {
            Ok(pest_program) => {
                let chumsky_program = syntax::parse(&src).unwrap_or_else(|d| {
                    panic!(
                        "chumsky rejected {} that pest accepts:\n{}",
                        path.display(),
                        syntax::render(&path.to_string_lossy(), &src, &d, false)
                    )
                });
                assert_eq!(
                    pest_program.to_string(),
                    chumsky_program.to_string(),
                    "AST mismatch on {}",
                    path.display()
                );
                // force-serve isn't part of Display; compare it explicitly.
                let pest_serve: Vec<(String, bool)> = pest_program
                    .idbs()
                    .iter()
                    .map(|d| (d.name().to_string(), d.force_serve()))
                    .collect();
                let chumsky_serve: Vec<(String, bool)> = chumsky_program
                    .idbs()
                    .iter()
                    .map(|d| (d.name().to_string(), d.force_serve()))
                    .collect();
                assert_eq!(
                    pest_serve,
                    chumsky_serve,
                    "force-serve mismatch on {}",
                    path.display()
                );
                checked += 1;
            }
            Err(_) => {
                // pest (or typing) rejects it — chumsky must reject it too.
                assert!(
                    syntax::parse(&src).is_err(),
                    "chumsky accepted {} that pest/typing rejects",
                    path.display()
                );
            }
        }
    }
    assert!(checked >= 20, "only {} programs compared", checked);
}
