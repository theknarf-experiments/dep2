//! Engine-owned string interner and value codec.
//!
//! FlowLog operates on `i64` columns internally for speed. This module is what
//! makes `string` and `float` first-class column types *inside the engine*:
//! every non-integer value is encoded to an `i64` here (strings are interned to
//! dense ids; floats are stored as their IEEE-754 bit pattern) and decoded back
//! through the same table on output.
//!
//! The interner is a process-global table: a string always maps to the same id
//! across fact loading, rule constants, streaming input, and output decoding, so
//! all paths agree. (One engine per process; interning is monotonic, so decoding
//! is always correct regardless of which path interned a given string first.)

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use parsing::decl::{is_null, DataType, NULL_SENTINEL};

#[derive(Default)]
struct Table {
    str_to_id: HashMap<String, i64>,
    id_to_str: Vec<String>,
}

fn table() -> &'static Mutex<Table> {
    static TABLE: OnceLock<Mutex<Table>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(Table::default()))
}

/// Intern a string, returning its stable `i64` id.
pub fn intern(s: &str) -> i64 {
    let mut t = table().lock().unwrap();
    if let Some(&id) = t.str_to_id.get(s) {
        return id;
    }
    let id = t.id_to_str.len() as i64;
    t.id_to_str.push(s.to_string());
    t.str_to_id.insert(s.to_string(), id);
    id
}

/// Decode an interned id back to its string, if known.
pub fn decode(id: i64) -> Option<String> {
    let t = table().lock().unwrap();
    t.id_to_str.get(id as usize).cloned()
}

/// Encode a float into the `i64` the engine stores (its bit pattern), nudging
/// off the NULL sentinel bit pattern if it collides.
pub fn float_to_i64(f: f64) -> i64 {
    let bits = f.to_bits() as i64;
    if bits == NULL_SENTINEL {
        NULL_SENTINEL + 1
    } else {
        bits
    }
}

/// Encode a raw text token (from a `.facts` file) into the engine's `i64`,
/// according to the column's declared type. Returns `None` if a numeric token
/// fails to parse (the caller drops the row, preserving prior reader behavior).
/// An empty token is treated as NULL.
pub fn encode_token(tok: &str, dt: DataType) -> Option<i64> {
    if tok.is_empty() {
        return Some(NULL_SENTINEL);
    }
    match dt {
        DataType::String => Some(intern(tok)),
        DataType::Integer => tok.parse::<i64>().ok(),
        DataType::Float => tok.parse::<f64>().ok().map(float_to_i64),
    }
}

/// Decode a stored `i64` to its display string, according to the column type.
pub fn decode_value(v: i64, dt: DataType) -> String {
    if is_null(v) {
        return "NULL".to_string();
    }
    match dt {
        DataType::String => decode(v).unwrap_or_else(|| v.to_string()),
        DataType::Float => format!("{}", f64::from_bits(v as u64)),
        DataType::Integer => v.to_string(),
    }
}

/// Decode a `", "`-joined row of stored `i64` values using per-column types.
/// Columns beyond `types` (or when `types` is empty) are passed through as-is.
pub fn decode_row(row: &str, types: &[DataType]) -> Vec<String> {
    row.split(", ")
        .enumerate()
        .map(|(i, cell)| match types.get(i) {
            Some(&dt) => match cell.parse::<i64>() {
                Ok(v) => decode_value(v, dt),
                Err(_) => cell.to_string(),
            },
            None => cell.to_string(),
        })
        .collect()
}

/// Decode a row already split into cells (raw stringified `i64`) using per-column
/// types. Cells beyond `types` are passed through unchanged.
pub fn decode_cells(cells: &[String], types: &[DataType]) -> Vec<String> {
    cells
        .iter()
        .enumerate()
        .map(|(i, cell)| match types.get(i) {
            Some(&dt) => match cell.parse::<i64>() {
                Ok(v) => decode_value(v, dt),
                Err(_) => cell.clone(),
            },
            None => cell.clone(),
        })
        .collect()
}

/// Rewrite a `.dl` program so every `"..."` string literal becomes its interned
/// integer id. FlowLog's executor consumes constants as `i64`, so literals must
/// be encoded before parsing; doing it here (against the same global table the
/// reader and streaming inputs use) is what lets a rule like
/// `r(N) :- ast(_, N, "function_item")` match interned `string` facts.
///
/// Quotes inside `#`/`//` line comments are ignored. The grammar's string has no
/// escapes, so the closing quote is simply the next `"`.
pub fn encode_literals(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;

        // Pass through `#` and `//` line comments verbatim.
        if c == '#' || (c == '/' && i + 1 < bytes.len() && bytes[i + 1] == b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(bytes[i] as char);
                i += 1;
            }
            continue;
        }

        // String literal -> interned id.
        if c == '"' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b'"' {
                j += 1;
            }
            let id = intern(&src[start..j]);
            out.push_str(&id.to_string());
            i = if j < bytes.len() { j + 1 } else { j };
            continue;
        }

        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_roundtrips_and_is_stable() {
        let a = intern("alpha");
        assert_eq!(a, intern("alpha"));
        assert_ne!(a, intern("beta"));
        assert_eq!(decode(a).as_deref(), Some("alpha"));
    }

    #[test]
    fn encode_decode_by_type() {
        // string round-trips through the table
        let id = encode_token("kind", DataType::String).unwrap();
        assert_eq!(decode_value(id, DataType::String), "kind");
        // integer passes through
        assert_eq!(encode_token("42", DataType::Integer), Some(42));
        assert_eq!(decode_value(42, DataType::Integer), "42");
        // float round-trips via bits
        let f = encode_token("3.5", DataType::Float).unwrap();
        assert_eq!(decode_value(f, DataType::Float), "3.5");
        // bad numeric token -> None (row dropped)
        assert_eq!(encode_token("xyz", DataType::Integer), None);
    }

    #[test]
    fn encode_literals_replaces_strings_only() {
        let out = encode_literals(r#"r(X) :- s(X, "kind"). // "comment""#);
        let id = intern("kind");
        assert!(out.contains(&format!("s(X, {})", id)), "got: {out}");
        assert!(out.contains(r#"// "comment""#), "comment untouched: {out}");
        assert!(
            !out.split("//").next().unwrap().contains('"'),
            "code quotes gone"
        );
    }

    #[test]
    fn decode_row_mixed_types() {
        let sid = intern("foo");
        let row = format!("{}, 7", sid);
        let decoded = decode_row(&row, &[DataType::String, DataType::Integer]);
        assert_eq!(decoded, vec!["foo".to_string(), "7".to_string()]);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        fn words() -> impl Strategy<Value = Vec<String>> {
            prop::collection::vec("[a-zA-Z_][a-zA-Z0-9_]{0,7}", 0..12)
        }

        proptest! {
            /// Interning is a deterministic injection that round-trips through decode.
            #[test]
            fn intern_injective_and_roundtrips(ws in words()) {
                let mut seen_id: std::collections::HashMap<i64, String> = Default::default();
                for w in &ws {
                    let id = intern(w);
                    prop_assert_eq!(id, intern(w)); // deterministic
                    let decoded = decode(id);
                    prop_assert_eq!(decoded.as_deref(), Some(w.as_str())); // round-trip
                    // distinct strings never share an id
                    if let Some(prev) = seen_id.get(&id) {
                        prop_assert_eq!(prev, w);
                    }
                    seen_id.insert(id, w.clone());
                }
            }

            /// String `encode_token` equals interning the raw text.
            #[test]
            fn encode_token_string_equals_intern(ws in words()) {
                for w in &ws {
                    prop_assert_eq!(encode_token(w, DataType::String), Some(intern(w)));
                }
            }

            /// `encode_literals` replaces every string literal with its id, leaves
            /// no quotes in code, and is effectively idempotent (re-running adds
            /// nothing because the output is quote-free).
            #[test]
            fn encode_literals_replaces_all(ws in words()) {
                let args: String = ws.iter().map(|w| format!("\"{}\"", w)).collect::<Vec<_>>().join(", ");
                let src = format!("r(X) :- s(X, {}).", args);
                let out = encode_literals(&src);
                prop_assert!(!out.contains('"'), "quotes left: {}", out);
                for w in &ws {
                    prop_assert!(out.contains(&intern(w).to_string()));
                }
                let before = intern("__sentinel__");
                let _ = encode_literals(&out);
                prop_assert_eq!(before, intern("__sentinel__"));
            }

            /// Quote-free input is passed through unchanged.
            #[test]
            fn encode_literals_identity_without_quotes(s in "[a-zA-Z0-9_(), :.+\\-]{0,40}") {
                prop_assert_eq!(encode_literals(&s), s);
            }

            /// Float encode/decode round-trips through the bit codec.
            #[test]
            fn float_roundtrips(x in -1e6f64..1e6f64) {
                let enc = encode_token(&x.to_string(), DataType::Float).unwrap();
                let dec = decode_value(enc, DataType::Float);
                prop_assert_eq!(dec, x.to_string());
            }
        }
    }
}
