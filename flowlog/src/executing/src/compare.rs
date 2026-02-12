use parsing::{arithmetic::ArithmeticOperator, compare::ComparisonOperator};
use planning::arguments::TransformationArgument;
use planning::arithmetic::ArithmeticArgument;
use planning::arithmetic::FactorArgument;
use planning::compare::ComparisonExprArgument;
use reading::row::Array;

pub fn compare_ints(x: i64, op: &ComparisonOperator, y: i64) -> bool {
    match op {
        ComparisonOperator::Equals => x == y,
        ComparisonOperator::NotEquals => x != y,
        ComparisonOperator::GreaterThan => x > y,
        ComparisonOperator::GreaterEqualThan => x >= y,
        ComparisonOperator::LessThan => x < y,
        ComparisonOperator::LessEqualThan => x <= y,
    }
}

pub fn arithmetic_ints(init: i64, rest: &[(&ArithmeticOperator, i64)]) -> i64 {
    let mut result = init;
    for (op, value) in rest {
        match op {
            ArithmeticOperator::Plus => result += value,
            ArithmeticOperator::Minus => result -= value,
            ArithmeticOperator::Multiply => result *= value,
            ArithmeticOperator::Divide => result /= value,
            ArithmeticOperator::Modulo => result %= value,
        }
    }
    result
}

/* ------------------------------ */
/* compare for rows */
/* ------------------------------ */
pub fn factor_row(v: &dyn Array, factor: &FactorArgument) -> i64 {
    match factor {
        FactorArgument::Var(transformation_arg) => match transformation_arg {
            TransformationArgument::KV((true, id)) => v.column(*id),
            _ => panic!("factor_row: expected a kv argument"),
        },
        FactorArgument::Const(constant) => constant.integer(),
    }
}

pub fn arithmetic_row(v: &dyn Array, arithmetic_expr: &ArithmeticArgument) -> i64 {
    let init = factor_row(v, arithmetic_expr.init());
    let rest = arithmetic_expr
        .rest()
        .iter()
        .map(|(op, factor)| (op, factor_row(v, factor)))
        .collect::<Vec<_>>();

    arithmetic_ints(init, &rest)
}

pub fn compare_row(v: &dyn Array, compare_expr: &ComparisonExprArgument) -> bool {
    let left = arithmetic_row(v, compare_expr.left());
    let right = arithmetic_row(v, compare_expr.right());
    compare_ints(left, compare_expr.operator(), right)
}

/* ---------------------------------------------- */
/* compare for joins (fused into joins) */
/* ---------------------------------------------- */
pub fn jn_compare_extractor(
    k: Option<&dyn Array>,
    v1: Option<&dyn Array>,
    v2: Option<&dyn Array>,
    extracts: &(bool, bool, usize),
) -> i64 {
    let (left_or_right, key_or_value, id) = extracts;
    if !key_or_value {
        // from key
        match k {
            Some(k) => k.column(*id),
            None => panic!("jn_compare_extractor: missing key array"),
        }
    } else {
        // from value
        match (left_or_right, v1, v2) {
            (false, Some(v1), _) => v1.column(*id), // from left if v1 is provided
            (true, _, Some(v2)) => v2.column(*id),  // from right if v2 is provided
            _ => panic!("jn_compare_extractor: bad arguments"),
        }
    }
}

pub fn jn_compare(
    k: Option<&dyn Array>,
    v1: Option<&dyn Array>,
    v2: Option<&dyn Array>,
    compare_expr: &ComparisonExprArgument,
) -> bool {
    let left = jn_arithmetic(k, v1, v2, compare_expr.left());
    let right = jn_arithmetic(k, v1, v2, compare_expr.right());
    compare_ints(left, compare_expr.operator(), right)
}

pub fn jn_arithmetic(
    k: Option<&dyn Array>,
    v1: Option<&dyn Array>,
    v2: Option<&dyn Array>,
    arithmetic_expr: &ArithmeticArgument,
) -> i64 {
    let init = jn_factor(k, v1, v2, arithmetic_expr.init());
    let rest = arithmetic_expr
        .rest()
        .iter()
        .map(|(op, factor)| (op, jn_factor(k, v1, v2, factor)))
        .collect::<Vec<_>>();

    arithmetic_ints(init, &rest)
}

pub fn jn_factor(
    k: Option<&dyn Array>,
    v1: Option<&dyn Array>,
    v2: Option<&dyn Array>,
    factor: &FactorArgument,
) -> i64 {
    match factor {
        FactorArgument::Var(transformation_arg) => match transformation_arg {
            TransformationArgument::Jn(extracts) => jn_compare_extractor(k, v1, v2, extracts),
            _ => panic!("jn_factor: expected a jn argument"),
        },
        FactorArgument::Const(constant) => constant.integer(),
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    // --- Comparison tests ---

    proptest! {
        #[test]
        fn compare_equals_reflexive(x in any::<i64>()) {
            prop_assert!(compare_ints(x, &ComparisonOperator::Equals, x));
        }

        #[test]
        fn compare_equals_symmetric(x in any::<i64>(), y in any::<i64>()) {
            prop_assert_eq!(
                compare_ints(x, &ComparisonOperator::Equals, y),
                compare_ints(y, &ComparisonOperator::Equals, x)
            );
        }

        #[test]
        fn compare_not_equals_negation(x in any::<i64>(), y in any::<i64>()) {
            prop_assert_eq!(
                compare_ints(x, &ComparisonOperator::NotEquals, y),
                !compare_ints(x, &ComparisonOperator::Equals, y)
            );
        }

        #[test]
        fn compare_greater_than_transitive(
            x in any::<i64>(),
            y in any::<i64>(),
            z in any::<i64>(),
        ) {
            if compare_ints(x, &ComparisonOperator::GreaterThan, y)
                && compare_ints(y, &ComparisonOperator::GreaterThan, z)
            {
                prop_assert!(compare_ints(x, &ComparisonOperator::GreaterThan, z));
            }
        }

        #[test]
        fn compare_trichotomy(x in any::<i64>(), y in any::<i64>()) {
            let lt = compare_ints(x, &ComparisonOperator::LessThan, y);
            let eq = compare_ints(x, &ComparisonOperator::Equals, y);
            let gt = compare_ints(x, &ComparisonOperator::GreaterThan, y);
            // exactly one must hold
            prop_assert_eq!(lt as u8 + eq as u8 + gt as u8, 1);
        }

        #[test]
        fn compare_gte_equiv(x in any::<i64>(), y in any::<i64>()) {
            let gte = compare_ints(x, &ComparisonOperator::GreaterEqualThan, y);
            let gt_or_eq = compare_ints(x, &ComparisonOperator::GreaterThan, y)
                || compare_ints(x, &ComparisonOperator::Equals, y);
            prop_assert_eq!(gte, gt_or_eq);
        }

        #[test]
        fn compare_lte_equiv(x in any::<i64>(), y in any::<i64>()) {
            let lte = compare_ints(x, &ComparisonOperator::LessEqualThan, y);
            let lt_or_eq = compare_ints(x, &ComparisonOperator::LessThan, y)
                || compare_ints(x, &ComparisonOperator::Equals, y);
            prop_assert_eq!(lte, lt_or_eq);
        }

        #[test]
        fn compare_duality(x in any::<i64>(), y in any::<i64>()) {
            prop_assert_eq!(
                compare_ints(x, &ComparisonOperator::LessThan, y),
                compare_ints(y, &ComparisonOperator::GreaterThan, x)
            );
        }
    }

    // --- Arithmetic tests (use i32 range to avoid overflow) ---

    proptest! {
        #[test]
        fn arith_empty_rest_identity(x in any::<i64>()) {
            prop_assert_eq!(arithmetic_ints(x, &[]), x);
        }

        #[test]
        fn arith_add_commutative(x in any::<i32>(), y in any::<i32>()) {
            let x = x as i64;
            let y = y as i64;
            let xy = arithmetic_ints(x, &[(&ArithmeticOperator::Plus, y)]);
            let yx = arithmetic_ints(y, &[(&ArithmeticOperator::Plus, x)]);
            prop_assert_eq!(xy, yx);
        }

        #[test]
        fn arith_add_associative(x in any::<i32>(), y in any::<i32>(), z in any::<i32>()) {
            let x = x as i64;
            let y = y as i64;
            let z = z as i64;
            // (x + y) + z
            let xy = arithmetic_ints(x, &[(&ArithmeticOperator::Plus, y)]);
            let xy_z = arithmetic_ints(xy, &[(&ArithmeticOperator::Plus, z)]);
            // x + (y + z)
            let yz = arithmetic_ints(y, &[(&ArithmeticOperator::Plus, z)]);
            let x_yz = arithmetic_ints(x, &[(&ArithmeticOperator::Plus, yz)]);
            prop_assert_eq!(xy_z, x_yz);
        }

        #[test]
        fn arith_mul_commutative(x in any::<i32>(), y in any::<i32>()) {
            let x = x as i64;
            let y = y as i64;
            let xy = arithmetic_ints(x, &[(&ArithmeticOperator::Multiply, y)]);
            let yx = arithmetic_ints(y, &[(&ArithmeticOperator::Multiply, x)]);
            prop_assert_eq!(xy, yx);
        }

        #[test]
        fn arith_sub_inverse_add(x in any::<i32>(), y in any::<i32>()) {
            let x = x as i64;
            let y = y as i64;
            // (x + y) - y == x
            let result = arithmetic_ints(x, &[
                (&ArithmeticOperator::Plus, y),
                (&ArithmeticOperator::Minus, y),
            ]);
            prop_assert_eq!(result, x);
        }

        #[test]
        fn arith_additive_identity(x in any::<i64>()) {
            prop_assert_eq!(arithmetic_ints(x, &[(&ArithmeticOperator::Plus, 0)]), x);
            prop_assert_eq!(arithmetic_ints(x, &[(&ArithmeticOperator::Multiply, 1)]), x);
        }
    }
}
