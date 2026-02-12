use arrayvec::ArrayVec;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::fmt;
use std::fmt::Debug;
use std::hash::Hash;

/* ------------------------------------------------------------------------------------ */
/* Array */
/* ------------------------------------------------------------------------------------ */

///
/// a trait to abstract ops over array implementations
pub trait Array: Debug + Send + Sync {
    /// insert a value
    fn push(&mut self, v: i64);
    /// return the number of columns
    fn arity(&self) -> usize;
    /// return the value of a column
    fn column(&self, id: usize) -> i64;
}

/// stack-allocated row for small arities using const generics
#[derive(Debug, Clone, Hash, PartialOrd, Ord, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row<const N: usize> {
    values: ArrayVec<i64, N>,
}

impl<const N: usize> Row<N> {
    pub fn new() -> Self {
        Self {
            values: ArrayVec::new(),
        }
    }

    // pub fn extend(&mut self, slice: &[i32]) {
    //     self.values.extend(slice.iter().cloned());
    // }
}

impl<const N: usize> Default for Row<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Array for Row<N> {
    fn push(&mut self, v: i64) {
        self.values.push(v);
    }

    fn arity(&self) -> usize {
        self.values.len()
    }

    fn column(&self, id: usize) -> i64 {
        unsafe { *self.values.get_unchecked(id) }
    }
}

impl<const N: usize> fmt::Display for Row<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            self.values
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// heap-allocated row for large arities using SmallVec as fallback
#[derive(Debug, Clone, Hash, PartialOrd, Ord, PartialEq, Eq, Serialize, Deserialize)]
pub struct FatRow {
    values: SmallVec<[i64; crate::FALLBACK_ARITY]>,
}

impl FatRow {
    pub fn new() -> Self {
        Self {
            values: SmallVec::new(),
        }
    }
}

impl Default for FatRow {
    fn default() -> Self {
        Self::new()
    }
}

impl Array for FatRow {
    fn push(&mut self, v: i64) {
        self.values.push(v);
    }

    fn arity(&self) -> usize {
        self.values.len()
    }

    fn column(&self, id: usize) -> i64 {
        unsafe { *self.values.get_unchecked(id) }
    }
}

impl fmt::Display for FatRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            self.values
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;
    use proptest::collection::vec;

    proptest! {
        #[test]
        fn row_push_column_roundtrip(values in vec(any::<i64>(), 1..=8usize)) {
            let mut row = Row::<8>::new();
            for &v in &values {
                row.push(v);
            }
            for (i, &v) in values.iter().enumerate() {
                prop_assert_eq!(row.column(i), v);
            }
        }

        #[test]
        fn fatrow_push_column_roundtrip(values in vec(any::<i64>(), 1..=20usize)) {
            let mut row = FatRow::new();
            for &v in &values {
                row.push(v);
            }
            for (i, &v) in values.iter().enumerate() {
                prop_assert_eq!(row.column(i), v);
            }
        }

        #[test]
        fn row_arity_tracks_pushes(values in vec(any::<i64>(), 1..=8usize)) {
            let mut row = Row::<8>::new();
            for (i, &v) in values.iter().enumerate() {
                prop_assert_eq!(row.arity(), i);
                row.push(v);
            }
            prop_assert_eq!(row.arity(), values.len());
        }

        #[test]
        fn fatrow_arity_tracks_pushes(values in vec(any::<i64>(), 1..=20usize)) {
            let mut row = FatRow::new();
            for (i, &v) in values.iter().enumerate() {
                prop_assert_eq!(row.arity(), i);
                row.push(v);
            }
            prop_assert_eq!(row.arity(), values.len());
        }

        #[test]
        fn row_fatrow_equivalence(values in vec(any::<i64>(), 1..=8usize)) {
            let mut row = Row::<8>::new();
            let mut fat = FatRow::new();
            for &v in &values {
                row.push(v);
                fat.push(v);
            }
            prop_assert_eq!(row.arity(), fat.arity());
            for i in 0..values.len() {
                prop_assert_eq!(row.column(i), fat.column(i));
            }
        }

        #[test]
        fn row_display_comma_separated(values in vec(any::<i64>(), 1..=8usize)) {
            let mut row = Row::<8>::new();
            for &v in &values {
                row.push(v);
            }
            let display = format!("{}", row);
            for v in &values {
                prop_assert!(display.contains(&v.to_string()));
            }
        }

        #[test]
        fn row_column_values_independent(a in any::<i64>(), b in any::<i64>(), c in any::<i64>()) {
            let mut row = Row::<8>::new();
            row.push(a);
            row.push(b);
            row.push(c);
            prop_assert_eq!(row.column(0), a);
            prop_assert_eq!(row.column(1), b);
            prop_assert_eq!(row.column(2), c);
        }
    }

    #[test]
    fn row_new_arity_zero() {
        let row = Row::<8>::new();
        assert_eq!(row.arity(), 0);
        let fat = FatRow::new();
        assert_eq!(fat.arity(), 0);
    }
}
