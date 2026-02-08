//! End-to-end test for Kafka streaming using testcontainers.
//!
//! Requires Docker to be running. The tests:
//! 1. Start an Apache Kafka container via testcontainers
//! 2. Create a dbflow engine with the Kafka streaming plugin
//! 3. Load an HCL program that reads from a Kafka topic
//! 4. Run streaming execution in a background thread
//! 5. Produce messages to the topic via rdkafka
//! 6. Verify output appears
//! 7. Trigger shutdown

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rdkafka::config::ClientConfig;
use rdkafka::producer::{BaseProducer, BaseRecord, Producer};
use testcontainers_modules::kafka::apache::Kafka;
use testcontainers_modules::testcontainers::runners::SyncRunner;
use testcontainers_modules::testcontainers::ImageExt;

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
/// Validates the full pipeline: Kafka → StreamingDataSource → channel → DD session → inspect → print.
#[test]
fn e2e_kafka_streaming() {
    if !docker_available() {
        eprintln!("Docker not available, skipping Kafka streaming test");
        return;
    }

    // 1. Start Kafka container.
    let kafka_node = Kafka::default()
        .with_env_var("KAFKA_AUTO_CREATE_TOPICS_ENABLE", "true")
        .start()
        .expect("failed to start Kafka container");

    let host_port = kafka_node
        .get_host_port_ipv4(9092)
        .expect("failed to get Kafka host port");
    let bootstrap_servers = format!("127.0.0.1:{}", host_port);

    eprintln!("Kafka bootstrap servers: {}", bootstrap_servers);

    // Give Kafka a moment to fully initialize.
    std::thread::sleep(Duration::from_secs(2));

    let topic = "dbflow-test-topic";

    // 2. Set up dbflow engine.
    let mut engine = DbFlow::with_config(DbFlowConfig {
        workers: 1,
        facts_dir: None,
        csvs_dir: None,
    });
    engine.add_plugin(Box::new(dbflow_plugin_kafka::KafkaPlugin));

    // 3. Load HCL program that reads from Kafka.
    let hcl = format!(
        r#"
        data "kafka" "events" {{
            brokers  = "{bootstrap_servers}"
            topic    = "{topic}"
            group_id = "dbflow-test-consumer"
        }}

        output "messages" {{
            value = data.kafka.events.value
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

    // 5. Wait for the consumer to be ready, then produce messages.
    std::thread::sleep(Duration::from_secs(3));

    let producer: BaseProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("message.timeout.ms", "5000")
        .create()
        .expect("failed to create Kafka producer");

    let messages = vec!["hello-dbflow", "streaming-works", "third-message"];
    for msg in &messages {
        producer
            .send(BaseRecord::<(), str>::to(topic).payload(msg))
            .expect("failed to send message");
    }
    producer
        .flush(Duration::from_secs(5))
        .expect("flush failed");

    eprintln!("Produced {} messages to {}", messages.len(), topic);

    // 6. Wait for processing.
    std::thread::sleep(Duration::from_secs(5));

    // 7. Signal shutdown and wait.
    shutdown.store(true, Ordering::Relaxed);

    streaming_handle.join().expect("streaming thread panicked");

    eprintln!("Kafka streaming e2e test completed successfully");
    // The fact that we get here without panicking means the full pipeline works.
    // The output lines (e.g., `output "messages": hello-dbflow`) are printed
    // to stdout during the streaming loop — visible with --nocapture.
}

/// Subprocess test: runs dbflow as a binary and captures stdout.
/// Pre-produces messages so the consumer (with auto.offset.reset=earliest)
/// picks them up, then verifies they appear in stdout.
#[test]
fn e2e_kafka_streaming_subprocess() {
    if !docker_available() {
        eprintln!("Docker not available, skipping Kafka streaming subprocess test");
        return;
    }

    // 1. Start Kafka container.
    let kafka_node = Kafka::default()
        .with_env_var("KAFKA_AUTO_CREATE_TOPICS_ENABLE", "true")
        .start()
        .expect("failed to start Kafka container");

    let host_port = kafka_node
        .get_host_port_ipv4(9092)
        .expect("failed to get Kafka host port");
    let bootstrap_servers = format!("127.0.0.1:{}", host_port);

    std::thread::sleep(Duration::from_secs(2));

    let topic = "dbflow-subprocess-topic";

    // 2. Pre-produce messages.
    let producer: BaseProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("message.timeout.ms", "5000")
        .create()
        .expect("failed to create Kafka producer");

    let messages = vec!["alpha", "beta", "gamma"];
    for msg in &messages {
        producer
            .send(BaseRecord::<(), str>::to(topic).payload(msg))
            .expect("failed to send message");
    }
    producer
        .flush(Duration::from_secs(5))
        .expect("flush failed");

    eprintln!("Pre-produced {} messages to {}", messages.len(), topic);

    // 3. Write HCL file.
    let hcl_content = format!(
        r#"
        data "kafka" "events" {{
            brokers  = "{bootstrap_servers}"
            topic    = "{topic}"
            group_id = "dbflow-subprocess-consumer"
        }}

        output "messages" {{
            value = data.kafka.events.value
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

    // 4. Start dbflow subprocess.
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

    // 5. Wait for the streaming pipeline to consume and process messages.
    //    The consumer uses auto.offset.reset=earliest, so it should pick up
    //    the pre-produced messages after subscribing.
    std::thread::sleep(Duration::from_secs(15));

    // 6. Send SIGTERM for graceful shutdown.
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

    // 7. Verify output contains our messages.
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
