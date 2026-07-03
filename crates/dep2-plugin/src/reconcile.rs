//! Snapshot-reconciliation helpers for streaming sources.
//!
//! A polling source (an HTTP API, a periodically re-read file) doesn't see
//! insert/delete events — it sees whole snapshots. To feed the engine's
//! diff-based input it must remember what it pushed last time and emit only the
//! difference. `DataValue` itself can't be a map key (floats), so snapshots are
//! held as [`CanonRow`]s — a hashable canonical form (floats by bit pattern) —
//! and [`reconcile`] pushes the multiset delta between two snapshots.

use std::collections::HashMap;
use std::sync::Arc;

use crate::{DataValue, ValueSink};

/// A hashable, comparable stand-in for one cell of a row. Floats are keyed by
/// IEEE-754 bit pattern, matching how the engine stores them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CanonValue {
    Str(Arc<str>),
    Int(i64),
    /// `f64::to_bits` of the value.
    Float(u64),
    Null,
}

impl CanonValue {
    pub fn str(s: impl AsRef<str>) -> Self {
        Self::Str(Arc::from(s.as_ref()))
    }

    pub fn float(f: f64) -> Self {
        Self::Float(f.to_bits())
    }

    pub fn to_data_value(&self) -> DataValue {
        match self {
            Self::Str(s) => DataValue::Str(Arc::clone(s)),
            Self::Int(i) => DataValue::Integer(*i),
            Self::Float(bits) => DataValue::Float(f64::from_bits(*bits)),
            Self::Null => DataValue::Null,
        }
    }
}

/// One row in canonical form.
pub type CanonRow = Vec<CanonValue>;

/// A relation snapshot: row -> multiplicity.
pub type Multiset = HashMap<CanonRow, usize>;

/// Build a [`Multiset`] from rows (duplicates counted).
pub fn multiset(rows: impl IntoIterator<Item = CanonRow>) -> Multiset {
    let mut ms = Multiset::new();
    for row in rows {
        *ms.entry(row).or_insert(0) += 1;
    }
    ms
}

/// Push the multiset delta `old -> new` for `relation` into `sink`:
/// rows that lost multiplicity are retracted, rows that gained are inserted.
/// Idempotent by construction: reconciling equal snapshots pushes nothing.
pub fn reconcile(sink: &mut dyn ValueSink, relation: &str, old: &Multiset, new: &Multiset) {
    for (row, &old_count) in old {
        let new_count = new.get(row).copied().unwrap_or(0);
        if old_count > new_count {
            let values: Vec<DataValue> = row.iter().map(CanonValue::to_data_value).collect();
            for _ in 0..(old_count - new_count) {
                sink.push(relation, &values, -1);
            }
        }
    }
    for (row, &new_count) in new {
        let old_count = old.get(row).copied().unwrap_or(0);
        if new_count > old_count {
            let values: Vec<DataValue> = row.iter().map(CanonValue::to_data_value).collect();
            for _ in 0..(new_count - old_count) {
                sink.push(relation, &values, 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Recorder(Vec<(String, Vec<DataValue>, isize)>);

    impl ValueSink for Recorder {
        fn push(&mut self, relation: &str, row: &[DataValue], diff: isize) {
            self.0.push((relation.to_string(), row.to_vec(), diff));
        }
    }

    fn row(id: i64, price: f64) -> CanonRow {
        vec![CanonValue::Int(id), CanonValue::float(price)]
    }

    #[test]
    fn equal_snapshots_push_nothing() {
        let ms = multiset([row(1, 0.5), row(2, 0.25)]);
        let mut sink = Recorder(Vec::new());
        reconcile(&mut sink, "r", &ms, &ms.clone());
        assert!(sink.0.is_empty());
    }

    #[test]
    fn changed_row_retracts_then_inserts() {
        let old = multiset([row(1, 0.5)]);
        let new = multiset([row(1, 0.75)]);
        let mut sink = Recorder(Vec::new());
        reconcile(&mut sink, "r", &old, &new);
        let mut diffs: Vec<isize> = sink.0.iter().map(|(_, _, d)| *d).collect();
        diffs.sort();
        assert_eq!(diffs, vec![-1, 1]);
    }

    #[test]
    fn multiplicity_delta_is_partial() {
        let old = multiset([row(1, 0.5), row(1, 0.5), row(2, 0.1)]);
        let new = multiset([row(1, 0.5), row(2, 0.1), row(2, 0.1)]);
        let mut sink = Recorder(Vec::new());
        reconcile(&mut sink, "r", &old, &new);
        assert_eq!(sink.0.len(), 2); // one -1 for row 1, one +1 for row 2
    }

    #[test]
    fn float_bits_round_trip() {
        let v = CanonValue::float(0.1 + 0.2);
        assert_eq!(v.to_data_value(), DataValue::Float(0.1 + 0.2));
    }
}
