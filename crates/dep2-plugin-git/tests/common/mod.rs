//! Shared fixture helper for the git plugin's tests.

use std::path::Path;
use std::process::Command;

/// Variables through which git redirects an operation at a repository other
/// than the one `current_dir` names.
///
/// Git exports these to hooks. A pre-commit hook that runs the test suite
/// therefore hands every test a `GIT_DIR` pointing at the real repository and a
/// `GIT_INDEX_FILE` pointing at the index lock being built for the commit in
/// progress — and `git add .` inside a tempdir then writes the tempdir's files
/// into the developer's actual index. That is how a fixture path (`sub/c.txt`)
/// ends up staged in this repository, referring to a blob that ceases to exist
/// the moment the tempdir is dropped; the next commit fails with "invalid
/// object ... Error building trees" and points at nothing that explains itself.
///
/// The tempdir is the whole point of these fixtures, so the inherited
/// environment is cleared rather than worked around.
const REDIRECTS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_PREFIX",
];

/// Run `git` against a fixture repository, isolated from any ambient git
/// environment and from the developer's identity and config.
pub fn git(dir: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .args(args);
    for key in REDIRECTS {
        cmd.env_remove(key);
    }
    let out = cmd.output().expect("git runs");
    assert!(out.status.success(), "git {:?}: {:?}", args, out);
}
