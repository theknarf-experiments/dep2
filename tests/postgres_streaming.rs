//! End-to-end test for PostgreSQL LISTEN/NOTIFY streaming using testcontainers.
//!
//! Requires Docker to be running. The tests:
//! 1. Start a PostgreSQL container via testcontainers
//! 2. Create a dbflow engine with the PostgreSQL streaming plugin
//! 3. Load an HCL program that listens on a PostgreSQL channel
//! 4. Run streaming execution in a background thread
//! 5. Send NOTIFY via a separate postgres::Client
//! 6. Verify output appears
//! 7. Trigger shutdown

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use postgres::{Client, NoTls};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::SyncRunner;

use dbflow_core::engine::{DbFlow, DbFlowConfig};

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Library-level test: runs the streaming engine in-process.
/// Validates the full pipeline: NOTIFY → StreamingDataSource → channel → DD session → inspect → print.
#[test]
fn e2e_postgres_streaming() {
    if !docker_available() {
        eprintln!("Docker not available, skipping PostgreSQL streaming test");
        return;
    }

    // 1. Start PostgreSQL container.
    let pg_node = Postgres::default()
        .start()
        .expect("failed to start PostgreSQL container");

    let host = pg_node.get_host().expect("failed to get host");
    let host_port = pg_node
        .get_host_port_ipv4(5432)
        .expect("failed to get PostgreSQL host port");
    let connection_string = format!(
        "host={} port={} user=postgres password=postgres dbname=postgres",
        host, host_port
    );

    eprintln!("PostgreSQL connection: {}", connection_string);

    // Give PostgreSQL a moment to fully initialize.
    std::thread::sleep(Duration::from_secs(2));

    let channel = "dbflow_test_channel";

    // 2. Set up dbflow engine.
    let mut engine = DbFlow::with_config(DbFlowConfig {
        workers: 1,
        facts_dir: None,
        csvs_dir: None,
    });
    engine.add_plugin(Box::new(dbflow_plugin_postgres::PostgresPlugin));

    // 3. Load HCL program that listens on a PostgreSQL channel.
    let hcl = format!(
        r#"
        data "postgres" "events" {{
            connection = "{connection_string}"
            channel    = "{channel}"
        }}

        output "messages" {{
            value = data.postgres.events.value
        }}
    "#
    );

    engine
        .load_hcl(&hcl, None)
        .expect("failed to load HCL program");

    assert!(
        engine.has_streaming(),
        "Engine should detect streaming data blocks"
    );

    // 4. Run streaming in a background thread.
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    let streaming_handle = std::thread::spawn(move || {
        engine
            .execute_streaming(shutdown_clone)
            .expect("streaming execution failed");
    });

    // 5. Wait for the listener to be ready, then send notifications.
    //    NOTIFY is fire-and-forget (no replay), so the listener must be
    //    active before we send.
    std::thread::sleep(Duration::from_secs(3));

    let mut notifier = Client::connect(&connection_string, NoTls)
        .expect("failed to connect notifier client");

    let messages = vec!["hello-dbflow", "streaming-works", "third-message"];
    for msg in &messages {
        notifier
            .execute(
                &format!("NOTIFY {}, '{}'", channel, msg),
                &[],
            )
            .expect("failed to send NOTIFY");
    }

    eprintln!("Sent {} notifications on {}", messages.len(), channel);

    // 6. Wait for processing.
    std::thread::sleep(Duration::from_secs(3));

    // 7. Signal shutdown and wait.
    shutdown.store(true, Ordering::Relaxed);

    streaming_handle
        .join()
        .expect("streaming thread panicked");

    eprintln!("PostgreSQL streaming e2e test completed successfully");
}

/// Subprocess test: runs dbflow as a binary and captures stdout.
/// Starts the listener first, then sends NOTIFY, verifies messages in stdout.
#[test]
fn e2e_postgres_streaming_subprocess() {
    if !docker_available() {
        eprintln!("Docker not available, skipping PostgreSQL streaming subprocess test");
        return;
    }

    // 1. Start PostgreSQL container.
    let pg_node = Postgres::default()
        .start()
        .expect("failed to start PostgreSQL container");

    let host = pg_node.get_host().expect("failed to get host");
    let host_port = pg_node
        .get_host_port_ipv4(5432)
        .expect("failed to get PostgreSQL host port");
    let connection_string = format!(
        "host={} port={} user=postgres password=postgres dbname=postgres",
        host, host_port
    );

    std::thread::sleep(Duration::from_secs(2));

    let channel = "dbflow_subprocess_channel";

    // 2. Write HCL file.
    let hcl_content = format!(
        r#"
        data "postgres" "events" {{
            connection = "{connection_string}"
            channel    = "{channel}"
        }}

        output "messages" {{
            value = data.postgres.events.value
        }}
    "#
    );

    let mut hcl_file = tempfile::Builder::new()
        .suffix(".hcl")
        .tempfile()
        .expect("failed to create temp HCL file");
    hcl_file
        .write_all(hcl_content.as_bytes())
        .expect("failed to write HCL");

    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let facts_dir = work_dir.path().join("facts");
    let csvs_dir = work_dir.path().join("csvs");

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

    // 4. Wait for the streaming pipeline to start listening.
    //    NOTIFY has no replay, so we must wait for the listener to be active.
    std::thread::sleep(Duration::from_secs(5));

    // 5. Send NOTIFY from a separate client.
    let mut notifier = Client::connect(&connection_string, NoTls)
        .expect("failed to connect notifier client");

    let messages = vec!["alpha", "beta", "gamma"];
    for msg in &messages {
        notifier
            .execute(
                &format!("NOTIFY {}, '{}'", channel, msg),
                &[],
            )
            .expect("failed to send NOTIFY");
    }

    eprintln!("Sent {} notifications on {}", messages.len(), channel);

    // 6. Wait for processing.
    std::thread::sleep(Duration::from_secs(5));

    // 7. Send SIGTERM for graceful shutdown.
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }

    let output = child.wait_with_output().expect("failed to wait for dbflow");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("stdout:\n{}", stdout);
    eprintln!("stderr:\n{}", stderr);

    // 8. Verify output contains our messages.
    for msg in &messages {
        assert!(
            stdout.contains(msg),
            "Expected message '{}' in output.\nstdout:\n{}\nstderr:\n{}",
            msg,
            stdout,
            stderr
        );
    }
}
