use parsing::decl::{is_null, DataType, NULL_SENTINEL};
use parsing::{
    arithmetic::{ArithmeticOperator, BuiltinOp},
    compare::ComparisonOperator,
};
use planning::arguments::TransformationArgument;
use planning::arithmetic::ArithmeticArgument;
use planning::arithmetic::FactorArgument;
use planning::compare::ComparisonExprArgument;
use reading::interner::{decode, intern};
use reading::row::Array;

/// Evaluate a string builtin on already-evaluated `i64` argument values.
/// String args are interned ids decoded back to text; `split_nth`'s index arg is
/// a raw integer. Boolean builtins return `1`/`0`; NULL propagates.
pub fn eval_builtin(op: BuiltinOp, args: &[i64]) -> i64 {
    match op {
        BuiltinOp::SplitNth => {
            if args.len() != 3 || is_null(args[0]) || is_null(args[1]) || args[2] < 0 {
                return NULL_SENTINEL;
            }
            match (decode(args[0]), decode(args[1])) {
                (Some(s), Some(sep)) => match s.split(sep.as_str()).nth(args[2] as usize) {
                    Some(seg) => intern(seg),
                    None => NULL_SENTINEL,
                },
                _ => NULL_SENTINEL,
            }
        }
        BuiltinOp::StartsWith => bool_builtin(args, |s, p| s.starts_with(p)),
        BuiltinOp::Contains => bool_builtin(args, |s, p| s.contains(p)),
        BuiltinOp::StrBefore => bool_builtin(args, |a, b| a < b),
        BuiltinOp::Replace => {
            if args.len() != 3 || is_null(args[0]) || is_null(args[1]) || is_null(args[2]) {
                return NULL_SENTINEL;
            }
            match (decode(args[0]), decode(args[1]), decode(args[2])) {
                (Some(s), Some(from), Some(to)) => intern(&s.replace(from.as_str(), to.as_str())),
                _ => NULL_SENTINEL,
            }
        }
        BuiltinOp::BeforeLast => split_last_builtin(args, |s, idx, _sep_len| &s[..idx]),
        BuiltinOp::AfterLast => split_last_builtin(args, |s, idx, sep_len| &s[idx + sep_len..]),
        BuiltinOp::Concat => {
            if args.len() != 2 || is_null(args[0]) || is_null(args[1]) {
                return NULL_SENTINEL;
            }
            match (decode(args[0]), decode(args[1])) {
                (Some(a), Some(b)) => intern(&format!("{a}{b}")),
                _ => NULL_SENTINEL,
            }
        }
    }
}

/// `before_last`/`after_last` share the "find the last `sep`" logic; the slice
/// they keep differs. Both return the whole string when `sep` is absent or
/// empty, and propagate NULL. The closure picks the kept side from the match
/// position.
fn split_last_builtin(args: &[i64], pick: impl Fn(&str, usize, usize) -> &str) -> i64 {
    if args.len() != 2 || is_null(args[0]) || is_null(args[1]) {
        return NULL_SENTINEL;
    }
    match (decode(args[0]), decode(args[1])) {
        (Some(s), Some(sep)) => {
            if sep.is_empty() {
                return intern(s.as_str());
            }
            match s.rfind(sep.as_str()) {
                Some(idx) => intern(pick(s.as_str(), idx, sep.len())),
                None => intern(s.as_str()),
            }
        }
        _ => NULL_SENTINEL,
    }
}

fn bool_builtin(args: &[i64], f: impl Fn(&str, &str) -> bool) -> i64 {
    if args.len() != 2 || is_null(args[0]) || is_null(args[1]) {
        return NULL_SENTINEL;
    }
    match (decode(args[0]), decode(args[1])) {
        (Some(a), Some(b)) => {
            if f(a.as_str(), b.as_str()) {
                1
            } else {
                0
            }
        }
        _ => NULL_SENTINEL,
    }
}

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

/// Type-aware comparison: dispatches to integer or float comparison.
/// Any comparison involving NULL_SENTINEL returns false (SQL-like null semantics).
pub fn compare_values(x: i64, op: &ComparisonOperator, y: i64, dt: &DataType) -> bool {
    if is_null(x) || is_null(y) {
        return false;
    }
    match dt {
        DataType::Float => {
            let fx = f64::from_bits(x as u64);
            let fy = f64::from_bits(y as u64);
            match op {
                ComparisonOperator::Equals => fx == fy,
                ComparisonOperator::NotEquals => fx != fy,
                ComparisonOperator::GreaterThan => fx > fy,
                ComparisonOperator::GreaterEqualThan => fx >= fy,
                ComparisonOperator::LessThan => fx < fy,
                ComparisonOperator::LessEqualThan => fx <= fy,
            }
        }
        _ => compare_ints(x, op, y),
    }
}

/// Type-aware arithmetic: dispatches to integer or float mode.
/// If any operand is NULL_SENTINEL, returns NULL_SENTINEL.
/// Integer mode: division/modulo by zero returns NULL_SENTINEL.
/// Float mode: uses native f64 operations (div by zero → Inf/NaN).
pub fn arithmetic_values(init: i64, rest: &[(&ArithmeticOperator, i64)], dt: &DataType) -> i64 {
    if is_null(init) || rest.iter().any(|(_, v)| is_null(*v)) {
        return NULL_SENTINEL;
    }
    match dt {
        DataType::Float => {
            let mut result = f64::from_bits(init as u64);
            for (op, value) in rest {
                let fv = f64::from_bits(*value as u64);
                match op {
                    ArithmeticOperator::Plus => result += fv,
                    ArithmeticOperator::Minus => result -= fv,
                    ArithmeticOperator::Multiply => result *= fv,
                    ArithmeticOperator::Divide => result /= fv,
                    ArithmeticOperator::Modulo => result %= fv,
                }
            }
            result.to_bits() as i64
        }
        _ => {
            let mut result = init;
            for (op, value) in rest {
                match op {
                    ArithmeticOperator::Plus => result += value,
                    ArithmeticOperator::Minus => result -= value,
                    ArithmeticOperator::Multiply => result *= value,
                    ArithmeticOperator::Divide => {
                        if *value == 0 {
                            return NULL_SENTINEL;
                        }
                        result /= value;
                    }
                    ArithmeticOperator::Modulo => {
                        if *value == 0 {
                            return NULL_SENTINEL;
                        }
                        result %= value;
                    }
                }
            }
            result
        }
    }
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
        FactorArgument::Const(constant) => constant.as_i64(),
        FactorArgument::Builtin(op, args) => {
            let vals: Vec<i64> = args.iter().map(|a| factor_row(v, a)).collect();
            eval_builtin(*op, &vals)
        }
    }
}

pub fn arithmetic_row(v: &dyn Array, arithmetic_expr: &ArithmeticArgument) -> i64 {
    let init = factor_row(v, arithmetic_expr.init());
    let rest = arithmetic_expr
        .rest()
        .iter()
        .map(|(op, factor)| (op, factor_row(v, factor)))
        .collect::<Vec<_>>();

    arithmetic_values(init, &rest, arithmetic_expr.data_type())
}

pub fn compare_row(v: &dyn Array, compare_expr: &ComparisonExprArgument) -> bool {
    let left = arithmetic_row(v, compare_expr.left());
    let right = arithmetic_row(v, compare_expr.right());
    compare_values(
        left,
        compare_expr.operator(),
        right,
        compare_expr.left().data_type(),
    )
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
    compare_values(
        left,
        compare_expr.operator(),
        right,
        compare_expr.left().data_type(),
    )
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

    arithmetic_values(init, &rest, arithmetic_expr.data_type())
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
        FactorArgument::Const(constant) => constant.as_i64(),
        FactorArgument::Builtin(op, args) => {
            let vals: Vec<i64> = args.iter().map(|a| jn_factor(k, v1, v2, a)).collect();
            eval_builtin(*op, &vals)
        }
    }
}

#[cfg(test)]
mod builtin_tests {
    use super::*;
    use reading::interner::{decode, intern};

    fn call2(op: BuiltinOp, s: &str, sep: &str) -> String {
        let r = eval_builtin(op, &[intern(s), intern(sep)]);
        decode(r).map(|c| c.to_string()).unwrap_or_default()
    }

    #[test]
    fn after_last_basename() {
        assert_eq!(call2(BuiltinOp::AfterLast, "a/b/c.rs", "/"), "c.rs");
        assert_eq!(call2(BuiltinOp::AfterLast, "c.rs", "/"), "c.rs"); // sep absent -> whole
        assert_eq!(call2(BuiltinOp::AfterLast, "App.tsx", "."), "tsx");
    }

    #[test]
    fn before_last_dirname_and_stem() {
        assert_eq!(call2(BuiltinOp::BeforeLast, "a/b/c.rs", "/"), "a/b");
        assert_eq!(call2(BuiltinOp::BeforeLast, "App.tsx", "."), "App");
        assert_eq!(call2(BuiltinOp::BeforeLast, "noext", "."), "noext"); // sep absent -> whole
    }

    #[test]
    fn composed_basename_without_extension() {
        // before_last(after_last(File, "/"), ".") = file stem, the resolver's key.
        let base = eval_builtin(
            BuiltinOp::AfterLast,
            &[intern("web/src/Graph.tsx"), intern("/")],
        );
        let stem = eval_builtin(BuiltinOp::BeforeLast, &[base, intern(".")]);
        assert_eq!(decode(stem).unwrap().to_string(), "Graph");
    }

    #[test]
    fn null_propagates() {
        assert_eq!(
            eval_builtin(BuiltinOp::AfterLast, &[NULL_SENTINEL, intern("/")]),
            NULL_SENTINEL
        );
        assert_eq!(
            eval_builtin(BuiltinOp::Concat, &[intern("a"), NULL_SENTINEL]),
            NULL_SENTINEL
        );
    }

    #[test]
    fn concat_joins() {
        assert_eq!(call2(BuiltinOp::Concat, "crates/executing", "/"), "crates/executing/");
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

    // --- Type-aware comparison tests ---

    proptest! {
        #[test]
        fn compare_values_int_matches_compare_ints(x in any::<i64>(), y in any::<i64>()) {
            // Backward compatibility: compare_values with Integer matches compare_ints
            // (unless x or y is NULL_SENTINEL, where behavior diverges)
            if !is_null(x) && !is_null(y) {
                for op in &[
                    ComparisonOperator::Equals,
                    ComparisonOperator::NotEquals,
                    ComparisonOperator::GreaterThan,
                    ComparisonOperator::GreaterEqualThan,
                    ComparisonOperator::LessThan,
                    ComparisonOperator::LessEqualThan,
                ] {
                    prop_assert_eq!(
                        compare_values(x, op, y, &DataType::Integer),
                        compare_ints(x, op, y)
                    );
                }
            }
        }

        #[test]
        fn compare_floats_reflexive(x_bits in any::<u64>()) {
            let f = f64::from_bits(x_bits);
            if f.is_finite() {
                let bits = f.to_bits() as i64;
                if !is_null(bits) {
                    prop_assert!(compare_values(bits, &ComparisonOperator::Equals, bits, &DataType::Float));
                }
            }
        }

        #[test]
        fn compare_floats_ordering(x_bits in any::<u64>(), y_bits in any::<u64>()) {
            let fx = f64::from_bits(x_bits);
            let fy = f64::from_bits(y_bits);
            if fx.is_finite() && fy.is_finite() {
                let xb = fx.to_bits() as i64;
                let yb = fy.to_bits() as i64;
                if !is_null(xb) && !is_null(yb) {
                    prop_assert_eq!(
                        compare_values(xb, &ComparisonOperator::LessThan, yb, &DataType::Float),
                        fx < fy
                    );
                }
            }
        }

        #[test]
        fn compare_floats_trichotomy(x_bits in any::<u64>(), y_bits in any::<u64>()) {
            let fx = f64::from_bits(x_bits);
            let fy = f64::from_bits(y_bits);
            if fx.is_finite() && fy.is_finite() {
                let xb = fx.to_bits() as i64;
                let yb = fy.to_bits() as i64;
                if !is_null(xb) && !is_null(yb) {
                    let lt = compare_values(xb, &ComparisonOperator::LessThan, yb, &DataType::Float);
                    let eq = compare_values(xb, &ComparisonOperator::Equals, yb, &DataType::Float);
                    let gt = compare_values(xb, &ComparisonOperator::GreaterThan, yb, &DataType::Float);
                    prop_assert_eq!(lt as u8 + eq as u8 + gt as u8, 1);
                }
            }
        }
    }

    #[test]
    fn compare_null_always_false() {
        for op in &[
            ComparisonOperator::Equals,
            ComparisonOperator::NotEquals,
            ComparisonOperator::GreaterThan,
            ComparisonOperator::GreaterEqualThan,
            ComparisonOperator::LessThan,
            ComparisonOperator::LessEqualThan,
        ] {
            // NULL vs non-null
            assert!(!compare_values(NULL_SENTINEL, op, 42, &DataType::Integer));
            assert!(!compare_values(42, op, NULL_SENTINEL, &DataType::Integer));
            // NULL vs NULL
            assert!(!compare_values(
                NULL_SENTINEL,
                op,
                NULL_SENTINEL,
                &DataType::Integer
            ));
            // Float mode
            let one = 1.0_f64.to_bits() as i64;
            assert!(!compare_values(NULL_SENTINEL, op, one, &DataType::Float));
            assert!(!compare_values(one, op, NULL_SENTINEL, &DataType::Float));
        }
    }

    // --- Type-aware arithmetic tests ---

    #[test]
    fn arithmetic_null_propagates() {
        // NULL in init
        assert_eq!(
            arithmetic_values(
                NULL_SENTINEL,
                &[(&ArithmeticOperator::Plus, 1)],
                &DataType::Integer
            ),
            NULL_SENTINEL
        );
        // NULL in rest
        assert_eq!(
            arithmetic_values(
                1,
                &[(&ArithmeticOperator::Plus, NULL_SENTINEL)],
                &DataType::Integer
            ),
            NULL_SENTINEL
        );
    }

    #[test]
    fn div_by_zero_int_returns_null() {
        assert_eq!(
            arithmetic_values(42, &[(&ArithmeticOperator::Divide, 0)], &DataType::Integer),
            NULL_SENTINEL
        );
    }

    #[test]
    fn mod_by_zero_int_returns_null() {
        assert_eq!(
            arithmetic_values(42, &[(&ArithmeticOperator::Modulo, 0)], &DataType::Integer),
            NULL_SENTINEL
        );
    }

    #[test]
    fn div_by_zero_float_returns_inf() {
        let one = 1.0_f64.to_bits() as i64;
        let zero = 0.0_f64.to_bits() as i64;
        let result = arithmetic_values(
            one,
            &[(&ArithmeticOperator::Divide, zero)],
            &DataType::Float,
        );
        let f = f64::from_bits(result as u64);
        assert!(f.is_infinite() && f > 0.0);
    }

    proptest! {
        #[test]
        fn float_arith_add_commutative(x_f64 in any::<f64>(), y_f64 in any::<f64>()) {
            if x_f64.is_finite() && y_f64.is_finite() {
                let xb = x_f64.to_bits() as i64;
                let yb = y_f64.to_bits() as i64;
                if !is_null(xb) && !is_null(yb) {
                    let xy = arithmetic_values(xb, &[(&ArithmeticOperator::Plus, yb)], &DataType::Float);
                    let yx = arithmetic_values(yb, &[(&ArithmeticOperator::Plus, xb)], &DataType::Float);
                    prop_assert_eq!(xy, yx);
                }
            }
        }

        #[test]
        fn float_arith_identity(x_f64 in any::<f64>()) {
            if x_f64.is_finite() {
                let xb = x_f64.to_bits() as i64;
                let zero = 0.0_f64.to_bits() as i64;
                let one = 1.0_f64.to_bits() as i64;
                if !is_null(xb) {
                    // x + 0.0 == x
                    let add_zero = arithmetic_values(xb, &[(&ArithmeticOperator::Plus, zero)], &DataType::Float);
                    let result_f = f64::from_bits(add_zero as u64);
                    let x_f = f64::from_bits(xb as u64);
                    prop_assert!((result_f - x_f).abs() < f64::EPSILON || (result_f == 0.0 && x_f == 0.0));
                    // x * 1.0 == x
                    let mul_one = arithmetic_values(xb, &[(&ArithmeticOperator::Multiply, one)], &DataType::Float);
                    let result_f = f64::from_bits(mul_one as u64);
                    prop_assert_eq!(result_f, x_f);
                }
            }
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
