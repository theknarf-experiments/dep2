//! Read-only HTTP/JSON query API over the engine's live state.
//!
//! Routes:
//!   GET /                      same as /relations
//!   GET /relations             -> { "relations": [ { "name", "count" }, ... ] }
//!   GET /relations/<name>      -> { "name", "rows": [ [col, ...], ... ] }

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dep2_core::engine::RelationState;
use serde_json::json;
use tiny_http::{Header, Method, Request, Response, Server};

type Resp = Response<Cursor<Vec<u8>>>;

/// Serve the query API on `addr` until `shutdown` is set. Blocks the caller, so
/// run it on its own thread.
pub fn serve(
    addr: &str,
    state: Arc<Mutex<RelationState>>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), String> {
    let server = Server::http(addr).map_err(|e| e.to_string())?;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match server.recv_timeout(Duration::from_millis(200)) {
            Ok(Some(req)) => handle(req, &state),
            Ok(None) => continue, // timed out; re-check shutdown
            Err(_) => break,
        }
    }
    Ok(())
}

fn json_response(value: serde_json::Value, status: u16) -> Resp {
    let body = serde_json::to_vec(&value).unwrap_or_default();
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    Response::from_data(body)
        .with_status_code(status)
        .with_header(header)
}

fn handle(req: Request, state: &Arc<Mutex<RelationState>>) {
    if *req.method() != Method::Get {
        let _ = req.respond(json_response(
            json!({ "error": "only GET is supported" }),
            405,
        ));
        return;
    }
    // Strip any query string; we only route on the path.
    let path = req.url().split('?').next().unwrap_or("/").to_string();
    let resp = route(&path, state);
    let _ = req.respond(resp);
}

fn route(path: &str, state: &Arc<Mutex<RelationState>>) -> Resp {
    let st = state.lock().unwrap();

    if path == "/" || path == "/relations" {
        let mut names: Vec<&String> = st.keys().collect();
        names.sort();
        let relations: Vec<_> = names
            .iter()
            .map(|n| json!({ "name": n, "count": st[*n].len() }))
            .collect();
        return json_response(json!({ "relations": relations }), 200);
    }

    if let Some(name) = path.strip_prefix("/relations/") {
        let name = name.trim_end_matches('/');
        return match st.get(name) {
            Some(rows) => {
                let mut out: Vec<Vec<String>> = rows.keys().cloned().collect();
                out.sort();
                json_response(
                    json!({ "name": name, "count": out.len(), "rows": out }),
                    200,
                )
            }
            None => json_response(
                json!({ "error": format!("unknown relation '{}'", name) }),
                404,
            ),
        };
    }

    json_response(json!({ "error": format!("not found: {}", path) }), 404)
}
