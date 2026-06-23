//! Read-only HTTP/JSON query API over the engine's live state.
//!
//! Routes:
//!   GET /                      same as /relations
//!   GET /relations             -> { "relations": [ { "name", "count" }, ... ] }
//!   GET /relations/<name>      -> { "name", "rows": [ [col, ...], ... ] }
//!   GET /program               -> { "path", "source" }  (the loaded .dl program)

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dep2_core::engine::{decode_state_row, RelationState, RelationTypes};
use serde::{Serialize, Serializer};
use serde_json::json;
use tiny_http::{Header, Method, Request, Response, Server};

type Resp = Response<Cursor<Vec<u8>>>;

/// The relation-rows response, serialized directly to bytes — no intermediate
/// `serde_json::Value` tree (which would allocate a `Vec<Value>` per row). `name`
/// borrows the request path; `rows` are the decoded rows.
#[derive(Serialize)]
struct RelationRows<'a> {
    name: &'a str,
    count: usize,
    rows: Vec<Vec<String>>,
}

/// A routed response body. Small/error responses stay a `serde_json::Value`
/// (built with `json!`); the potentially-large rows response is a dedicated
/// struct so it serializes straight to bytes. Both serialize the same way, so the
/// HTTP path and the unit tests share one serialization.
enum Body<'a> {
    Json(serde_json::Value),
    Rows(RelationRows<'a>),
}

impl Serialize for Body<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Body::Json(v) => v.serialize(s),
            Body::Rows(r) => r.serialize(s),
        }
    }
}

/// Declared-but-unserved relations -> the rule heads that consume them, used to
/// explain why a relation isn't queryable.
pub type Unserved = Arc<HashMap<String, Vec<String>>>;

/// The loaded program, exposed verbatim (path + source) by `/program`.
pub struct ProgramSource {
    pub path: String,
    pub source: String,
}

/// Serve the query API on `addr` until `shutdown` is set. Blocks the caller, so
/// run it on its own thread.
pub fn serve(
    addr: &str,
    state: Arc<Mutex<RelationState>>,
    types: Arc<RelationTypes>,
    unserved: Unserved,
    program: Arc<ProgramSource>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), String> {
    let server = Server::http(addr).map_err(|e| e.to_string())?;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match server.recv_timeout(Duration::from_millis(200)) {
            Ok(Some(req)) => handle(req, &state, &types, &unserved, &program),
            Ok(None) => continue, // timed out; re-check shutdown
            Err(_) => break,
        }
    }
    Ok(())
}

/// Build an HTTP response from already-serialized JSON bytes.
fn json_bytes_response(body: Vec<u8>, status: u16) -> Resp {
    let content_type = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    // Allow any origin so a browser SPA (e.g. the Vite dev server on another
    // port) can poll this read-only API. Plain GETs are CORS "simple requests"
    // and don't preflight, so a single Allow-Origin header is enough.
    let cors = Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap();
    Response::from_data(body)
        .with_status_code(status)
        .with_header(content_type)
        .with_header(cors)
}

fn json_response(value: serde_json::Value, status: u16) -> Resp {
    json_bytes_response(serde_json::to_vec(&value).unwrap_or_default(), status)
}

fn handle(
    req: Request,
    state: &Arc<Mutex<RelationState>>,
    types: &RelationTypes,
    unserved: &Unserved,
    program: &ProgramSource,
) {
    if *req.method() != Method::Get {
        let _ = req.respond(json_response(
            json!({ "error": "only GET is supported" }),
            405,
        ));
        return;
    }
    // Strip any query string; we only route on the path.
    let path = req.url().split('?').next().unwrap_or("/").to_string();
    let resp = route(&path, state, types, unserved, program);
    let _ = req.respond(resp);
}

fn route(
    path: &str,
    state: &Arc<Mutex<RelationState>>,
    types: &RelationTypes,
    unserved: &Unserved,
    program: &ProgramSource,
) -> Resp {
    let (status, body) = route_json(path, state, types, unserved, program);
    json_bytes_response(serde_json::to_vec(&body).unwrap_or_default(), status)
}

/// Pure routing logic: map a request path to `(status, body)`. Kept free of HTTP
/// types so it can be unit-tested directly.
fn route_json<'a>(
    path: &'a str,
    state: &Arc<Mutex<RelationState>>,
    types: &RelationTypes,
    unserved: &Unserved,
    program: &ProgramSource,
) -> (u16, Body<'a>) {
    // The loaded program — doesn't touch relation state.
    if path == "/program" {
        return (
            200,
            Body::Json(json!({ "path": program.path, "source": program.source })),
        );
    }

    let st = state.lock().unwrap();

    if path == "/" || path == "/relations" {
        let mut names: Vec<&String> = st.keys().collect();
        names.sort();
        let relations: Vec<_> = names
            .iter()
            .map(|n| json!({ "name": n, "count": st[*n].len() }))
            .collect();
        return (200, Body::Json(json!({ "relations": relations })));
    }

    if let Some(name) = path.strip_prefix("/relations/") {
        let name = name.trim_end_matches('/');
        return match st.get(name) {
            Some(rows) => {
                // Decode the raw `i64` rows to display text here — lazily, only for
                // the relation actually queried (rows churned during a seed are
                // never decoded). Empty/missing types render columns as integers.
                let col_types: &[_] = types.get(name).map(|v| v.as_slice()).unwrap_or(&[]);
                let mut out: Vec<Vec<String>> = rows
                    .keys()
                    .map(|r| decode_state_row(r, col_types))
                    .collect();
                out.sort();
                (
                    200,
                    Body::Rows(RelationRows {
                        name,
                        count: out.len(),
                        rows: out,
                    }),
                )
            }
            // Declared and computed, but not served (consumed by another rule and
            // not `.out`). Explain rather than say "unknown".
            None => match unserved.get(name) {
                Some(consumers) => (
                    404,
                    Body::Json(json!({
                        "error": format!(
                            "relation '{}' is computed but not served (consumed by {}); \
                             declare it under .out to expose it",
                            name,
                            consumers.join(", ")
                        )
                    })),
                ),
                None => (
                    404,
                    Body::Json(json!({ "error": format!("unknown relation '{}'", name) })),
                ),
            },
        };
    }

    (
        404,
        Body::Json(json!({ "error": format!("not found: {}", path) })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Rows are stored as raw encoded i64; with no column types the query API
    // renders each column as an integer, which is what these tests assert against.
    fn state_with(rel: &str, rows: &[&[i64]]) -> Arc<Mutex<RelationState>> {
        let mut st = RelationState::new();
        let map = st.entry(rel.to_string()).or_default();
        for r in rows {
            map.insert(r.iter().copied().collect(), 1);
        }
        Arc::new(Mutex::new(st))
    }

    fn no_types() -> Arc<RelationTypes> {
        Arc::new(RelationTypes::new())
    }

    // Serialize a routed body to a `Value` so assertions can index into it — the
    // same serialization the HTTP path uses.
    fn as_value(body: Body) -> serde_json::Value {
        serde_json::to_value(&body).unwrap()
    }

    fn unserved_with(pairs: &[(&str, &[&str])]) -> Unserved {
        Arc::new(
            pairs
                .iter()
                .map(|(n, cs)| (n.to_string(), cs.iter().map(|s| s.to_string()).collect()))
                .collect(),
        )
    }

    fn prog() -> ProgramSource {
        ProgramSource {
            path: "x.dl".to_string(),
            source: "reach(a, b) :- edge(a, b).".to_string(),
        }
    }

    #[test]
    fn served_relation_returns_rows() {
        let state = state_with("func", &[&[1, 2], &[3, 4]]);
        let unserved = unserved_with(&[]);
        let (status, body) = route_json("/relations/func", &state, &no_types(), &unserved, &prog());
        let body = as_value(body);
        assert_eq!(status, 200);
        assert_eq!(body["count"], 2);
        assert_eq!(body["rows"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn unserved_relation_explains_itself() {
        let state = state_with("func", &[&[1, 2]]);
        let unserved = unserved_with(&[("reach", &["tdep_count", "indirect_only"])]);
        let (status, body) =
            route_json("/relations/reach", &state, &no_types(), &unserved, &prog());
        let body = as_value(body);
        assert_eq!(status, 404);
        let err = body["error"].as_str().unwrap();
        assert!(err.contains("not served"), "got: {err}");
        assert!(err.contains("tdep_count, indirect_only"), "got: {err}");
        assert!(err.contains(".out"), "got: {err}");
    }

    #[test]
    fn truly_unknown_relation() {
        let state = state_with("func", &[&[1, 2]]);
        let unserved = unserved_with(&[("reach", &["x"])]);
        let (status, body) = route_json("/relations/nope", &state, &no_types(), &unserved, &prog());
        let body = as_value(body);
        assert_eq!(status, 404);
        assert_eq!(body["error"], "unknown relation 'nope'");
    }

    #[test]
    fn relations_listing_shows_served_only() {
        let state = state_with("func", &[&[1, 2]]);
        let unserved = unserved_with(&[("reach", &["x"])]);
        let (status, body) = route_json("/relations", &state, &no_types(), &unserved, &prog());
        let body = as_value(body);
        assert_eq!(status, 200);
        let names: Vec<&str> = body["relations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["func"]); // reach (unserved) is not listed
    }

    #[test]
    fn program_returns_source() {
        let state = state_with("func", &[&[1, 2]]);
        let unserved = unserved_with(&[]);
        let (status, body) = route_json("/program", &state, &no_types(), &unserved, &prog());
        let body = as_value(body);
        assert_eq!(status, 200);
        assert_eq!(body["path"], "x.dl");
        assert!(body["source"].as_str().unwrap().contains(":- edge(a, b)"));
    }
}
