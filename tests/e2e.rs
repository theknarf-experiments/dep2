use std::io::Write;
use std::process::Command;

use proptest::prelude::*;

/// Run the hcl-flowlog binary on a temporary HCL file with isolated temp directories.
/// Returns stdout as a String. Panics if the process exits non-zero.
fn run_hcl(hcl_source: &str) -> String {
    run_hcl_with_args(hcl_source, &[])
}

/// Run the hcl-flowlog binary with extra CLI args and isolated temp directories.
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

    let output = Command::new(env!("CARGO_BIN_EXE_hcl-flowlog"))
        .arg(f.path())
        .arg("--facts")
        .arg(&facts_dir)
        .arg("--csvs")
        .arg(&csvs_dir)
        .args(args)
        .output()
        .expect("failed to execute hcl-flowlog");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!(
            "hcl-flowlog exited with {}\nstderr:\n{}",
            output.status, stderr
        );
    }

    String::from_utf8(output.stdout).expect("non-UTF8 stdout")
}

#[test]
fn e2e_literal_output() {
    let stdout = run_hcl(r#"
        output "greeting" {
            value = "hello"
        }
    "#);
    assert!(
        stdout.contains(r#"output "greeting": hello"#),
        "Expected greeting output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_integer_output() {
    let stdout = run_hcl(r#"
        output "port" {
            value = 8080
        }
    "#);
    assert!(
        stdout.contains(r#"output "port": 8080"#),
        "Expected port output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_edb_with_output() {
    let stdout = run_hcl(r#"
        resource "server" "w1" {
            ip = "10.0.0.5"
        }

        output "server_ip" {
            value = server.w1.ip
        }
    "#);
    assert!(
        stdout.contains(r#"output "server_ip": 10.0.0.5"#),
        "Expected server_ip output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_idb_with_output() {
    let stdout = run_hcl(r#"
        resource "server" "w1" {
            ip = "10.0.0.5"
        }

        resource "monitor" "m1" {
            target_ip = server.w1.ip
        }

        output "monitored_ip" {
            value = monitor.m1.target_ip
        }
    "#);
    assert!(
        stdout.contains(r#"output "monitored_ip": 10.0.0.5"#),
        "Expected monitored_ip output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_variable_substitution() {
    let stdout = run_hcl(r#"
        variable "addr" {
            default = "192.168.1.1"
        }

        resource "host" "h1" {
            ip = var.addr
        }

        output "host_ip" {
            value = host.h1.ip
        }
    "#);
    assert!(
        stdout.contains(r#"output "host_ip": 192.168.1.1"#),
        "Expected host_ip output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_multiple_outputs() {
    let stdout = run_hcl(r#"
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
    "#);
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
// Reference detection and recursion tests
// ---------------------------------------------------------------------------

/// Run the hcl-flowlog binary expecting it may fail. Returns (success, stdout, stderr).
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

    let output = Command::new(env!("CARGO_BIN_EXE_hcl-flowlog"))
        .arg(f.path())
        .arg("--facts")
        .arg(&facts_dir)
        .arg("--csvs")
        .arg(&csvs_dir)
        .output()
        .expect("failed to execute hcl-flowlog");

    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn e2e_deep_acyclic_chain() {
    let stdout = run_hcl(r#"
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
    "#);
    assert!(
        stdout.contains(r#"output "result": deep"#),
        "Expected deep chain output, got:\n{}",
        stdout
    );
}

#[test]
fn e2e_diamond_dependency() {
    let stdout = run_hcl(r#"
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
    "#);
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
    let stdout = run_hcl(r#"
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
    "#);
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
    let (success, stdout, stderr) = run_hcl_result(r#"
        resource "a" "r" {
            link = b.r.link
        }

        resource "b" "r" {
            link = a.r.link
        }

        output "result" {
            value = a.r.link
        }
    "#);
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
    let (success, stdout, stderr) = run_hcl_result(r#"
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
    "#);
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
    let (success, stdout, stderr) = run_hcl_result(r#"
        resource "loop" "r" {
            val = loop.r.val
        }

        output "result" {
            value = loop.r.val
        }
    "#);
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
