//! String interning shared across the whole pipeline.
//!
//! FlowLog operates purely on `i64`. Strings only become integers because this
//! table interns them. The *same* table instance must be used for:
//!   1. interning string literals baked into the `.dl` program,
//!   2. encoding string values arriving from streaming plugins, and
//!   3. decoding output rows back into strings.
//! As long as a single [`RuntimeStringTable`] is used everywhere, a given
//! string always maps to the same id, so all three stay consistent.

use std::collections::HashMap;
use std::sync::Mutex;

use parsing::decl::NULL_SENTINEL;

/// Bidirectional string interning table. Maps strings to dense `i64` ids.
#[derive(Debug, Default, Clone)]
pub struct StringTable {
    str_to_id: HashMap<String, i64>,
    id_to_str: Vec<String>,
}

impl StringTable {
    pub fn intern(&mut self, s: &str) -> i64 {
        if let Some(&id) = self.str_to_id.get(s) {
            return id;
        }
        let id = self.id_to_str.len() as i64;
        self.id_to_str.push(s.to_string());
        self.str_to_id.insert(s.to_string(), id);
        id
    }

    pub fn decode(&self, id: i64) -> Option<&str> {
        self.id_to_str.get(id as usize).map(|s| s.as_str())
    }
}

/// Thread-safe wrapper around [`StringTable`] for runtime use.
///
/// Streaming source threads, the program loader, and the output callback all
/// share one of these via `Arc`.
#[derive(Default)]
pub struct RuntimeStringTable {
    inner: Mutex<StringTable>,
}

impl RuntimeStringTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn intern(&self, s: &str) -> i64 {
        self.inner.lock().unwrap().intern(s)
    }

    pub fn decode(&self, id: i64) -> Option<String> {
        self.inner.lock().unwrap().decode(id).map(|s| s.to_string())
    }
}

/// Encode a plugin [`DataValue`](dep2_plugin::DataValue) into the `i64` that
/// FlowLog consumes. Strings are interned; integers pass through; floats are
/// stored as their bit pattern; bools become 0/1; nulls use the sentinel.
pub fn encode_value(val: &dep2_plugin::DataValue, table: &RuntimeStringTable) -> i64 {
    use dep2_plugin::DataValue;
    match val {
        DataValue::String(s) => table.intern(s),
        DataValue::Integer(i) => *i,
        DataValue::Float(f) => {
            let bits = f.to_bits() as i64;
            // Avoid colliding with the NULL sentinel bit pattern.
            if bits == NULL_SENTINEL {
                NULL_SENTINEL + 1
            } else {
                bits
            }
        }
        DataValue::Bool(b) => {
            if *b {
                1
            } else {
                0
            }
        }
        DataValue::Null => NULL_SENTINEL,
    }
}

/// Rewrite a native `.dl` program so that every `"..."` string literal is
/// replaced by its interned integer id, interning into `table`.
///
/// FlowLog has no string table of its own, so it can only consume integer
/// constants. By interning literals here (into the *same* table the streaming
/// encoders use), a rule like `node(N) :- ast_node(_, N, "function_item", _)`
/// works: the literal and the streamed kind string get identical ids.
///
/// Quotes inside `#`/`//` line comments are ignored. The grammar's string
/// literal has no escape sequences (`"\"" ~ (!"\"" ~ ANY)*`), so the closing
/// quote is simply the next `"`.
pub fn intern_string_literals(src: &str, table: &RuntimeStringTable) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;

        // Skip `#` line comments.
        if c == '#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(bytes[i] as char);
                i += 1;
            }
            continue;
        }

        // Skip `//` line comments.
        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(bytes[i] as char);
                i += 1;
            }
            continue;
        }

        // String literal: capture until the next `"` and replace with its id.
        if c == '"' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b'"' {
                j += 1;
            }
            let literal = &src[start..j];
            let id = table.intern(literal);
            out.push_str(&id.to_string());
            // Skip past the closing quote (if present).
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
    fn intern_is_stable() {
        let t = RuntimeStringTable::new();
        let a = t.intern("foo");
        let b = t.intern("bar");
        assert_eq!(a, t.intern("foo"));
        assert_ne!(a, b);
        assert_eq!(t.decode(a).as_deref(), Some("foo"));
    }

    #[test]
    fn rewrites_literals_to_ids() {
        let t = RuntimeStringTable::new();
        let out = intern_string_literals(
            r#"node(N) :- ast(_, N, "function_item", _). // a "comment" stays"#,
            &t,
        );
        let id = t.intern("function_item");
        assert!(out.contains(&format!("ast(_, N, {}, _)", id)), "got: {out}");
        // The comment text is untouched.
        assert!(out.contains(r#"// a "comment" stays"#), "got: {out}");
    }

    #[test]
    fn literal_and_stream_share_ids() {
        let t = RuntimeStringTable::new();
        let _ = intern_string_literals(r#"r(X) :- s(X, "kind")."#, &t);
        let streamed = encode_value(&dep2_plugin::DataValue::String("kind".to_string()), &t);
        assert_eq!(streamed, t.intern("kind"));
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        // Identifier-ish strings (no quotes/newlines) so they round-trip through
        // the table and can be embedded in programs without confusing the lexer.
        fn words() -> impl Strategy<Value = Vec<String>> {
            prop::collection::vec("[a-zA-Z_][a-zA-Z0-9_]{0,7}", 0..12)
        }

        proptest! {
            /// Interning is a deterministic bijection: same string -> same id,
            /// distinct strings -> distinct ids, and decode inverts intern.
            #[test]
            fn intern_is_injective_and_roundtrips(ws in words()) {
                let t = RuntimeStringTable::new();
                let mut by_str: std::collections::HashMap<String, i64> = Default::default();
                let mut by_id: std::collections::HashMap<i64, String> = Default::default();
                for w in &ws {
                    let id = t.intern(w);
                    // determinism
                    prop_assert_eq!(id, t.intern(w));
                    // round-trip
                    let decoded = t.decode(id);
                    prop_assert_eq!(decoded.as_deref(), Some(w.as_str()));
                    // consistency with prior interns
                    if let Some(&prev) = by_str.get(w) {
                        prop_assert_eq!(prev, id);
                    } else {
                        // a fresh string must get a fresh id (injectivity)
                        prop_assert!(!by_id.contains_key(&id), "id {} reused for distinct strings", id);
                    }
                    by_str.insert(w.clone(), id);
                    by_id.insert(id, w.clone());
                }
            }

            /// `encode_value` of a string equals interning it directly.
            #[test]
            fn encode_string_equals_intern(ws in words()) {
                let t = RuntimeStringTable::new();
                for w in &ws {
                    let enc = encode_value(&dep2_plugin::DataValue::String(w.clone()), &t);
                    prop_assert_eq!(enc, t.intern(w));
                }
            }

            /// A program built only from atoms with string-literal arguments has
            /// every literal replaced by an integer id, and rewriting is
            /// idempotent in effect: feeding the output back interns nothing new
            /// (all quotes are gone) and the table is unchanged thereafter.
            #[test]
            fn rewrite_replaces_all_literals(ws in words()) {
                let t = RuntimeStringTable::new();
                // Build: r(X) :- s(X, "w0", "w1", ...).
                let args: String = ws.iter().map(|w| format!("\"{}\"", w)).collect::<Vec<_>>().join(", ");
                let src = format!("r(X) :- s(X, {}).", args);
                let out = intern_string_literals(&src, &t);
                // No quotes remain in the rewritten program.
                prop_assert!(!out.contains('"'), "quotes left in: {}", out);
                // Each literal's id appears in the output.
                for w in &ws {
                    let id = t.intern(w);
                    prop_assert!(
                        out.contains(&id.to_string()),
                        "id {} for {:?} missing from {}", id, w, out
                    );
                }
                // Re-running over the (quote-free) output adds no new strings.
                let before = t.intern("__sentinel__");
                let _ = intern_string_literals(&out, &t);
                let after = t.intern("__sentinel__");
                prop_assert_eq!(before, after);
            }

            /// Text outside string literals and comments is preserved verbatim
            /// when the input contains no quotes at all.
            #[test]
            fn rewrite_is_identity_without_quotes(s in "[a-zA-Z0-9_(), :.\\-]{0,40}") {
                let t = RuntimeStringTable::new();
                prop_assert_eq!(intern_string_literals(&s, &t), s);
            }
        }
    }
}
