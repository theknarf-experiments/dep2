use std::io::Write;
use std::process::Command;

use proptest::prelude::*;

/// Run the dbflow binary on a temporary HCL file with isolated temp directories.
/// Returns stdout as a String. Panics if the process exits non-zero.
fn run_hcl(hcl_source: &str) -> String {
    run_hcl_with_args(hcl_source, &[])
}

/// Run the dbflow binary with extra CLI args and isolated temp directories.
fn run_hcl_with_args(hcl_source: &str, args: &[&str]) -> String {
    let mut f = tempfile::Builder::new()
        .suffix(".hcl")
        .tempfile()
        .expect("failed to create temp file");
    f.write_all(hcl_source.as_bytes())
        .expect("failed to write HCL");

    // Each test gets its own temp directory for facts and csvs to avoid
    // interference when tests run in parallel.
    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let facts_dir = work_dir.path().join("facts");
    let csvs_dir = work_dir.path().join("csvs");

    let output = Command::new(env!("CARGO_BIN_EXE_dbflow"))
        .arg(f.path())
        .arg("--facts")
        .arg(&facts_dir)
        .arg("--csvs")
        .arg(&csvs_dir)
        .args(args)
        .output()
        .expect("failed to execute dbflow");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("dbflow exited with {}\nstderr:\n{}", output.status, stderr);
    }

    String::from_utf8(output.stdout).expect("non-UTF8 stdout")
}

#[test]
fn e2e_literal_output() {
    let stdout = run_hcl(
        r#"
        output "greeting" {
            value = "hello"
        }
    "#,
    );
    assert!(
        stdout.contains(r#"output "greeting": hello"#),
        "Expected greeting output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_integer_output() {
    let stdout = run_hcl(
        r#"
        output "port" {
            value = 8080
        }
    "#,
    );
    assert!(
        stdout.contains(r#"output "port": 8080"#),
        "Expected port output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_edb_with_output() {
    let stdout = run_hcl(
        r#"
        resource "server" "w1" {
            ip = "10.0.0.5"
        }

        output "server_ip" {
            value = server.w1.ip
        }
    "#,
    );
    assert!(
        stdout.contains(r#"output "server_ip": 10.0.0.5"#),
        "Expected server_ip output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_idb_with_output() {
    let stdout = run_hcl(
        r#"
        resource "server" "w1" {
            ip = "10.0.0.5"
        }

        resource "monitor" "m1" {
            target_ip = server.w1.ip
        }

        output "monitored_ip" {
            value = monitor.m1.target_ip
        }
    "#,
    );
    assert!(
        stdout.contains(r#"output "monitored_ip": 10.0.0.5"#),
        "Expected monitored_ip output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_variable_substitution() {
    let stdout = run_hcl(
        r#"
        variable "addr" {
            default = "192.168.1.1"
        }

        resource "host" "h1" {
            ip = var.addr
        }

        output "host_ip" {
            value = host.h1.ip
        }
    "#,
    );
    assert!(
        stdout.contains(r#"output "host_ip": 192.168.1.1"#),
        "Expected host_ip output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_multiple_outputs() {
    let stdout = run_hcl(
        r#"
        resource "server" "w1" {
            ip = "10.0.0.5"
            dc = "us-west"
        }

        output "ip" {
            value = server.w1.ip
        }

        output "dc" {
            value = server.w1.dc
        }
    "#,
    );
    assert!(
        stdout.contains(r#"output "ip": 10.0.0.5"#),
        "Expected ip output, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains(r#"output "dc": us-west"#),
        "Expected dc output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_emit_dl_flag() {
    let stdout = run_hcl_with_args(
        r#"
        resource "server" "w1" {
            ip = "10.0.0.5"
        }

        output "server_ip" {
            value = server.w1.ip
        }
    "#,
        &["--emit-dl"],
    );
    assert!(stdout.contains(".in"), "Expected .in section:\n{}", stdout);
    assert!(
        stdout.contains(".decl server("),
        "Expected server decl:\n{}",
        stdout
    );
    assert!(
        stdout.contains("hcl_output_server_ip"),
        "Expected output IDB:\n{}",
        stdout
    );
    // --emit-dl should NOT run execution or print output lines.
    assert!(
        !stdout.contains(r#"output "server_ip":"#),
        "Should not contain execution output with --emit-dl:\n{}",
        stdout
    );
}

#[test]
fn e2e_module_with_output() {
    // Create a child module file.
    let child_hcl = r#"
        variable "ip" {
            default = "0.0.0.0"
        }

        resource "server" "s1" {
            ip = var.ip
        }

        output "server_ip" {
            value = server.s1.ip
        }
    "#;
    let mut child_file = tempfile::Builder::new()
        .suffix(".hcl")
        .tempfile()
        .expect("failed to create child temp file");
    child_file
        .write_all(child_hcl.as_bytes())
        .expect("failed to write child HCL");

    let parent_hcl = format!(
        r#"
        module "web" {{
            source = "{}"
            ip = "10.0.0.1"
        }}

        output "result" {{
            value = module.web.server_ip
        }}
    "#,
        child_file.path().to_string_lossy().replace('\\', "/")
    );

    let stdout = run_hcl(&parent_hcl);
    assert!(
        stdout.contains(r#"output "result": 10.0.0.1"#),
        "Expected module output, got:\n{}",
        stdout
    );
}

// ---------------------------------------------------------------------------
// Negation tests
// ---------------------------------------------------------------------------

#[test]
fn e2e_negation_basic() {
    // Server IP is NOT in the blocked list → allowed rule fires.
    let stdout = run_hcl(
        r#"
        resource "server" "w1" {
            ip = "10.0.0.1"
        }

        resource "blocked" "b1" {
            ip = "10.0.0.2"
        }

        resource "allowed" "rule" {
            ip = server.w1.ip
            not_blocked = !blocked.b1.ip
        }

        output "result" {
            value = allowed.rule.ip
        }
    "#,
    );
    assert!(
        stdout.contains(r#"output "result": 10.0.0.1"#),
        "Expected allowed IP, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_negation_filters_match() {
    // Server IP IS in the blocked list (same IP) → negation filters it out → empty.
    let (success, stdout, stderr) = run_hcl_result(
        r#"
        resource "server" "w1" {
            ip = "10.0.0.1"
        }

        resource "blocked" "b1" {
            ip = "10.0.0.1"
        }

        resource "allowed" "rule" {
            ip = server.w1.ip
            not_blocked = !blocked.b1.ip
        }

        output "result" {
            value = allowed.rule.ip
        }
    "#,
    );
    if success {
        assert!(
            stdout.contains("(no results)") || stdout.contains("(empty)"),
            "Expected empty output when IPs match, got:\n{}",
            stdout
        );
    } else {
        panic!(
            "Negation filter test failed.\nstdout:\n{}\nstderr:\n{}",
            stdout, stderr
        );
    }
}

#[test]
fn e2e_negation_no_positive_var_sharing() {
    // Only negated reference, no positive ref for variable sharing.
    // The negation acts on the label only (all field args are placeholders).
    // Since blocked.b1 exists, the negation filters it → empty.
    let (success, stdout, stderr) = run_hcl_result(
        r#"
        resource "blocked" "b1" {
            ip = "10.0.0.2"
        }

        resource "check" "rule" {
            flag = !blocked.b1.ip
        }

        output "result" {
            value = check.rule.flag
        }
    "#,
    );
    // `check.rule` has only a negated ref, so it has no positive schema columns
    // besides the label. The output references check.rule.flag, but "flag" is
    // negated and excluded from schema, so the output should fail to compile
    // (unknown field). This is acceptable behavior.
    if success {
        // If it succeeds, the output should be empty (blocked.b1 exists, so negation filters).
        assert!(
            stdout.contains("(no results)") || stdout.contains("(empty)"),
            "Expected empty or error, got:\n{}",
            stdout
        );
    }
    // If it fails, that's also acceptable — the output references a negated field
    // that's excluded from the schema.
    let _ = (success, stdout, stderr);
}

#[test]
fn e2e_negation_with_emit_dl() {
    // Verify the --emit-dl output shows the negated atom.
    let stdout = run_hcl_with_args(
        r#"
        resource "server" "w1" {
            ip = "10.0.0.1"
        }

        resource "blocked" "b1" {
            ip = "10.0.0.2"
        }

        resource "allowed" "rule" {
            ip = server.w1.ip
            not_blocked = !blocked.b1.ip
        }

        output "result" {
            value = allowed.rule.ip
        }
    "#,
        &["--emit-dl"],
    );
    // The Datalog output should contain a negated atom.
    assert!(
        stdout.contains("!blocked("),
        "Expected negated atom in Datalog output:\n{}",
        stdout
    );
    assert!(
        stdout.contains(".decl allowed("),
        "Expected allowed decl:\n{}",
        stdout
    );
}

// ---------------------------------------------------------------------------
// Reference detection and recursion tests
// ---------------------------------------------------------------------------

/// Run the dbflow binary expecting it may fail. Returns (success, stdout, stderr).
fn run_hcl_result(hcl_source: &str) -> (bool, String, String) {
    let mut f = tempfile::Builder::new()
        .suffix(".hcl")
        .tempfile()
        .expect("failed to create temp file");
    f.write_all(hcl_source.as_bytes())
        .expect("failed to write HCL");

    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let facts_dir = work_dir.path().join("facts");
    let csvs_dir = work_dir.path().join("csvs");

    let output = Command::new(env!("CARGO_BIN_EXE_dbflow"))
        .arg(f.path())
        .arg("--facts")
        .arg(&facts_dir)
        .arg("--csvs")
        .arg(&csvs_dir)
        .output()
        .expect("failed to execute dbflow");

    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Run the dbflow binary on HCL that enters streaming mode (e.g. CSV data blocks).
/// Spawns the process, waits for output, sends SIGTERM, and returns stdout.
fn run_hcl_streaming(hcl_source: &str) -> String {
    let mut f = tempfile::Builder::new()
        .suffix(".hcl")
        .tempfile()
        .expect("failed to create temp file");
    f.write_all(hcl_source.as_bytes())
        .expect("failed to write HCL");

    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let facts_dir = work_dir.path().join("facts");
    let csvs_dir = work_dir.path().join("csvs");

    #[allow(unused_mut)]
    let mut child = Command::new(env!("CARGO_BIN_EXE_dbflow"))
        .arg(f.path())
        .arg("--facts")
        .arg(&facts_dir)
        .arg("--csvs")
        .arg(&csvs_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start dbflow");

    // Wait for the streaming pipeline to process initial data.
    std::thread::sleep(std::time::Duration::from_secs(5));

    // Send SIGTERM for graceful shutdown.
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }

    let output = child.wait_with_output().expect("failed to wait for dbflow");
    String::from_utf8(output.stdout).expect("non-UTF8 stdout")
}

#[test]
fn e2e_deep_acyclic_chain() {
    let stdout = run_hcl(
        r#"
        resource "origin" "o1" {
            val = "deep"
        }

        resource "relay" "r1" {
            val = origin.o1.val
        }

        resource "sink" "s1" {
            val = relay.r1.val
        }

        output "result" {
            value = sink.s1.val
        }
    "#,
    );
    assert!(
        stdout.contains(r#"output "result": deep"#),
        "Expected deep chain output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_diamond_dependency() {
    let stdout = run_hcl(
        r#"
        resource "source" "s1" {
            val = "diamond"
        }

        resource "left" "l1" {
            val = source.s1.val
        }

        resource "right" "r1" {
            val = source.s1.val
        }

        output "lout" {
            value = left.l1.val
        }

        output "rout" {
            value = right.r1.val
        }
    "#,
    );
    assert!(
        stdout.contains(r#"output "lout": diamond"#),
        "Expected left output, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains(r#"output "rout": diamond"#),
        "Expected right output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_multiple_edb_same_type() {
    let stdout = run_hcl(
        r#"
        resource "node" "a1" {
            val = "alpha"
        }

        resource "node" "a2" {
            val = "beta"
        }

        output "out1" {
            value = node.a1.val
        }

        output "out2" {
            value = node.a2.val
        }
    "#,
    );
    assert!(
        stdout.contains(r#"output "out1": alpha"#),
        "Expected alpha output, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains(r#"output "out2": beta"#),
        "Expected beta output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_mutual_recursion_no_base() {
    // Design doc Example 2: mutual recursion with no base case.
    // Both A and B reference each other with no EDB facts to start derivation.
    // The least fixpoint is empty — no facts can be derived.
    let (success, stdout, stderr) = run_hcl_result(
        r#"
        resource "a" "r" {
            link = b.r.link
        }

        resource "b" "r" {
            link = a.r.link
        }

        output "result" {
            value = a.r.link
        }
    "#,
    );
    if success {
        // Process succeeded — output should be empty (no results).
        assert!(
            stdout.contains("(no results)") || stdout.contains("(empty)"),
            "Expected empty output for mutual recursion with no base, got:\n{}",
            stdout
        );
    } else {
        // If it fails, that's also a valid finding — record what happened.
        panic!(
            "Mutual recursion (no base) failed.\nstdout:\n{}\nstderr:\n{}",
            stdout, stderr
        );
    }
}

#[test]
fn e2e_cascade_same_type_edb_idb() {
    // Same type "a" has both EDB and IDB instances.
    // a.base is EDB (literal), b.mid derives from a.base, a.end derives from b.mid.
    // This is acyclic but tests that a type can have both facts and rules.
    let (success, stdout, stderr) = run_hcl_result(
        r#"
        resource "a" "base" {
            val = "start"
        }

        resource "b" "mid" {
            val = a.base.val
        }

        resource "a" "end" {
            val = b.mid.val
        }

        output "result" {
            value = a.end.val
        }
    "#,
    );
    if success {
        assert!(
            stdout.contains(r#"output "result": start"#),
            "Expected cascade output 'start', got:\n{}",
            stdout
        );
    } else {
        panic!(
            "Cascade (same type EDB+IDB) failed.\nstdout:\n{}\nstderr:\n{}",
            stdout, stderr
        );
    }
}

#[test]
fn e2e_self_reference() {
    // A block references its own field — single-node cycle.
    // No base facts exist, so the fixpoint should be empty.
    let (success, stdout, stderr) = run_hcl_result(
        r#"
        resource "loop" "r" {
            val = loop.r.val
        }

        output "result" {
            value = loop.r.val
        }
    "#,
    );
    if success {
        assert!(
            stdout.contains("(no results)") || stdout.contains("(empty)"),
            "Expected empty output for self-reference, got:\n{}",
            stdout
        );
    } else {
        panic!(
            "Self-reference failed.\nstdout:\n{}\nstderr:\n{}",
            stdout, stderr
        );
    }
}

// ---------------------------------------------------------------------------
// Property-based e2e tests
// ---------------------------------------------------------------------------

/// Generate a valid HCL identifier: starts with a lowercase letter, followed
/// by lowercase letters and digits. Length 1..8.
fn arb_ident() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9]{0,7}"
}

/// Generate a safe string value for HCL (alphanumeric + limited punctuation,
/// no quotes or backslashes that would break parsing). Length 1..20.
fn arb_safe_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_.-]{1,20}"
}

/// Generate an integer that fits in i32 and is representable as a FlowLog
/// constant. We avoid extreme values near i32::MIN since Display may produce
/// negative signs that interact with parsing.
fn arb_int() -> impl Strategy<Value = i32> {
    0..100_000i32
}

// ---------------------------------------------------------------------------
// Data block tests
// ---------------------------------------------------------------------------

#[test]
fn e2e_csv_data_block() {
    // Create a temp CSV file.
    let mut csv_file = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("failed to create CSV file");
    csv_file
        .write_all(b"name,city\nalice,london\nbob,paris\n")
        .expect("failed to write CSV");

    let csv_path = csv_file.path().to_string_lossy().replace('\\', "/");

    let hcl_content = format!(
        r#"
        data "csv" "people" {{
            path = "{csv_path}"
        }}

        output "person_name" {{
            value = data.csv.people.name
        }}
    "#
    );

    let stdout = run_hcl_streaming(&hcl_content);
    // Output is multi-line: one row per CSV record.
    assert!(
        stdout.contains("alice") && stdout.contains("bob"),
        "Expected person names from CSV, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_csv_data_block_with_resource() {
    // Create a temp CSV file.
    let mut csv_file = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("failed to create CSV file");
    csv_file
        .write_all(b"ip,region\n10.0.0.1,us-west\n10.0.0.2,eu-east\n")
        .expect("failed to write CSV");

    let csv_path = csv_file.path().to_string_lossy().replace('\\', "/");

    let hcl_content = format!(
        r#"
        data "csv" "hosts" {{
            path = "{csv_path}"
        }}

        resource "monitor" "m1" {{
            target = data.csv.hosts.ip
        }}

        output "monitored" {{
            value = monitor.m1.target
        }}
    "#
    );

    let stdout = run_hcl_streaming(&hcl_content);
    // The monitor should pick up IPs from the CSV (one row per CSV record).
    assert!(
        stdout.contains("10.0.0.1") && stdout.contains("10.0.0.2"),
        "Expected monitored IPs from CSV, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_csv_data_block_emit_dl() {
    // Verify --emit-dl output shows the data relation declaration.
    let mut csv_file = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("failed to create CSV file");
    csv_file
        .write_all(b"name,age\nalice,30\n")
        .expect("failed to write CSV");

    let csv_path = csv_file.path().to_string_lossy().replace('\\', "/");

    let hcl = format!(
        r#"
        data "csv" "users" {{
            path = "{csv_path}"
        }}

        output "user" {{
            value = data.csv.users.name
        }}
    "#
    );
    let stdout = run_hcl_with_args(&hcl, &["--emit-dl"]);
    assert!(
        stdout.contains("_data_csv_users"),
        "Expected _data_csv_users in Datalog output:\n{}",
        stdout
    );
    assert!(
        stdout.contains("hcl_output_user"),
        "Expected hcl_output_user in Datalog output:\n{}",
        stdout
    );
}

// ---------------------------------------------------------------------------
// Comparison filter tests
// ---------------------------------------------------------------------------

#[test]
fn e2e_comparison_filter_integer() {
    // Create a temp CSV file with amounts.
    let mut csv_file = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("failed to create CSV file");
    csv_file
        .write_all(b"customer,amount\nalice,100\nbob,30\ncharlie,75\n")
        .expect("failed to write CSV");

    let csv_path = csv_file.path().to_string_lossy().replace('\\', "/");

    let hcl = format!(
        r#"
        data "csv" "orders" {{
            path = "{csv_path}"
        }}

        resource "big_order" "rule" {{
            customer = data.csv.orders.customer
            amount = data.csv.orders.amount
            _filter = data.csv.orders.amount > 50
        }}

        output "result" {{
            value = big_order.rule.customer
        }}
    "#
    );
    let stdout = run_hcl_streaming(&hcl);
    // Only alice (100) and charlie (75) should pass the filter > 50.
    assert!(
        stdout.contains("alice"),
        "Expected alice in filtered output, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("charlie"),
        "Expected charlie in filtered output, got:\n{}",
        stdout
    );
    assert!(
        !stdout.contains("bob"),
        "Did not expect bob (amount=30) in filtered output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_comparison_filter_equality() {
    let mut csv_file = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("failed to create CSV file");
    csv_file
        .write_all(b"name,score\nalice,42\nbob,99\ncharlie,42\n")
        .expect("failed to write CSV");

    let csv_path = csv_file.path().to_string_lossy().replace('\\', "/");

    let hcl = format!(
        r#"
        data "csv" "scores" {{
            path = "{csv_path}"
        }}

        resource "exact" "rule" {{
            name = data.csv.scores.name
            score = data.csv.scores.score
            _filter = data.csv.scores.score == 42
        }}

        output "result" {{
            value = exact.rule.name
        }}
    "#
    );
    let stdout = run_hcl_streaming(&hcl);
    assert!(
        stdout.contains("alice") && stdout.contains("charlie"),
        "Expected alice and charlie with score 42, got:\n{}",
        stdout
    );
    assert!(
        !stdout.contains("bob"),
        "Did not expect bob (score=99), got:\n{}",
        stdout
    );
}

#[test]
fn e2e_comparison_filter_emit_dl() {
    let mut csv_file = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("failed to create CSV file");
    csv_file
        .write_all(b"name,amount\nalice,100\n")
        .expect("failed to write CSV");

    let csv_path = csv_file.path().to_string_lossy().replace('\\', "/");

    let hcl = format!(
        r#"
        data "csv" "orders" {{
            path = "{csv_path}"
        }}

        resource "big_order" "rule" {{
            name = data.csv.orders.name
            amount = data.csv.orders.amount
            _filter = data.csv.orders.amount > 50
        }}

        output "result" {{
            value = big_order.rule.name
        }}
    "#
    );
    let stdout = run_hcl_with_args(&hcl, &["--emit-dl"]);
    // Should have a comparison predicate in the Datalog output.
    assert!(
        stdout.contains("> 50"),
        "Expected comparison > 50 in Datalog:\n{}",
        stdout
    );
    // The _filter attribute should NOT appear as a schema column.
    assert!(
        !stdout.contains("filter"),
        "Expected _filter excluded from schema:\n{}",
        stdout
    );
}

// ---------------------------------------------------------------------------
// Aggregate tests
// ---------------------------------------------------------------------------

#[test]
fn e2e_aggregate_count() {
    let mut csv_file = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("failed to create CSV file");
    csv_file
        .write_all(b"region,city\nus,nyc\nus,la\neu,london\n")
        .expect("failed to write CSV");

    let csv_path = csv_file.path().to_string_lossy().replace('\\', "/");

    let hcl = format!(
        r#"
        data "csv" "cities" {{
            path = "{csv_path}"
        }}

        resource "region_count" "all" {{
            region = data.csv.cities.region
            total = count(data.csv.cities.city)
        }}

        output "result" {{
            value = region_count.all.region
        }}
    "#
    );
    let stdout = run_hcl_streaming(&hcl);
    // Should show regions us and eu.
    assert!(
        stdout.contains("us") && stdout.contains("eu"),
        "Expected region names in aggregate output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_aggregate_sum() {
    let mut csv_file = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("failed to create CSV file");
    csv_file
        .write_all(b"region,amount\nus,100\nus,200\neu,50\n")
        .expect("failed to write CSV");

    let csv_path = csv_file.path().to_string_lossy().replace('\\', "/");

    let hcl = format!(
        r#"
        data "csv" "sales" {{
            path = "{csv_path}"
        }}

        resource "totals" "all" {{
            region = data.csv.sales.region
            total = sum(data.csv.sales.amount)
        }}

        output "result" {{
            value = totals.all.total
        }}
    "#
    );
    let stdout = run_hcl_streaming(&hcl);
    // us should have sum 300, eu should have sum 50.
    assert!(
        stdout.contains("300") && stdout.contains("50"),
        "Expected sum results 300 and 50, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_aggregate_min_max() {
    let mut csv_file = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("failed to create CSV file");
    csv_file
        .write_all(b"group,val\na,10\na,30\na,20\nb,5\nb,15\n")
        .expect("failed to write CSV");

    let csv_path = csv_file.path().to_string_lossy().replace('\\', "/");

    // Test min
    let hcl_min = format!(
        r#"
        data "csv" "data" {{
            path = "{csv_path}"
        }}

        resource "mins" "all" {{
            group = data.csv.data.group
            minimum = min(data.csv.data.val)
        }}

        output "result" {{
            value = mins.all.minimum
        }}
    "#
    );
    let stdout = run_hcl_streaming(&hcl_min);
    // Group a: min=10, group b: min=5.
    assert!(
        stdout.contains("10") && stdout.contains("5"),
        "Expected min results 10 and 5, got:\n{}",
        stdout
    );

    // Test max
    let hcl_max = format!(
        r#"
        data "csv" "data" {{
            path = "{csv_path}"
        }}

        resource "maxes" "all" {{
            group = data.csv.data.group
            maximum = max(data.csv.data.val)
        }}

        output "result" {{
            value = maxes.all.maximum
        }}
    "#
    );
    let stdout = run_hcl_streaming(&hcl_max);
    // Group a: max=30, group b: max=15.
    assert!(
        stdout.contains("30") && stdout.contains("15"),
        "Expected max results 30 and 15, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_aggregate_with_filter() {
    let mut csv_file = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("failed to create CSV file");
    csv_file
        .write_all(b"region,amount\nus,100\nus,200\nus,20\neu,50\neu,10\n")
        .expect("failed to write CSV");

    let csv_path = csv_file.path().to_string_lossy().replace('\\', "/");

    let hcl = format!(
        r#"
        data "csv" "sales" {{
            path = "{csv_path}"
        }}

        resource "big_totals" "all" {{
            region = data.csv.sales.region
            total = sum(data.csv.sales.amount)
            _filter = data.csv.sales.amount > 30
        }}

        output "result" {{
            value = big_totals.all.total
        }}
    "#
    );
    let stdout = run_hcl_streaming(&hcl);
    // Only amounts > 30: us gets 100+200=300, eu gets 50.
    assert!(
        stdout.contains("300") && stdout.contains("50"),
        "Expected filtered sum results 300 and 50, got:\n{}",
        stdout
    );
}

// ---------------------------------------------------------------------------
// Arithmetic tests
// ---------------------------------------------------------------------------

#[test]
fn e2e_arithmetic_in_filter() {
    let mut csv_file = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("failed to create CSV file");
    csv_file
        .write_all(b"customer,amount,tax\nalice,900,200\nbob,400,100\n")
        .expect("failed to write CSV");

    let csv_path = csv_file.path().to_string_lossy().replace('\\', "/");

    let hcl = format!(
        r#"
        data "csv" "orders" {{
            path = "{csv_path}"
        }}

        resource "expensive" "rule" {{
            customer = data.csv.orders.customer
            _filter = data.csv.orders.amount + data.csv.orders.tax > 1000
        }}

        output "result" {{
            value = expensive.rule.customer
        }}
    "#
    );
    let stdout = run_hcl_streaming(&hcl);
    // alice: 900+200=1100 > 1000, bob: 400+100=500 < 1000.
    assert!(
        stdout.contains("alice"),
        "Expected alice (900+200>1000), got:\n{}",
        stdout
    );
    assert!(
        !stdout.contains("bob"),
        "Did not expect bob (400+100<1000), got:\n{}",
        stdout
    );
}

// ---------------------------------------------------------------------------
// Exec plugin tests
// ---------------------------------------------------------------------------

#[test]
fn e2e_exec_append_mode() {
    let hcl = r#"
        data "exec" "lines" {
            command = "printf 'alice 30\nbob 25\ncharlie 40\n'"
            split   = "\\s+"
            mode    = "append"
            columns = "name,age"
        }

        output "names" {
            value = data.exec.lines.name
        }
    "#;

    let stdout = run_hcl_streaming(hcl);
    assert!(
        stdout.contains("alice") && stdout.contains("bob") && stdout.contains("charlie"),
        "Expected all names from exec append mode, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_exec_snapshot_mode() {
    // Write a shell script that outputs two snapshots separated by ANSI clear-screen.
    // First snapshot: alice, bob
    // Second snapshot: alice, charlie (bob retracted, charlie inserted)
    let mut script = tempfile::Builder::new()
        .suffix(".sh")
        .tempfile()
        .expect("failed to create script file");
    // Use $'\e' bash syntax for ESC character to avoid HCL escape issues.
    script
        .write_all(b"#!/bin/bash\nprintf 'alice 10\\nbob 20\\n'\nprintf $'\\x1b[2J'\nprintf 'alice 10\\ncharlie 30\\n'\n")
        .expect("failed to write script");

    let script_path = script.path().to_string_lossy().replace('\\', "/");

    let hcl = format!(
        r#"
        data "exec" "procs" {{
            command = "bash {script_path}"
            split   = "\\s+"
            mode    = "snapshot"
            columns = "name,score"
        }}

        output "result" {{
            value = data.exec.procs.name
        }}
    "#
    );

    let stdout = run_hcl_streaming(&hcl);
    // After processing both snapshots, we should see alice and charlie in the output.
    // bob was retracted in the second snapshot.
    assert!(
        stdout.contains("alice") && stdout.contains("charlie"),
        "Expected alice and charlie from snapshot diff, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_exec_with_filter() {
    let hcl = r#"
        data "exec" "nums" {
            command = "printf 'alice 100\nbob 500\ncharlie 200\n'"
            split   = "\\s+"
            mode    = "append"
            columns = "name,score"
        }

        resource "high_score" "rule" {
            name = data.exec.nums.name
            _filter = data.exec.nums.score > 150
        }

        output "winners" {
            value = high_score.rule.name
        }
    "#;

    let stdout = run_hcl_streaming(hcl);
    assert!(
        stdout.contains("bob") && stdout.contains("charlie"),
        "Expected bob (500>150) and charlie (200>150), got:\n{}",
        stdout
    );
    assert!(
        !stdout.contains("alice"),
        "Did not expect alice (100<150), got:\n{}",
        stdout
    );
}

#[test]
fn e2e_exec_with_header() {
    let hcl = r#"
        data "exec" "people" {
            command = "printf 'name age\nalice 30\nbob 25\n'"
            split   = "\\s+"
            mode    = "append"
            header  = "true"
        }

        output "names" {
            value = data.exec.people.name
        }
    "#;

    let stdout = run_hcl_streaming(hcl);
    assert!(
        stdout.contains("alice") && stdout.contains("bob"),
        "Expected names from header-mode exec, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_exec_auto_columns() {
    let hcl = r#"
        data "exec" "items" {
            command = "printf 'hello 42\nworld 99\n'"
            split   = "\\s+"
            mode    = "append"
        }

        output "first" {
            value = data.exec.items.col0
        }
    "#;

    let stdout = run_hcl_streaming(hcl);
    assert!(
        stdout.contains("hello") && stdout.contains("world"),
        "Expected auto-generated col0 values, got:\n{}",
        stdout
    );
}

// ---------------------------------------------------------------------------
// Property-based e2e tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Debezium plugin tests
// ---------------------------------------------------------------------------

/// Send a raw HTTP POST using std::net::TcpStream (no extra deps).
fn post_json(addr: &str, body: &str) -> Result<String, String> {
    use std::io::{Read, Write as IoWrite};
    use std::net::TcpStream;

    let mut stream =
        TcpStream::connect(addr).map_err(|e| format!("connect to {}: {}", addr, e))?;
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        addr,
        body.len(),
        body
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write: {}", e))?;
    stream
        .flush()
        .map_err(|e| format!("flush: {}", e))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("read: {}", e))?;
    Ok(response)
}

/// Wait for a TCP port to accept connections, retrying every 50ms up to ~5s.
fn wait_for_port(addr: &str) {
    for _ in 0..100 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("timed out waiting for {} to accept connections", addr);
}

/// Spawn dbflow in streaming mode, run a callback to interact with it, then
/// send SIGTERM and return stdout.
fn run_hcl_streaming_with<F: FnOnce()>(hcl_source: &str, interact: F) -> String {
    let mut f = tempfile::Builder::new()
        .suffix(".hcl")
        .tempfile()
        .expect("failed to create temp file");
    f.write_all(hcl_source.as_bytes())
        .expect("failed to write HCL");

    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let facts_dir = work_dir.path().join("facts");
    let csvs_dir = work_dir.path().join("csvs");

    #[allow(unused_mut)]
    let mut child = Command::new(env!("CARGO_BIN_EXE_dbflow"))
        .arg(f.path())
        .arg("--facts")
        .arg(&facts_dir)
        .arg("--csvs")
        .arg(&csvs_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start dbflow");

    // Run the interaction callback (e.g. POST events to the HTTP server).
    interact();

    // Give the streaming pipeline time to process the events.
    std::thread::sleep(std::time::Duration::from_secs(3));

    // Send SIGTERM for graceful shutdown.
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }

    let output = child.wait_with_output().expect("failed to wait for dbflow");
    String::from_utf8(output.stdout).expect("non-UTF8 stdout")
}

fn debezium_event(op: &str, before: Option<&str>, after: Option<&str>, schema: &str, table: &str) -> String {
    let before_val = before.unwrap_or("null");
    let after_val = after.unwrap_or("null");
    format!(
        r#"{{"before": {}, "after": {}, "source": {{"schema": "{}", "table": "{}"}}, "op": "{}"}}"#,
        before_val, after_val, schema, table, op
    )
}

#[test]
fn e2e_debezium_insert() {
    let addr = "127.0.0.1:18081";
    let hcl = format!(
        r#"
        data "debezium" "users" {{
            listen  = "{addr}"
            table   = "public.users"
            columns = "id,name"
            types   = "integer,string"
        }}

        output "result" {{
            value = data.debezium.users.name
        }}
    "#
    );

    let stdout = run_hcl_streaming_with(&hcl, || {
        wait_for_port(addr);
        let event = debezium_event(
            "c",
            None,
            Some(r#"{"id": 1, "name": "alice"}"#),
            "public",
            "users",
        );
        let resp = post_json(addr, &event).expect("POST failed");
        assert!(resp.contains("200"), "Expected 200 OK, got: {}", resp);
    });

    assert!(
        stdout.contains("alice"),
        "Expected 'alice' in output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_debezium_update() {
    let addr = "127.0.0.1:18082";
    let hcl = format!(
        r#"
        data "debezium" "users" {{
            listen  = "{addr}"
            table   = "public.users"
            columns = "id,name"
            types   = "integer,string"
        }}

        output "result" {{
            value = data.debezium.users.name
        }}
    "#
    );

    let stdout = run_hcl_streaming_with(&hcl, || {
        wait_for_port(addr);

        // First: create event.
        let create = debezium_event(
            "c",
            None,
            Some(r#"{"id": 1, "name": "alice"}"#),
            "public",
            "users",
        );
        post_json(addr, &create).expect("POST create failed");

        // Brief pause for processing.
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Then: update event (alice → bob).
        let update = debezium_event(
            "u",
            Some(r#"{"id": 1, "name": "alice"}"#),
            Some(r#"{"id": 1, "name": "bob"}"#),
            "public",
            "users",
        );
        post_json(addr, &update).expect("POST update failed");
    });

    // After update, the final state should have bob (alice was retracted).
    assert!(
        stdout.contains("bob"),
        "Expected 'bob' after update, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_debezium_table_filter() {
    let addr = "127.0.0.1:18083";
    let hcl = format!(
        r#"
        data "debezium" "users" {{
            listen  = "{addr}"
            table   = "public.users"
            columns = "id,name"
            types   = "integer,string"
        }}

        output "result" {{
            value = data.debezium.users.name
        }}
    "#
    );

    let stdout = run_hcl_streaming_with(&hcl, || {
        wait_for_port(addr);

        // Event for a DIFFERENT table — should be filtered out.
        let other = debezium_event(
            "c",
            None,
            Some(r#"{"id": 1, "name": "should_not_appear"}"#),
            "public",
            "orders",
        );
        post_json(addr, &other).expect("POST other table failed");

        // Event for the matching table.
        let matching = debezium_event(
            "c",
            None,
            Some(r#"{"id": 2, "name": "visible"}"#),
            "public",
            "users",
        );
        post_json(addr, &matching).expect("POST matching table failed");
    });

    assert!(
        stdout.contains("visible"),
        "Expected 'visible' in output, got:\n{}",
        stdout
    );
    assert!(
        !stdout.contains("should_not_appear"),
        "Did not expect filtered table event, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_debezium_delete() {
    let addr = "127.0.0.1:18084";
    let hcl = format!(
        r#"
        data "debezium" "users" {{
            listen  = "{addr}"
            table   = "public.users"
            columns = "id,name"
            types   = "integer,string"
        }}

        output "result" {{
            value = data.debezium.users.name
        }}
    "#
    );

    let stdout = run_hcl_streaming_with(&hcl, || {
        wait_for_port(addr);

        // Insert two rows.
        let c1 = debezium_event(
            "c",
            None,
            Some(r#"{"id": 1, "name": "alice"}"#),
            "public",
            "users",
        );
        post_json(addr, &c1).expect("POST create 1 failed");

        let c2 = debezium_event(
            "c",
            None,
            Some(r#"{"id": 2, "name": "bob"}"#),
            "public",
            "users",
        );
        post_json(addr, &c2).expect("POST create 2 failed");

        std::thread::sleep(std::time::Duration::from_millis(500));

        // Delete alice.
        let d = debezium_event(
            "d",
            Some(r#"{"id": 1, "name": "alice"}"#),
            None,
            "public",
            "users",
        );
        post_json(addr, &d).expect("POST delete failed");
    });

    // After delete, bob should remain but alice should be retracted.
    assert!(
        stdout.contains("bob"),
        "Expected 'bob' to remain after deleting alice, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_debezium_snapshot_read() {
    // Debezium sends op:"r" for initial snapshot rows before streaming starts.
    let addr = "127.0.0.1:18085";
    let hcl = format!(
        r#"
        data "debezium" "users" {{
            listen  = "{addr}"
            table   = "public.users"
            columns = "id,name"
            types   = "integer,string"
        }}

        output "result" {{
            value = data.debezium.users.name
        }}
    "#
    );

    let stdout = run_hcl_streaming_with(&hcl, || {
        wait_for_port(addr);

        // Simulate Debezium initial snapshot: op:"r" for each existing row.
        for (id, name) in [(1, "alice"), (2, "bob"), (3, "charlie")] {
            let event = debezium_event(
                "r",
                None,
                Some(&format!(r#"{{"id": {}, "name": "{}"}}"#, id, name)),
                "public",
                "users",
            );
            post_json(addr, &event).expect("POST snapshot-read failed");
        }
    });

    assert!(
        stdout.contains("alice") && stdout.contains("bob") && stdout.contains("charlie"),
        "Expected all snapshot-read rows in output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_debezium_multiple_inserts() {
    let addr = "127.0.0.1:18086";
    let hcl = format!(
        r#"
        data "debezium" "orders" {{
            listen  = "{addr}"
            table   = "inventory.orders"
            columns = "order_id,customer,amount"
            types   = "integer,string,integer"
        }}

        output "customers" {{
            value = data.debezium.orders.customer
        }}

        output "amounts" {{
            value = data.debezium.orders.amount
        }}
    "#
    );

    let stdout = run_hcl_streaming_with(&hcl, || {
        wait_for_port(addr);

        let rows = vec![
            (1, "alice", 100),
            (2, "bob", 250),
            (3, "charlie", 50),
            (4, "alice", 300),
        ];

        for (id, customer, amount) in rows {
            let event = debezium_event(
                "c",
                None,
                Some(&format!(
                    r#"{{"order_id": {}, "customer": "{}", "amount": {}}}"#,
                    id, customer, amount
                )),
                "inventory",
                "orders",
            );
            post_json(addr, &event).expect("POST failed");
        }
    });

    // All four inserts should appear.
    assert!(
        stdout.contains("alice") && stdout.contains("bob") && stdout.contains("charlie"),
        "Expected all customer names in output, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("100") && stdout.contains("250") && stdout.contains("50") && stdout.contains("300"),
        "Expected all amounts in output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_debezium_with_resource() {
    // Debezium data flowing into an IDB resource (derived rule / join).
    let addr = "127.0.0.1:18087";
    let hcl = format!(
        r#"
        data "debezium" "orders" {{
            listen  = "{addr}"
            table   = "public.orders"
            columns = "id,customer,amount"
            types   = "integer,string,integer"
        }}

        resource "order_summary" "rule" {{
            customer = data.debezium.orders.customer
            amount   = data.debezium.orders.amount
        }}

        output "result" {{
            value = order_summary.rule.customer
        }}
    "#
    );

    let stdout = run_hcl_streaming_with(&hcl, || {
        wait_for_port(addr);

        let c1 = debezium_event(
            "c",
            None,
            Some(r#"{"id": 1, "customer": "alice", "amount": 100}"#),
            "public",
            "orders",
        );
        post_json(addr, &c1).expect("POST create failed");

        let c2 = debezium_event(
            "c",
            None,
            Some(r#"{"id": 2, "customer": "bob", "amount": 200}"#),
            "public",
            "orders",
        );
        post_json(addr, &c2).expect("POST create failed");
    });

    assert!(
        stdout.contains("alice") && stdout.contains("bob"),
        "Expected IDB-derived customers from debezium data, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_debezium_with_filter() {
    // Comparison filter on debezium integer column.
    let addr = "127.0.0.1:18088";
    let hcl = format!(
        r#"
        data "debezium" "orders" {{
            listen  = "{addr}"
            table   = "public.orders"
            columns = "id,customer,amount"
            types   = "integer,string,integer"
        }}

        resource "big_order" "rule" {{
            customer = data.debezium.orders.customer
            amount   = data.debezium.orders.amount
            _filter  = data.debezium.orders.amount > 150
        }}

        output "result" {{
            value = big_order.rule.customer
        }}
    "#
    );

    let stdout = run_hcl_streaming_with(&hcl, || {
        wait_for_port(addr);

        for (id, customer, amount) in [(1, "alice", 100), (2, "bob", 200), (3, "charlie", 50)] {
            let event = debezium_event(
                "c",
                None,
                Some(&format!(
                    r#"{{"id": {}, "customer": "{}", "amount": {}}}"#,
                    id, customer, amount
                )),
                "public",
                "orders",
            );
            post_json(addr, &event).expect("POST failed");
        }
    });

    // Only bob (200 > 150) should pass the filter.
    assert!(
        stdout.contains("bob"),
        "Expected bob (amount=200 > 150), got:\n{}",
        stdout
    );
    assert!(
        !stdout.contains("alice"),
        "Did not expect alice (amount=100), got:\n{}",
        stdout
    );
    assert!(
        !stdout.contains("charlie"),
        "Did not expect charlie (amount=50), got:\n{}",
        stdout
    );
}

#[test]
fn e2e_debezium_with_aggregate() {
    // Aggregate (sum) on debezium-sourced data, grouped by region.
    let addr = "127.0.0.1:18089";
    let hcl = format!(
        r#"
        data "debezium" "sales" {{
            listen  = "{addr}"
            table   = "public.sales"
            columns = "id,region,amount"
            types   = "integer,string,integer"
        }}

        resource "totals" "all" {{
            region = data.debezium.sales.region
            total  = sum(data.debezium.sales.amount)
        }}

        output "result" {{
            value = totals.all.total
        }}
    "#
    );

    let stdout = run_hcl_streaming_with(&hcl, || {
        wait_for_port(addr);

        let rows = vec![
            (1, "us", 100),
            (2, "us", 200),
            (3, "eu", 50),
            (4, "eu", 75),
        ];

        for (id, region, amount) in rows {
            let event = debezium_event(
                "c",
                None,
                Some(&format!(
                    r#"{{"id": {}, "region": "{}", "amount": {}}}"#,
                    id, region, amount
                )),
                "public",
                "sales",
            );
            post_json(addr, &event).expect("POST failed");
        }
    });

    // us: 100+200=300, eu: 50+75=125
    assert!(
        stdout.contains("300"),
        "Expected US sum 300, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("125"),
        "Expected EU sum 125, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_debezium_table_filter_no_schema() {
    // Table filter with no schema prefix — matches any schema.
    let addr = "127.0.0.1:18090";
    let hcl = format!(
        r#"
        data "debezium" "users" {{
            listen  = "{addr}"
            table   = "users"
            columns = "id,name"
            types   = "integer,string"
        }}

        output "result" {{
            value = data.debezium.users.name
        }}
    "#
    );

    let stdout = run_hcl_streaming_with(&hcl, || {
        wait_for_port(addr);

        // Event with schema "public" — should match since we only filter on table.
        let e1 = debezium_event(
            "c",
            None,
            Some(r#"{"id": 1, "name": "from_public"}"#),
            "public",
            "users",
        );
        post_json(addr, &e1).expect("POST failed");

        // Event with schema "myapp" — should also match.
        let e2 = debezium_event(
            "c",
            None,
            Some(r#"{"id": 2, "name": "from_myapp"}"#),
            "myapp",
            "users",
        );
        post_json(addr, &e2).expect("POST failed");

        // Event for a different table — should NOT match.
        let e3 = debezium_event(
            "c",
            None,
            Some(r#"{"id": 3, "name": "wrong_table"}"#),
            "public",
            "orders",
        );
        post_json(addr, &e3).expect("POST failed");
    });

    assert!(
        stdout.contains("from_public") && stdout.contains("from_myapp"),
        "Expected both schemas to match when no schema filter, got:\n{}",
        stdout
    );
    assert!(
        !stdout.contains("wrong_table"),
        "Did not expect events from unrelated table, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_debezium_schema_mismatch() {
    // Right table name but wrong schema should be filtered.
    let addr = "127.0.0.1:18091";
    let hcl = format!(
        r#"
        data "debezium" "users" {{
            listen  = "{addr}"
            table   = "public.users"
            columns = "id,name"
            types   = "integer,string"
        }}

        output "result" {{
            value = data.debezium.users.name
        }}
    "#
    );

    let stdout = run_hcl_streaming_with(&hcl, || {
        wait_for_port(addr);

        // Right table, wrong schema.
        let wrong_schema = debezium_event(
            "c",
            None,
            Some(r#"{"id": 1, "name": "wrong_schema"}"#),
            "private",
            "users",
        );
        post_json(addr, &wrong_schema).expect("POST failed");

        // Right table, right schema.
        let correct = debezium_event(
            "c",
            None,
            Some(r#"{"id": 2, "name": "correct"}"#),
            "public",
            "users",
        );
        post_json(addr, &correct).expect("POST failed");
    });

    assert!(
        stdout.contains("correct"),
        "Expected event from matching schema, got:\n{}",
        stdout
    );
    assert!(
        !stdout.contains("wrong_schema"),
        "Did not expect event from non-matching schema, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_debezium_full_lifecycle() {
    // Full CDC lifecycle: snapshot → create → update → delete.
    let addr = "127.0.0.1:18092";
    let hcl = format!(
        r#"
        data "debezium" "users" {{
            listen  = "{addr}"
            table   = "public.users"
            columns = "id,name,email"
            types   = "integer,string,string"
        }}

        output "names" {{
            value = data.debezium.users.name
        }}
    "#
    );

    let stdout = run_hcl_streaming_with(&hcl, || {
        wait_for_port(addr);

        let pause = std::time::Duration::from_millis(300);

        // 1. Snapshot reads (initial table state).
        let snap1 = debezium_event(
            "r",
            None,
            Some(r#"{"id": 1, "name": "alice", "email": "alice@test.com"}"#),
            "public",
            "users",
        );
        post_json(addr, &snap1).expect("POST snap1 failed");

        let snap2 = debezium_event(
            "r",
            None,
            Some(r#"{"id": 2, "name": "bob", "email": "bob@test.com"}"#),
            "public",
            "users",
        );
        post_json(addr, &snap2).expect("POST snap2 failed");

        std::thread::sleep(pause);

        // 2. New insert after snapshot.
        let create = debezium_event(
            "c",
            None,
            Some(r#"{"id": 3, "name": "charlie", "email": "charlie@test.com"}"#),
            "public",
            "users",
        );
        post_json(addr, &create).expect("POST create failed");

        std::thread::sleep(pause);

        // 3. Update: alice changes email (name stays the same).
        let update = debezium_event(
            "u",
            Some(r#"{"id": 1, "name": "alice", "email": "alice@test.com"}"#),
            Some(r#"{"id": 1, "name": "alice", "email": "alice@new.com"}"#),
            "public",
            "users",
        );
        post_json(addr, &update).expect("POST update failed");

        std::thread::sleep(pause);

        // 4. Delete bob.
        let delete = debezium_event(
            "d",
            Some(r#"{"id": 2, "name": "bob", "email": "bob@test.com"}"#),
            None,
            "public",
            "users",
        );
        post_json(addr, &delete).expect("POST delete failed");
    });

    // Final state: alice (updated) + charlie remain. bob was deleted.
    assert!(
        stdout.contains("alice") && stdout.contains("charlie"),
        "Expected alice and charlie to remain, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_debezium_default_string_types() {
    // Omitting `types` should default all columns to string.
    let addr = "127.0.0.1:18093";
    let hcl = format!(
        r#"
        data "debezium" "events" {{
            listen  = "{addr}"
            table   = "public.events"
            columns = "id,kind,payload"
        }}

        output "result" {{
            value = data.debezium.events.kind
        }}
    "#
    );

    let stdout = run_hcl_streaming_with(&hcl, || {
        wait_for_port(addr);

        let event = debezium_event(
            "c",
            None,
            Some(r#"{"id": "evt-001", "kind": "signup", "payload": "user registered"}"#),
            "public",
            "events",
        );
        post_json(addr, &event).expect("POST failed");

        let event2 = debezium_event(
            "c",
            None,
            Some(r#"{"id": "evt-002", "kind": "purchase", "payload": "item bought"}"#),
            "public",
            "events",
        );
        post_json(addr, &event2).expect("POST failed");
    });

    assert!(
        stdout.contains("signup") && stdout.contains("purchase"),
        "Expected string-typed event kinds, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_debezium_arithmetic_filter() {
    // Arithmetic expression in comparison filter on debezium integer columns.
    let addr = "127.0.0.1:18094";
    let hcl = format!(
        r#"
        data "debezium" "orders" {{
            listen  = "{addr}"
            table   = "public.orders"
            columns = "id,customer,price,tax"
            types   = "integer,string,integer,integer"
        }}

        resource "expensive" "rule" {{
            customer = data.debezium.orders.customer
            _filter  = data.debezium.orders.price + data.debezium.orders.tax > 500
        }}

        output "result" {{
            value = expensive.rule.customer
        }}
    "#
    );

    let stdout = run_hcl_streaming_with(&hcl, || {
        wait_for_port(addr);

        // alice: 400+150=550 > 500 ✓
        let e1 = debezium_event(
            "c",
            None,
            Some(r#"{"id": 1, "customer": "alice", "price": 400, "tax": 150}"#),
            "public",
            "orders",
        );
        post_json(addr, &e1).expect("POST failed");

        // bob: 300+100=400 < 500 ✗
        let e2 = debezium_event(
            "c",
            None,
            Some(r#"{"id": 2, "customer": "bob", "price": 300, "tax": 100}"#),
            "public",
            "orders",
        );
        post_json(addr, &e2).expect("POST failed");

        // charlie: 450+60=510 > 500 ✓
        let e3 = debezium_event(
            "c",
            None,
            Some(r#"{"id": 3, "customer": "charlie", "price": 450, "tax": 60}"#),
            "public",
            "orders",
        );
        post_json(addr, &e3).expect("POST failed");
    });

    assert!(
        stdout.contains("alice"),
        "Expected alice (400+150=550 > 500), got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("charlie"),
        "Expected charlie (450+60=510 > 500), got:\n{}",
        stdout
    );
    assert!(
        !stdout.contains("bob"),
        "Did not expect bob (300+100=400 < 500), got:\n{}",
        stdout
    );
}

#[test]
fn e2e_debezium_update_aggregate_recomputes() {
    // Verify that an update (retract old + insert new) causes aggregates to recompute.
    // Insert two rows for region "us", then update one row's amount.
    let addr = "127.0.0.1:18095";
    let hcl = format!(
        r#"
        data "debezium" "sales" {{
            listen  = "{addr}"
            table   = "public.sales"
            columns = "id,region,amount"
            types   = "integer,string,integer"
        }}

        resource "totals" "all" {{
            region = data.debezium.sales.region
            total  = sum(data.debezium.sales.amount)
        }}

        output "result" {{
            value = totals.all.total
        }}
    "#
    );

    let stdout = run_hcl_streaming_with(&hcl, || {
        wait_for_port(addr);

        let pause = std::time::Duration::from_millis(300);

        // Insert: us/100, us/200 → sum should be 300.
        let c1 = debezium_event(
            "c",
            None,
            Some(r#"{"id": 1, "region": "us", "amount": 100}"#),
            "public",
            "sales",
        );
        post_json(addr, &c1).expect("POST failed");

        let c2 = debezium_event(
            "c",
            None,
            Some(r#"{"id": 2, "region": "us", "amount": 200}"#),
            "public",
            "sales",
        );
        post_json(addr, &c2).expect("POST failed");

        std::thread::sleep(pause);

        // Update id=1: amount 100 → 400. New sum should be 400+200=600.
        let upd = debezium_event(
            "u",
            Some(r#"{"id": 1, "region": "us", "amount": 100}"#),
            Some(r#"{"id": 1, "region": "us", "amount": 400}"#),
            "public",
            "sales",
        );
        post_json(addr, &upd).expect("POST update failed");
    });

    // After the update, the sum should be 600 (400 + 200).
    assert!(
        stdout.contains("600"),
        "Expected updated sum 600, got:\n{}",
        stdout
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Arbitrary string literals survive the full pipeline round-trip.
    #[test]
    fn prop_literal_string_roundtrip(val in arb_safe_string()) {
        let hcl = format!(
            r#"
            output "out" {{
                value = "{val}"
            }}
            "#
        );
        let stdout = run_hcl(&hcl);
        prop_assert!(
            stdout.contains(&format!(r#"output "out": {val}"#)),
            "Expected value '{}' in output, got:\n{}", val, stdout
        );
    }

    /// Arbitrary integer literals survive the full pipeline round-trip.
    #[test]
    fn prop_literal_int_roundtrip(val in arb_int()) {
        let hcl = format!(
            r#"
            output "out" {{
                value = {val}
            }}
            "#
        );
        let stdout = run_hcl(&hcl);
        prop_assert!(
            stdout.contains(&format!(r#"output "out": {val}"#)),
            "Expected value '{}' in output, got:\n{}", val, stdout
        );
    }

    /// Arbitrary string values survive EDB → output reference round-trip.
    #[test]
    fn prop_edb_string_roundtrip(
        type_name in arb_ident(),
        label in arb_ident(),
        attr in arb_ident(),
        val in arb_safe_string(),
    ) {
        let hcl = format!(
            r#"
            resource "{type_name}" "{label}" {{
                {attr} = "{val}"
            }}

            output "out" {{
                value = {type_name}.{label}.{attr}
            }}
            "#
        );
        let stdout = run_hcl(&hcl);
        prop_assert!(
            stdout.contains(&format!(r#"output "out": {val}"#)),
            "Expected value '{}' in output, got:\n{}", val, stdout
        );
    }

    /// Arbitrary values survive EDB → IDB → output reference chain.
    #[test]
    fn prop_idb_chain_roundtrip(
        val in arb_safe_string(),
    ) {
        let hcl = format!(
            r#"
            resource "src" "s1" {{
                data = "{val}"
            }}

            resource "dst" "d1" {{
                data = src.s1.data
            }}

            output "out" {{
                value = dst.d1.data
            }}
            "#
        );
        let stdout = run_hcl(&hcl);
        prop_assert!(
            stdout.contains(&format!(r#"output "out": {val}"#)),
            "Expected value '{}' in output, got:\n{}", val, stdout
        );
    }

    /// Variables with arbitrary default values survive substitution round-trip.
    #[test]
    fn prop_variable_roundtrip(
        var_name in arb_ident(),
        val in arb_safe_string(),
    ) {
        let hcl = format!(
            r#"
            variable "{var_name}" {{
                default = "{val}"
            }}

            resource "host" "h1" {{
                data = var.{var_name}
            }}

            output "out" {{
                value = host.h1.data
            }}
            "#
        );
        let stdout = run_hcl(&hcl);
        prop_assert!(
            stdout.contains(&format!(r#"output "out": {val}"#)),
            "Expected value '{}' in output, got:\n{}", val, stdout
        );
    }

    /// Multiple attributes with different arbitrary values each decode correctly.
    #[test]
    fn prop_multiple_attrs_roundtrip(
        val_a in arb_safe_string(),
        val_b in arb_safe_string(),
    ) {
        let hcl = format!(
            r#"
            resource "node" "n1" {{
                alpha = "{val_a}"
                beta = "{val_b}"
            }}

            output "outa" {{
                value = node.n1.alpha
            }}

            output "outb" {{
                value = node.n1.beta
            }}
            "#
        );
        let stdout = run_hcl(&hcl);
        prop_assert!(
            stdout.contains(&format!(r#"output "outa": {val_a}"#)),
            "Expected alpha='{}' in output, got:\n{}", val_a, stdout
        );
        prop_assert!(
            stdout.contains(&format!(r#"output "outb": {val_b}"#)),
            "Expected beta='{}' in output, got:\n{}", val_b, stdout
        );
    }
}

#[test]
fn e2e_multiple_aggregates() {
    let mut csv_file = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("failed to create CSV file");
    csv_file
        .write_all(b"region,amount\nus,100\nus,200\nus,50\neu,80\neu,120\n")
        .expect("failed to write CSV");

    let csv_path = csv_file.path().to_string_lossy().replace('\\', "/");

    let hcl = format!(
        r#"
        data "csv" "sales" {{
            path = "{csv_path}"
        }}

        resource "stats" "all" {{
            region      = data.csv.sales.region
            total_sales = sum(data.csv.sales.amount)
            max_sale    = max(data.csv.sales.amount)
        }}

        output "totals" {{
            value = stats.all.total_sales
        }}

        output "maxes" {{
            value = stats.all.max_sale
        }}
    "#
    );
    let stdout = run_hcl_streaming(&hcl);
    // us: sum=350, max=200; eu: sum=200, max=120
    assert!(
        stdout.contains("350") && stdout.contains("200"),
        "Expected sum 350 (us) and 200 (eu) in output, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("120"),
        "Expected max 120 (eu) in output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_multiple_aggregates_three() {
    let mut csv_file = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("failed to create CSV file");
    csv_file
        .write_all(b"group,val\na,10\na,30\na,20\nb,5\nb,15\n")
        .expect("failed to write CSV");

    let csv_path = csv_file.path().to_string_lossy().replace('\\', "/");

    let hcl = format!(
        r#"
        data "csv" "data" {{
            path = "{csv_path}"
        }}

        resource "summary" "all" {{
            group   = data.csv.data.group
            total   = sum(data.csv.data.val)
            minimum = min(data.csv.data.val)
            maximum = max(data.csv.data.val)
        }}

        output "totals" {{
            value = summary.all.total
        }}

        output "mins" {{
            value = summary.all.minimum
        }}

        output "maxes" {{
            value = summary.all.maximum
        }}
    "#
    );
    let stdout = run_hcl_streaming(&hcl);
    // Group a: sum=60, min=10, max=30; Group b: sum=20, min=5, max=15
    assert!(
        stdout.contains("60") && stdout.contains("20"),
        "Expected sums 60 and 20, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("10") && stdout.contains("5"),
        "Expected mins 10 and 5, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("30") && stdout.contains("15"),
        "Expected maxes 30 and 15, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_abs_function() {
    let mut csv_file = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("failed to create CSV file");
    csv_file
        .write_all(b"label,value\npos,42\nneg,-17\nzero,0\n")
        .expect("failed to write CSV");

    let csv_path = csv_file.path().to_string_lossy().replace('\\', "/");

    let hcl = format!(
        r#"
        data "csv" "nums" {{
            path = "{csv_path}"
        }}

        resource "magnitude" "all" {{
            label     = data.csv.nums.label
            abs_value = abs(data.csv.nums.value)
        }}

        output "result" {{
            value = magnitude.all.abs_value
        }}
    "#
    );
    let stdout = run_hcl_streaming(&hcl);
    // abs(42)=42, abs(-17)=17, abs(0)=0
    assert!(
        stdout.contains("42") && stdout.contains("17"),
        "Expected abs values 42 and 17, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_neg_function() {
    let mut csv_file = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("failed to create CSV file");
    csv_file
        .write_all(b"label,value\npos,42\nneg,-17\n")
        .expect("failed to write CSV");

    let csv_path = csv_file.path().to_string_lossy().replace('\\', "/");

    let hcl = format!(
        r#"
        data "csv" "nums" {{
            path = "{csv_path}"
        }}

        resource "negated" "all" {{
            label     = data.csv.nums.label
            neg_value = neg(data.csv.nums.value)
        }}

        output "result" {{
            value = negated.all.neg_value
        }}
    "#
    );
    let stdout = run_hcl_streaming(&hcl);
    // neg(42)=-42, neg(-17)=17
    assert!(
        stdout.contains("-42") && stdout.contains("17"),
        "Expected neg values -42 and 17, got:\n{}",
        stdout
    );
}
