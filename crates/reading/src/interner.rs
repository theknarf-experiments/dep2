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

use std::hash::Hasher;
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use parsing::decl::{is_null, DataType, NULL_SENTINEL};
use rustc_hash::{FxHashMap, FxHasher};

/// Number of interner shards. The interner is sharded so the many timely workers
/// can intern concurrently without serializing on one lock (a single global
/// `Mutex` made the per-row interning a lock convoy that erased multi-worker
/// parallelism). A power of two; 64 keeps per-worker contention low.
const SHARDS: usize = 64;

#[derive(Default)]
struct Shard {
    // FxHash, not the default SipHash: `intern` is on the dataflow hot path
    // (every string-builtin result — concat/before_last/replace/… — is
    // re-interned), and SipHash of the key string dominated the profile.
    //
    // The key is the SAME `Arc<str>` stored in `id_to_str` (not a separate
    // `String`), so each interned string's bytes are stored once, not twice.
    // `Arc<str>: Borrow<str>` lets `get(&str)` look up without allocating.
    str_to_id: FxHashMap<Arc<str>, i64>,
    // `Arc<str>`, not `String`: `decode` is on the dataflow hot path (string
    // builtins decode every operand) and is called far more than `intern`.
    // Returning an `Arc<str>` makes each decode a refcount bump instead of a
    // fresh heap allocation + copy of the whole string.
    id_to_str: Vec<Arc<str>>,
}

fn shards() -> &'static [Mutex<Shard>; SHARDS] {
    static SHARDS_ARR: OnceLock<[Mutex<Shard>; SHARDS]> = OnceLock::new();
    SHARDS_ARR.get_or_init(|| std::array::from_fn(|_| Mutex::new(Shard::default())))
}

/// Which shard a string lives in (deterministic, seed-free, so every thread and
/// every path agrees).
#[inline]
fn shard_of(s: &str) -> usize {
    let mut h = FxHasher::default();
    h.write(s.as_bytes());
    (h.finish() as usize) % SHARDS
}

/// Intern a string, returning its stable `i64` id.
///
/// Ids are *strided* across shards: `id = local_index * SHARDS + shard`. This
/// keeps a string's id stable and globally unique (a given string always hashes
/// to the same shard and keeps the same local slot) while letting each shard
/// allocate ids independently under its own lock. `decode` reverses the stride.
pub fn intern(s: &str) -> i64 {
    let si = shard_of(s);
    let mut shard = shards()[si].lock();
    if let Some(&id) = shard.str_to_id.get(s) {
        return id;
    }
    let id = (shard.id_to_str.len() * SHARDS + si) as i64;
    // One allocation, shared between the id->str Vec and the str->id map key, so
    // the string's bytes are stored once.
    let arc: Arc<str> = Arc::from(s);
    shard.id_to_str.push(Arc::clone(&arc));
    shard.str_to_id.insert(arc, id);
    id
}

/// Decode an interned id back to its string, if known. Returns a cheaply-cloned
/// `Arc<str>` (a refcount bump, not a fresh allocation).
pub fn decode(id: i64) -> Option<Arc<str>> {
    if id < 0 {
        return None;
    }
    let id = id as usize;
    let si = id % SHARDS;
    let local = id / SHARDS;
    shards()[si].lock().id_to_str.get(local).cloned()
}

/// Encode a float into the `i64` the engine stores (its bit pattern).
///
/// Thin alias for [`parsing::decl::encode_float`], which is the single place
/// the `-0.0`/NULL collision is resolved. It used to nudge to
/// `NULL_SENTINEL + 1` instead — the smallest subnormal, a *different* number
/// from the one being stored, and one that no longer compared equal to `+0.0`
/// the way IEEE says it should.
pub fn float_to_i64(f: f64) -> i64 {
    parsing::decl::encode_float(f)
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
        // `i64::MIN` is the NULL sentinel, so it is not a representable number.
        // It parses to the sentinel itself and therefore loads as NULL with no
        // mapping needed — but that it does so is the intended contract, not
        // luck. NULL is the same answer arithmetic gives when a result leaves
        // the domain, and it keeps the rest of the row, which dropping the row
        // over one unrepresentable cell would not.
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
        DataType::String => decode(v).map_or_else(|| v.to_string(), |s| s.to_string()),
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

/// Decode a row of stored `i64` values directly (no string round-trip) using
/// per-column types. Columns beyond `types` fall back to the integer rendering.
/// This is the output path's decode: the dataflow hands raw `i64` rows straight
/// here, so there is no stringify-then-reparse.
pub fn decode_cells_i64(cells: &[i64], types: &[DataType]) -> Vec<String> {
    cells
        .iter()
        .enumerate()
        .map(|(i, &v)| match types.get(i) {
            Some(&dt) => decode_value(v, dt),
            None => v.to_string(),
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
/// Quotes inside `#`/`//` line comments are ignored. String literals support
/// backslash escapes (`\"`, `\\`, `\n`, `\t`, `\r`); an unknown escape `\x`
/// yields the literal `x`. So `"\""` denotes a single `"` character.
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
            let (decoded, next) = decode_literal_body(bytes, i + 1);
            out.push_str(&intern(&decoded).to_string());
            i = next;
            continue;
        }

        out.push(c);
        i += 1;
    }
    out
}

/// Decode a string literal's body starting just after the opening quote:
/// escapes are decoded byte-wise (`\n`/`\t`/`\r` and `\x -> x`); other bytes
/// pass through so multi-byte UTF-8 is preserved. Returns the decoded text and
/// the index just past the closing quote.
fn decode_literal_body(bytes: &[u8], start: usize) -> (String, usize) {
    let mut j = start;
    let mut buf: Vec<u8> = Vec::new();
    while j < bytes.len() {
        let b = bytes[j];
        if b == b'\\' && j + 1 < bytes.len() {
            buf.push(match bytes[j + 1] {
                b'n' => b'\n',
                b't' => b'\t',
                b'r' => b'\r',
                other => other, // \", \\, and unknown \x -> x
            });
            j += 2;
            continue;
        }
        if b == b'"' {
            break;
        }
        buf.push(b);
        j += 1;
    }
    let next = if j < bytes.len() { j + 1 } else { j };
    (String::from_utf8_lossy(&buf).into_owned(), next)
}

/// Intern one quoted string token (`"..."`, quotes included — the form the
/// parser stores in `Const::Text`), decoding escapes exactly like
/// [`encode_literals`]. The AST-level counterpart of the textual rewrite.
pub fn intern_literal(quoted: &str) -> i64 {
    let bytes = quoted.as_bytes();
    let start = usize::from(bytes.first() == Some(&b'"'));
    let (decoded, _) = decode_literal_body(bytes, start);
    intern(&decoded)
}

#[cfg(test)]
mod tests {

    /// Ingestion is the other place a value can collide with the sentinel:
    /// a `.facts`/CSV cell is arbitrary text.
    #[test]
    fn negative_zero_loads_as_positive_zero() {
        let v = encode_token("-0.0", DataType::Float).unwrap();
        assert!(!is_null(v), "a -0.0 cell loaded as NULL");
        assert_eq!(f64::from_bits(v as u64), 0.0);
        // It must also share an encoding with the cell that says "0.0", or the
        // two would fail to join despite being the same number.
        assert_eq!(v, encode_token("0.0", DataType::Float).unwrap());
    }

    /// The integer hole, at the loading boundary: `i64::MIN` is not a number the
    /// engine can hold, so a cell containing it reads as NULL. The row is kept —
    /// discarding its other columns over one unrepresentable cell would be worse.
    #[test]
    fn the_reserved_integer_loads_as_null() {
        let v = encode_token("-9223372036854775808", DataType::Integer);
        assert_eq!(v, Some(NULL_SENTINEL));
        // One away, and at the other end, are ordinary numbers.
        assert_eq!(
            encode_token("-9223372036854775807", DataType::Integer),
            Some(i64::MIN + 1)
        );
        assert_eq!(
            encode_token("9223372036854775807", DataType::Integer),
            Some(i64::MAX)
        );
        // Out of range is still a parse failure (row dropped), not a NULL.
        assert_eq!(encode_token("9223372036854775808", DataType::Integer), None);
    }

    use super::*;

    #[test]
    fn intern_literal_matches_encode_literals() {
        for lit in [
            r#""hello""#,
            r#""with \"quotes\" and \\slash""#,
            r#""tabs\tand\nnewlines""#,
            r#""multi-byte æøå ☃""#,
            r#""""#,
        ] {
            let via_rewrite: i64 = encode_literals(lit).trim().parse().unwrap();
            assert_eq!(via_rewrite, intern_literal(lit), "literal {lit}");
        }
    }

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
    fn encode_literals_decodes_escapes() {
        // "\"" is a single double-quote; "a\\b" is a\b; "x\ty" has a tab.
        let out = encode_literals(r#"r(X) :- s(X, "\""), t(X, "a\\b"), u(X, "x\ty")."#);
        let quote = intern("\"");
        let backslash = intern("a\\b");
        let tab = intern("x\ty");
        assert!(
            out.contains(&format!("s(X, {})", quote)),
            "escaped quote: {out}"
        );
        assert!(
            out.contains(&format!("t(X, {})", backslash)),
            "escaped backslash: {out}"
        );
        assert!(out.contains(&format!("u(X, {})", tab)), "tab escape: {out}");
        // The escaped quote must not have terminated the literal early.
        assert!(!out.contains('"'), "no raw quotes remain: {out}");
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
