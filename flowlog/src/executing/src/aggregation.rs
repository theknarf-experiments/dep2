use parsing::aggregation::{Aggregation, AggregationOperator};
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

    move |_key, input, _output, updates| {
        let mut out = Row::<1>::new();

        // Extract values from input rows for aggregation
        let values: Vec<i64> = input.iter().map(|(row, _)| row.column(0)).collect();

        if let Some(result) = aggregate_ints(&values, &operator) {
            out.push(result);
            updates.push((out, semiring_one()));
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

    move |_key, input, output, _fuel| {
        let mut out = Row::<1>::new();

        let values: Vec<i64> = input.iter().map(|(row, _)| row.column(0)).collect();

        if let Some(result) = aggregate_ints(&values, &operator) {
            out.push(result);
            output.push((out, semiring_one()));
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
}
