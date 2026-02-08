//! End-to-end test for CSV streaming with file watching and retraction support.
//!
//! 1. Create a temp CSV file with initial rows
//! 2. Start the streaming engine (subprocess)
//! 3. Verify initial rows appear as inserts
//! 4. Modify the CSV (add/remove rows)
//! 5. Verify inserts and retractions appear in output
//! 6. Shutdown

use std::io::Write;
use std::time::Duration;

/// Subprocess test: runs dbflow as a binary and captures stdout.
/// Writes initial CSV, starts dbflow, modifies CSV, verifies output.
#[test]
fn e2e_csv_streaming_subprocess() {
    // 1. Create temp CSV file with initial data.
    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let csv_path = work_dir.path().join("test_data.csv");

    {
        let mut f = std::fs::File::create(&csv_path).expect("failed to create CSV");
        writeln!(f, "name").unwrap();
        writeln!(f, "Alice").unwrap();
        writeln!(f, "Bob").unwrap();
    }

    let facts_dir = work_dir.path().join("facts");
    let csvs_dir = work_dir.path().join("csvs");

    // 2. Write HCL file.
    let hcl_content = format!(
        r#"
        data "csv" "people" {{
            path = "{}"
        }}

        output "names" {{
            value = data.csv.people.name
        }}
    "#,
        csv_path.display()
    );

    let mut hcl_file = tempfile::Builder::new()
        .suffix(".hcl")
        .tempfile()
        .expect("failed to create temp HCL file");
    hcl_file
        .write_all(hcl_content.as_bytes())
        .expect("failed to write HCL");

    // 3. Start dbflow subprocess.
    #[allow(unused_mut)]
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_dbflow"))
        .arg(hcl_file.path())
        .arg("--facts")
        .arg(&facts_dir)
        .arg("--csvs")
        .arg(&csvs_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start dbflow");

    // 4. Wait for the streaming pipeline to process initial data.
    std::thread::sleep(Duration::from_secs(3));

    // 5. Modify the CSV: remove Bob, add Charlie.
    {
        let mut f = std::fs::File::create(&csv_path).expect("failed to rewrite CSV");
        writeln!(f, "name").unwrap();
        writeln!(f, "Alice").unwrap();
        writeln!(f, "Charlie").unwrap();
    }

    // 6. Wait for the file watcher to detect changes.
    std::thread::sleep(Duration::from_secs(3));

    // 7. Send SIGTERM for graceful shutdown.
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }

    let output = child
        .wait_with_output()
        .expect("failed to wait for dbflow");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("stdout:\n{}", stdout);
    eprintln!("stderr:\n{}", stderr);

    // 8. Verify output:
    //    - Initial inserts for Alice and Bob
    //    - After modification: retract Bob, insert Charlie
    assert!(
        stdout.contains("Alice"),
        "Expected Alice in output.\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("Bob"),
        "Expected Bob in output.\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("Charlie"),
        "Expected Charlie in output.\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr
    );
    // Verify retraction occurred for Bob
    assert!(
        stdout.contains("retract"),
        "Expected retraction in output.\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr
    );
}
