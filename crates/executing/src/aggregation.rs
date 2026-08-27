use differential_dataflow::difference::Abelian;
use parsing::aggregation::{Aggregation, AggregationOperator};
use parsing::decl::{is_null, DataType, NULL_SENTINEL};
use reading::row::{Array, FatRow, Row};
use reading::{diff_to_i32, semiring_one, Semiring};

/// Aggregates a collection of integer values using the specified aggregation operator.
///
/// # Arguments
/// * `input` - Vector of integers to aggregate
/// * `op` - The aggregation operation to perform
///
/// # Returns
/// * `Some(result)` - The aggregation result, or the count for Count operations
/// * `None` - If the input is empty and the operation cannot produce a meaningful result
fn aggregate_ints(input: &[i64], op: &AggregationOperator) -> Option<i64> {
    match op {
        AggregationOperator::Count => Some(input.len() as i64),
        AggregationOperator::Sum => {
            // Widened to i128 so the addition itself cannot overflow, but a
            // total that does not fit back into i64 still has no answer. `as
            // i64` would truncate it into a plausible-looking wrong number,
            // which is the failure mode worth avoiding: a silently wrong sum
            // is harder to notice than a missing one.
            let wide: i128 = input.iter().map(|&x| x as i128).sum();
            Some(i64::try_from(wide).unwrap_or(NULL_SENTINEL))
        }
        AggregationOperator::Min => input.iter().min().copied(),
        AggregationOperator::Max => input.iter().max().copied(),
        AggregationOperator::Avg => {
            if input.is_empty() {
                return None;
            }
            let sum: i128 = input.iter().map(|&x| x as i128).sum();
            // Truncating division (toward zero), like Rust integer division.
            Some((sum / input.len() as i128) as i64)
        }
    }
}

/// Type-aware aggregation: filters out NULL_SENTINEL values, then dispatches
/// to integer or float aggregation.
fn aggregate_values(input: &[i64], op: &AggregationOperator, dt: &DataType) -> Option<i64> {
    let filtered: Vec<i64> = input.iter().copied().filter(|v| !is_null(*v)).collect();
    match dt {
        // `min`/`max` over a string column compare the DECODED TEXT, not the
        // stored id. Ids are handed out by the interner in arrival order, so
        // they differ between runs and across the parse pool's threads —
        // ordering by id would make the result nondeterministic. Text order is
        // stable, which is what lets a string column serve as a `merge(min)`
        // representative. `sum`/`avg` stay meaningless here and fall through to
        // the integer path (typing rejects them on string columns).
        DataType::String if matches!(op, AggregationOperator::Min | AggregationOperator::Max) => {
            let want_min = matches!(op, AggregationOperator::Min);
            filtered
                .iter()
                .copied()
                .map(|v| (reading::decode(v), v))
                .reduce(|best, cand| {
                    let better = match (&cand.0, &best.0) {
                        (Some(c), Some(b)) => {
                            if want_min {
                                c.as_ref() < b.as_ref()
                            } else {
                                c.as_ref() > b.as_ref()
                            }
                        }
                        // An id with no text behind it cannot be ordered by
                        // content; fall back to the id so the fold stays total.
                        _ => {
                            if want_min {
                                cand.1 < best.1
                            } else {
                                cand.1 > best.1
                            }
                        }
                    };
                    if better {
                        cand
                    } else {
                        best
                    }
                })
                .map(|(_, v)| v)
        }
        DataType::Float => {
            if matches!(op, AggregationOperator::Count) {
                return Some(filtered.len() as i64);
            }
            if filtered.is_empty() {
                return None;
            }
            let floats: Vec<f64> = filtered.iter().map(|v| f64::from_bits(*v as u64)).collect();
            let result = match op {
                AggregationOperator::Sum => floats.iter().sum::<f64>(),
                AggregationOperator::Avg => floats.iter().sum::<f64>() / filtered.len() as f64,
                AggregationOperator::Min => floats.iter().copied().fold(f64::INFINITY, f64::min),
                AggregationOperator::Max => {
                    floats.iter().copied().fold(f64::NEG_INFINITY, f64::max)
                }
                AggregationOperator::Count => unreachable!(),
            };
            // Encode, do not `to_bits`: the fold can land on `-0.0`, whose bit
            // pattern is the NULL sentinel, so the group's aggregate would come
            // back as no value at all. `min`/`max` cannot reach it (they return
            // an input, and no input can be `-0.0` any more), but `avg` can by
            // UNDERFLOW — a tiny negative sum divided by the group size rounds
            // below the smallest subnormal and lands on negative zero.
            Some(parsing::decl::encode_float(result))
        }
        _ => {
            if matches!(op, AggregationOperator::Count) {
                return Some(filtered.len() as i64);
            }
            if filtered.is_empty() {
                return None;
            }
            aggregate_ints(&filtered, op)
        }
    }
}

/// Expand a reduce input into one value per body match. The plan projects the
/// body onto (group key, value), so equal contributions arrive folded into one
/// record with multiplicity > 1 (three body matches with the same value = one
/// row, diff 3). Expand by the difference or `sum`/`count` under-count whenever
/// contributions collide on a value.
fn expand_values(input: &[(&Row<1>, Semiring)]) -> Vec<i64> {
    input
        .iter()
        .flat_map(|(row, diff)| {
            let mult = diff_to_i32(diff).max(0) as usize;
            std::iter::repeat(row.column(0)).take(mult)
        })
        .collect()
}

/// Creates the reduction logic for differential dataflow aggregation operations.
///
/// This function returns a closure that can be used with differential dataflow's
/// reduce operator to perform aggregations on grouped data.
///
/// # Type Parameters
/// * `N_GB` - Number of columns in the group-by key
/// * `N_TOT` - Total number of columns in the relation
///
/// # Arguments
/// * `aggregation` - The aggregation specification containing the operator
///
/// # Returns
/// A closure that implements the aggregation logic for differential dataflow
pub fn aggregation_reduce_logic<const N_GB: usize>(
    aggregation: &Aggregation,
) -> impl FnMut(
    &Row<N_GB>,
    &[(&Row<1>, Semiring)],
    &mut Vec<(Row<1>, Semiring)>,
    &mut Vec<(Row<1>, Semiring)>,
) {
    let operator = *aggregation.operator();
    let data_type = *aggregation.data_type();

    // `reduce_core` contract: `output` holds the previously-produced output for
    // this key, `updates` is where we push the deltas to emit. To replace (and,
    // when the input empties, retract) the aggregate we emit the new value and
    // subtract the previous output — otherwise stale aggregates linger after a
    // contributing fact is deleted. `reduce_core` invokes us even on empty input
    // when prior output exists, so this is also the retraction path.
    move |_key, input, output, updates| {
        if !input.is_empty() {
            let values = expand_values(input);
            if let Some(result) = aggregate_values(&values, &operator, &data_type) {
                let mut out = Row::<1>::new();
                out.push(result);
                updates.push((out, semiring_one()));
            }
        }
        for (row, diff) in output.drain(..) {
            let mut neg = diff;
            neg.negate();
            updates.push((row, neg));
        }
    }
}

/// Creates a mapping function to merge key-value pairs back into a relation after aggregation.
///
/// This function reconstructs the full relation by combining the group-by key with
/// the aggregated value. The aggregated value is placed as the last column.
///
/// # Type Parameters
/// * `N_GB` - Number of columns in the group-by key
/// * `N_TOT` - Total number of columns in the output relation (should equal N_GB + 1)
///
/// # Returns
/// A closure that merges key-value pairs into complete rows
pub fn aggregation_merge_kv<const N_GB: usize, const N_TOT: usize>(
) -> impl Fn((Row<N_GB>, Row<1>)) -> Row<N_TOT> {
    move |(key, value)| {
        let mut out_row = Row::<N_TOT>::new();

        // First, add all columns from the group-by key
        for i in 0..N_GB {
            out_row.push(key.column(i));
        }

        // Then, add the aggregated value as the last column
        out_row.push(value.column(0));

        out_row
    }
}

// ============================================================================
// Fat Row Variants
// ============================================================================
// These functions provide the same aggregation logic but work with FatRow,
// which has dynamic arity instead of compile-time fixed arity.

/// Fat row version of aggregation reduce logic.
///
/// Similar to `aggregation_reduce_logic` but works with `FatRow` which supports
/// dynamic column counts determined at runtime.
///
/// # Arguments
/// * `aggregation` - The aggregation specification containing the operator
///
/// # Returns
/// A closure that implements the aggregation logic for differential dataflow
pub fn aggregation_reduce_logic_fat(
    aggregation: &Aggregation,
) -> impl FnMut(
    &FatRow,
    &[(&Row<1>, Semiring)],
    &mut Vec<(Row<1>, Semiring)>,
    &mut Vec<(Row<1>, Semiring)>,
) {
    let operator = *aggregation.operator();
    let data_type = *aggregation.data_type();

    // Same `reduce_core` contract as the thin version: emit the new aggregate
    // and subtract the previously-produced `output` so updates and retractions
    // propagate. (The 4th buffer, `updates`, is where emitted deltas go.)
    move |_key, input, output, updates| {
        if !input.is_empty() {
            let values = expand_values(input);
            if let Some(result) = aggregate_values(&values, &operator, &data_type) {
                let mut out = Row::<1>::new();
                out.push(result);
                updates.push((out, semiring_one()));
            }
        }
        for (row, diff) in output.drain(..) {
            let mut neg = diff;
            neg.negate();
            updates.push((row, neg));
        }
    }
}

/// Fat row version of key-value merging after aggregation.
///
/// Reconstructs a `FatRow` by appending the aggregated value to the group-by key.
/// The aggregated value is always placed as the last column.
///
/// # Returns
/// A closure that merges key-value pairs into complete fat rows
pub fn aggregation_merge_kv_fat() -> impl Fn((FatRow, Row<1>)) -> FatRow {
    move |(key, value)| {
        let mut out_row = FatRow::new();

        // Copy all columns from the group-by key
        for i in 0..key.arity() {
            out_row.push(key.column(i));
        }

        // Append the aggregated value as the last column
        out_row.push(value.column(0));

        out_row
    }
}

/// Fat row version of relation separation into key-value pairs.
///
/// Splits a `FatRow` into group-by key (all columns except the last) and
/// the aggregation value (the last column).
///
/// # Returns
/// A closure that separates fat rows into key-value pairs for aggregation
pub fn aggregation_separate_kv_fat() -> impl Fn(FatRow) -> (FatRow, Row<1>) {
    move |row| {
        let mut group_by_row = FatRow::new();
        let mut aggregate_row = Row::<1>::new();

        let arity = row.arity();

        // Extract all columns except the last as the group-by key
        for i in 0..arity - 1 {
            group_by_row.push(row.column(i));
        }

        // Extract the last column as the value to aggregate
        aggregate_row.push(row.column(arity - 1));

        (group_by_row, aggregate_row)
    }
}

#[cfg(test)]
mod negative_zero_tests {
    use super::*;
    use parsing::decl::{encode_float, is_null, DataType};

    /// `avg` can UNDERFLOW to `-0.0`, whose bit pattern is the NULL sentinel, so
    /// the aggregation had to encode its result rather than take raw bits.
    /// Reached here the way a real group would: a tiny negative that averages
    /// below the smallest subnormal and rounds to negative zero.
    #[test]
    fn an_aggregate_underflowing_to_negative_zero_is_not_null() {
        let vals = [encode_float(-5e-324), encode_float(0.0), encode_float(0.0)];
        // The underlying f64 really does land on -0.0 ...
        let raw: f64 = vals.iter().map(|v| f64::from_bits(*v as u64)).sum::<f64>() / 3.0;
        assert!(
            raw == 0.0 && raw.is_sign_negative(),
            "expected -0.0, got {raw}"
        );
        // ... and the aggregation must not report that as NULL.
        let got = aggregate_values(&vals, &AggregationOperator::Avg, &DataType::Float).unwrap();
        assert!(!is_null(got), "avg underflowing to -0.0 reported NULL");
        assert_eq!(f64::from_bits(got as u64), 0.0);
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use parsing::aggregation::AggregationOperator;
    use parsing::decl::NULL_SENTINEL;
    use proptest::collection::vec;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn agg_count_equals_length(values in vec(any::<i64>(), 0..50usize)) {
            prop_assert_eq!(
                aggregate_ints(&values, &AggregationOperator::Count),
                Some(values.len() as i64)
            );
        }

        #[test]
        fn agg_sum_equals_iter_sum(values in vec(any::<i64>(), 0..50usize)) {
            let expected: i128 = values.iter().map(|&x| x as i128).sum();
            // A total outside i64 has no representable answer and reports null
            // rather than a truncated one.
            prop_assert_eq!(
                aggregate_ints(&values, &AggregationOperator::Sum),
                Some(i64::try_from(expected).unwrap_or(NULL_SENTINEL))
            );
        }

        /// Integer avg is the i128-widened sum divided by the count,
        /// truncated toward zero (Rust integer division).
        #[test]
        fn agg_avg_equals_widened_mean(values in vec(any::<i64>(), 1..50usize)) {
            let sum: i128 = values.iter().map(|&x| x as i128).sum();
            let expected = (sum / values.len() as i128) as i64;
            prop_assert_eq!(
                aggregate_ints(&values, &AggregationOperator::Avg),
                Some(expected)
            );
        }

        /// Float avg through the type-aware path, NULLs skipped.
        #[test]
        fn agg_avg_float_skips_nulls(values in vec(-1e6f64..1e6f64, 1..30usize)) {
            let mut encoded: Vec<i64> = values.iter().map(|f| f.to_bits() as i64).collect();
            encoded.push(parsing::decl::NULL_SENTINEL);
            let got = aggregate_values(&encoded, &AggregationOperator::Avg, &DataType::Float)
                .map(|v| f64::from_bits(v as u64));
            let expected = values.iter().sum::<f64>() / values.len() as f64;
            let got = got.unwrap();
            prop_assert!((got - expected).abs() <= expected.abs() * 1e-12 + 1e-9,
                "got {got}, expected {expected}");
        }

        /// Avg is permutation-invariant.
        #[test]
        fn agg_avg_order_independent(mut values in vec(-1000i64..1000, 1..30usize)) {
            let a = aggregate_ints(&values, &AggregationOperator::Avg);
            values.reverse();
            prop_assert_eq!(a, aggregate_ints(&values, &AggregationOperator::Avg));
        }

        #[test]
        fn agg_min_equals_iter_min(values in vec(any::<i64>(), 0..50usize)) {
            prop_assert_eq!(
                aggregate_ints(&values, &AggregationOperator::Min),
                values.iter().min().copied()
            );
        }

        #[test]
        fn agg_max_equals_iter_max(values in vec(any::<i64>(), 0..50usize)) {
            prop_assert_eq!(
                aggregate_ints(&values, &AggregationOperator::Max),
                values.iter().max().copied()
            );
        }

        #[test]
        fn agg_single_element(x in any::<i64>()) {
            let v = vec![x];
            prop_assert_eq!(aggregate_ints(&v, &AggregationOperator::Count), Some(1));
            prop_assert_eq!(aggregate_ints(&v, &AggregationOperator::Sum), Some(x));
            prop_assert_eq!(aggregate_ints(&v, &AggregationOperator::Min), Some(x));
            prop_assert_eq!(aggregate_ints(&v, &AggregationOperator::Max), Some(x));
        }

        #[test]
        fn agg_order_independent(values in vec(any::<i64>(), 2..50usize)) {
            let mut reversed = values.clone();
            reversed.reverse();
            for op in &[
                AggregationOperator::Count,
                AggregationOperator::Sum,
                AggregationOperator::Min,
                AggregationOperator::Max,
            ] {
                prop_assert_eq!(
                    aggregate_ints(&values, op),
                    aggregate_ints(&reversed, op)
                );
            }
        }
    }

    #[test]
    /// A total outside i64 reports null instead of a truncated number.
    ///
    /// The addition is done in i128 so it cannot overflow, but the result was
    /// then cast back with `as i64`, which wraps: summing two values near
    /// i64::MAX produced a small negative number and nothing indicated that
    /// anything had gone wrong. A missing sum is recoverable; a plausible wrong
    /// one is not.
    fn agg_sum_outside_i64_is_null() {
        let huge = vec![i64::MAX, i64::MAX];
        assert_eq!(
            aggregate_ints(&huge, &AggregationOperator::Sum),
            Some(NULL_SENTINEL)
        );

        let very_negative = vec![i64::MIN, i64::MIN];
        assert_eq!(
            aggregate_ints(&very_negative, &AggregationOperator::Sum),
            Some(NULL_SENTINEL)
        );

        // A total that does fit is still exact.
        assert_eq!(
            aggregate_ints(&[i64::MAX, -1], &AggregationOperator::Sum),
            Some(i64::MAX - 1)
        );
    }

    #[test]
    fn agg_empty_count_sum_zero() {
        let empty: Vec<i64> = vec![];
        assert_eq!(aggregate_ints(&empty, &AggregationOperator::Count), Some(0));
        assert_eq!(aggregate_ints(&empty, &AggregationOperator::Sum), Some(0));
    }

    #[test]
    fn agg_empty_min_max_none() {
        let empty: Vec<i64> = vec![];
        assert_eq!(aggregate_ints(&empty, &AggregationOperator::Min), None);
        assert_eq!(aggregate_ints(&empty, &AggregationOperator::Max), None);
        assert_eq!(aggregate_ints(&empty, &AggregationOperator::Avg), None);
    }

    // --- Multiset expansion (reduce input -> one value per body match) ---

    #[test]
    fn expand_values_respects_multiplicity() {
        let mut a = Row::<1>::new();
        a.push(3000);
        let mut b = Row::<1>::new();
        b.push(4000);
        // Three body matches at 3000 arrive folded into one record, diff 3.
        let input: Vec<(&Row<1>, Semiring)> = vec![(&a, 3), (&b, 1)];
        let values = expand_values(&input);
        assert_eq!(
            aggregate_ints(&values, &AggregationOperator::Sum),
            Some(13000)
        );
        assert_eq!(
            aggregate_ints(&values, &AggregationOperator::Count),
            Some(4)
        );
    }

    // --- Type-aware aggregation tests ---

    #[test]
    fn agg_values_count_skips_nulls() {
        let values = vec![1, 2, NULL_SENTINEL];
        assert_eq!(
            aggregate_values(&values, &AggregationOperator::Count, &DataType::Integer),
            Some(2)
        );
    }

    #[test]
    fn agg_values_sum_skips_nulls() {
        let values = vec![10, 20, NULL_SENTINEL];
        assert_eq!(
            aggregate_values(&values, &AggregationOperator::Sum, &DataType::Integer),
            Some(30)
        );
    }

    #[test]
    fn agg_values_all_nulls_count_zero() {
        let values = vec![NULL_SENTINEL, NULL_SENTINEL];
        assert_eq!(
            aggregate_values(&values, &AggregationOperator::Count, &DataType::Integer),
            Some(0)
        );
    }

    #[test]
    fn agg_values_all_nulls_min_max_none() {
        let values = vec![NULL_SENTINEL, NULL_SENTINEL];
        assert_eq!(
            aggregate_values(&values, &AggregationOperator::Min, &DataType::Integer),
            None
        );
        assert_eq!(
            aggregate_values(&values, &AggregationOperator::Max, &DataType::Integer),
            None
        );
    }

    #[test]
    fn agg_float_sum() {
        let a = 1.5_f64.to_bits() as i64;
        let b = 2.5_f64.to_bits() as i64;
        let result = aggregate_values(&[a, b], &AggregationOperator::Sum, &DataType::Float);
        let f = f64::from_bits(result.unwrap() as u64);
        assert!((f - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn agg_float_min_max() {
        let a = 1.5_f64.to_bits() as i64;
        let b = 3.0_f64.to_bits() as i64;
        let c = 2.0_f64.to_bits() as i64;

        let min_result = aggregate_values(&[a, b, c], &AggregationOperator::Min, &DataType::Float);
        let min_f = f64::from_bits(min_result.unwrap() as u64);
        assert!((min_f - 1.5).abs() < f64::EPSILON);

        let max_result = aggregate_values(&[a, b, c], &AggregationOperator::Max, &DataType::Float);
        let max_f = f64::from_bits(max_result.unwrap() as u64);
        assert!((max_f - 3.0).abs() < f64::EPSILON);
    }
}
