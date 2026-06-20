use differential_dataflow::difference::Abelian;
use parsing::aggregation::{Aggregation, AggregationOperator};
use parsing::decl::{is_null, DataType};
use reading::row::{Array, FatRow, Row};
use reading::{semiring_one, Semiring};

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
            let wide: i128 = input.iter().map(|&x| x as i128).sum();
            Some(wide as i64)
        }
        AggregationOperator::Min => input.iter().min().copied(),
        AggregationOperator::Max => input.iter().max().copied(),
    }
}

/// Type-aware aggregation: filters out NULL_SENTINEL values, then dispatches
/// to integer or float aggregation.
fn aggregate_values(input: &[i64], op: &AggregationOperator, dt: &DataType) -> Option<i64> {
    let filtered: Vec<i64> = input.iter().copied().filter(|v| !is_null(*v)).collect();
    match dt {
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
                AggregationOperator::Min => floats
                    .iter()
                    .copied()
                    .fold(f64::INFINITY, f64::min),
                AggregationOperator::Max => floats
                    .iter()
                    .copied()
                    .fold(f64::NEG_INFINITY, f64::max),
                AggregationOperator::Count => unreachable!(),
            };
            Some(result.to_bits() as i64)
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
            let values: Vec<i64> = input.iter().map(|(row, _)| row.column(0)).collect();
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
            let values: Vec<i64> = input.iter().map(|(row, _)| row.column(0)).collect();
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
mod property_tests {
    use super::*;
    use parsing::aggregation::AggregationOperator;
    use parsing::decl::NULL_SENTINEL;
    use proptest::prelude::*;
    use proptest::collection::vec;

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
            prop_assert_eq!(
                aggregate_ints(&values, &AggregationOperator::Sum),
                Some(expected as i64)
            );
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
