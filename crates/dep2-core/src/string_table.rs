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
}
