//! Tell the linker where libduckdb lives.
//!
//! `libduckdb-sys` emits `-lduckdb`, but not a search path for it, so linking
//! the final binary fails with `ld: library 'duckdb' not found` even though the
//! plugin crate itself compiled — rlibs defer the link, so the error surfaces
//! only when something links an executable, which makes it look like a problem
//! with the binary rather than with this dependency.
//!
//! Homebrew installs into `/opt/homebrew/lib` on Apple silicon and
//! `/usr/local/lib` on Intel, neither of which the linker searches by default.
//! Rather than hardcode one, ask in the order that respects an explicit choice
//! first: `DUCKDB_LIB_DIR`, then `pkg-config`, then the usual prefixes.
//!
//! Nothing is emitted when the library cannot be found. The link error that
//! follows is clearer than anything invented here, and a wrong `-L` would only
//! obscure it.

use std::path::Path;
use std::process::Command;

fn candidates() -> Vec<String> {
    let mut out = Vec::new();

    if let Ok(dir) = std::env::var("DUCKDB_LIB_DIR") {
        out.push(dir);
    }

    // pkg-config knows the answer when duckdb was installed with a .pc file.
    if let Ok(o) = Command::new("pkg-config")
        .args(["--variable=libdir", "duckdb"])
        .output()
    {
        if o.status.success() {
            let dir = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !dir.is_empty() {
                out.push(dir);
            }
        }
    }

    // Homebrew, asked directly rather than assumed: the prefix differs between
    // Apple silicon and Intel, and a user may have relocated it entirely.
    if let Ok(o) = Command::new("brew").args(["--prefix", "duckdb"]).output() {
        if o.status.success() {
            let prefix = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !prefix.is_empty() {
                out.push(format!("{}/lib", prefix));
            }
        }
    }

    out.extend(
        ["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"]
            .iter()
            .map(|s| s.to_string()),
    );
    out
}

fn main() {
    println!("cargo:rerun-if-env-changed=DUCKDB_LIB_DIR");

    // The bundled build compiles DuckDB itself and needs no search path.
    if std::env::var("CARGO_FEATURE_BUNDLED").is_ok() {
        return;
    }

    for dir in candidates() {
        let found = ["libduckdb.dylib", "libduckdb.so", "duckdb.lib"]
            .iter()
            .any(|f| Path::new(&dir).join(f).exists());
        if found {
            println!("cargo:rustc-link-search=native={}", dir);
            return;
        }
    }
}
