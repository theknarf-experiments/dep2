//! End-to-end test over real TCP: spawn the compiled `dep2` binary with a CSV
//! source, then drive the full HTTP surface — relations, program, and the
//! whole runtime-query lifecycle (add, introspect, rows, drop) — with
//! hand-rolled HTTP/1.0 requests. Everything the unit tests exercise via
//! `route_query` directly runs here through tiny_http, headers, and JSON
//! serialization on the wire.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PROG: &str = "\
.in
.decl edge(x: number, y: number)

.printsize
.decl tc(x: number, y: number)

.rule
tc(X, Y) :- edge(X, Y).
tc(X, Y) :- tc(X, Z), edge(Z, Y).
";

const QUERY_PROG: &str = "\
.in
.decl tc(x: number, y: number)

.printsize
.decl q(x: number)

.rule
q(X) :- tc(X, _).
";

/// One HTTP/1.0 request; the server closes the connection after responding.
fn http(addr: &str, method: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: {addr}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).expect("write request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read response");

    let status: u16 = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("bad status line in: {raw}"));
    let json_body = raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or_default();
    let value = if json_body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(json_body)
            .unwrap_or_else(|e| panic!("non-JSON body ({e}): {json_body}"))
    };
    (status, value)
}

/// Poll `path` until `pred` accepts the response body or the deadline passes.
fn poll_until(
    addr: &str,
    path: &str,
    pred: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (status, body) = http(addr, "GET", path, "");
        if status == 200 && pred(&body) {
            return body;
        }
        assert!(
            Instant::now() < deadline,
            "timed out polling {path}; last: {status} {body}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Kill the child on drop so a failing assertion doesn't leak the engine.
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn full_query_lifecycle_over_tcp() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("edge.csv");
    std::fs::write(&csv, "x,y\n1,2\n2,3\n").unwrap();
    let prog = dir.path().join("prog.dl");
    std::fs::write(&prog, PROG).unwrap();

    // An OS-assigned free port; tiny racy between drop and bind, fine in tests.
    let addr = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };

    let child = Command::new(env!("CARGO_BIN_EXE_dep2"))
        .args([
            "run",
            prog.to_str().unwrap(),
            "-s",
            &format!("edge=csv:path={}", csv.display()),
            "--addr",
            &addr,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dep2");
    let _guard = KillOnDrop(child);

    // Engine + server come up, the CSV streams in, tc converges to 3 rows.
    let deadline = Instant::now() + Duration::from_secs(30);
    while TcpStream::connect(&addr).is_err() {
        assert!(Instant::now() < deadline, "server never came up on {addr}");
        std::thread::sleep(Duration::from_millis(100));
    }
    poll_until(&addr, "/relations", |b| {
        b["relations"]
            .as_array()
            .is_some_and(|rels| rels.iter().any(|r| r["name"] == "tc" && r["count"] == 3))
    });

    // The base surface: rows decode on the wire, the program round-trips.
    let (status, body) = http(&addr, "GET", "/relations/tc", "");
    assert_eq!(status, 200);
    let mut rows: Vec<Vec<&str>> = body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            r.as_array()
                .unwrap()
                .iter()
                .map(|c| c.as_str().unwrap())
                .collect()
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![vec!["1", "2"], vec!["1", "3"], vec!["2", "3"]],
        "transitive closure over the wire"
    );
    let (status, body) = http(&addr, "GET", "/program", "");
    assert_eq!(status, 200);
    assert!(body["source"]
        .as_str()
        .unwrap()
        .contains("tc(X, Y) :- edge(X, Y)."));

    // Add a runtime query over the published tc.
    let add = serde_json::json!({ "id": "q1", "program": QUERY_PROG }).to_string();
    let (status, body) = http(&addr, "POST", "/query", &add);
    assert_eq!((status, body["ok"].as_bool()), (200, Some(true)), "{body}");
    let (_, body) = http(&addr, "GET", "/query", "");
    assert_eq!(body["queries"], serde_json::json!(["q1"]));

    // Its dataflow replays history and converges: q(X) :- tc(X, _) -> {1, 2}.
    poll_until(&addr, "/query/q1/relations/q", |b| b["count"] == 2);
    let (_, body) = http(&addr, "GET", "/query/q1/relations/q", "");
    assert_eq!(body["rows"], serde_json::json!([["1"], ["2"]]));

    // Introspection: the program it was added with, plus live counts.
    let (status, body) = http(&addr, "GET", "/query/q1", "");
    assert_eq!(status, 200);
    assert_eq!(body["id"], "q1");
    assert_eq!(body["program"].as_str().unwrap(), QUERY_PROG);
    assert_eq!(
        body["relations"],
        serde_json::json!([{ "name": "q", "count": 2 }])
    );

    // Drop it: gone from the listing, introspection 404s, re-add works.
    let (status, _) = http(&addr, "DELETE", "/query/q1", "");
    assert_eq!(status, 200);
    let (status, _) = http(&addr, "GET", "/query/q1", "");
    assert_eq!(status, 404);
    let (_, body) = http(&addr, "GET", "/query", "");
    assert_eq!(body["queries"].as_array().unwrap().len(), 0);
    let (status, _) = http(&addr, "POST", "/query", &add);
    assert_eq!(status, 200, "id must be reusable after a drop");
    poll_until(&addr, "/query/q1/relations/q", |b| b["count"] == 2);
}
