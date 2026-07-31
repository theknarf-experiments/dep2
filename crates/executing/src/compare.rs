use arrayvec::ArrayVec;
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
///
/// String builtins are MEMOIZED per thread on their argument ids: evaluation
/// pays decode (shard lock + refcount), the string operation, and re-interning
/// the result (byte hash + shard lock), while the same call recurs constantly —
/// across rules sharing a builtin over one column, across deltas, and across
/// fixpoint iterations. The interner is monotonic and process-global, so
/// `(op, args) -> result` is stable and the cache is semantics-free.
pub fn eval_builtin(op: BuiltinOp, args: &[i64]) -> i64 {
    // Only ops that decode/intern; numeric ops are cheaper than a map probe.
    let memoize = matches!(
        op,
        BuiltinOp::SplitNth
            | BuiltinOp::StartsWith
            | BuiltinOp::Contains
            | BuiltinOp::StrBefore
            | BuiltinOp::Replace
            | BuiltinOp::BeforeLast
            | BuiltinOp::AfterLast
            | BuiltinOp::Concat
            | BuiltinOp::ExtractNumber
            | BuiltinOp::DateEpoch
            | BuiltinOp::ToLower
            | BuiltinOp::ToUpper
            | BuiltinOp::Similarity
    ) && args.len() <= 3;
    if !memoize {
        return eval_builtin_uncached(op, args);
    }

    // Arity is fixed per op (op is part of the key), so zero-padding is safe.
    let mut key = (op as u8, [0i64; 3]);
    key.1[..args.len()].copy_from_slice(args);

    BUILTIN_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if let Some(&v) = memo.get(&key) {
            return v;
        }
        let v = eval_builtin_uncached(op, args);
        // Bound the cache: dump wholesale past the cap (simple, and a full
        // cache means the workload's key set is huge anyway).
        if memo.len() >= BUILTIN_MEMO_CAP {
            memo.clear();
        }
        memo.insert(key, v);
        v
    })
}

/// ~1M entries ≈ 40MB per dataflow worker thread at the bound.
const BUILTIN_MEMO_CAP: usize = 1 << 20;

thread_local! {
    static BUILTIN_MEMO: std::cell::RefCell<rustc_hash::FxHashMap<(u8, [i64; 3]), i64>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

fn eval_builtin_uncached(op: BuiltinOp, args: &[i64]) -> i64 {
    match op {
        BuiltinOp::SplitNth => {
            if args.len() != 3 || is_null(args[0]) || is_null(args[1]) || args[2] < 0 {
                return NULL_SENTINEL;
            }
            match (decode(args[0]), decode(args[1])) {
                (Some(s), Some(sep)) => match s.split(sep.as_ref()).nth(args[2] as usize) {
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
                (Some(s), Some(from), Some(to)) => intern(&s.replace(from.as_ref(), to.as_ref())),
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
        BuiltinOp::ExtractNumber => {
            if args.len() != 1 || is_null(args[0]) {
                return NULL_SENTINEL;
            }
            match decode(args[0]) {
                Some(s) => extract_number(&s),
                None => NULL_SENTINEL,
            }
        }
        BuiltinOp::DateEpoch => {
            if args.len() != 1 || is_null(args[0]) {
                return NULL_SENTINEL;
            }
            match decode(args[0]) {
                Some(s) => date_epoch(&s),
                None => NULL_SENTINEL,
            }
        }
        // Numeric conversions: the explicit bridges across the typing pass's
        // no-implicit-mixing rule. `as` casts saturate on overflow/NaN.
        BuiltinOp::ToFloat => {
            if args.len() != 1 || is_null(args[0]) {
                return NULL_SENTINEL;
            }
            (args[0] as f64).to_bits() as i64
        }
        BuiltinOp::Round => {
            if args.len() != 1 || is_null(args[0]) {
                return NULL_SENTINEL;
            }
            f64::from_bits(args[0] as u64).round() as i64
        }
        BuiltinOp::Floor => {
            if args.len() != 1 || is_null(args[0]) {
                return NULL_SENTINEL;
            }
            f64::from_bits(args[0] as u64).floor() as i64
        }
        BuiltinOp::ToLower => case_builtin(args, str::to_lowercase),
        BuiltinOp::ToUpper => case_builtin(args, str::to_uppercase),
        // Float math. NaN results become NULL (a NaN would silently fail every
        // comparison and decode confusingly); infinities keep their IEEE-754
        // value, consistent with float division by zero.
        BuiltinOp::Ln => float_builtin(args, f64::ln),
        BuiltinOp::Exp => float_builtin(args, f64::exp),
        BuiltinOp::Sqrt => float_builtin(args, f64::sqrt),
        BuiltinOp::Pow => {
            if args.len() != 2 || is_null(args[0]) || is_null(args[1]) {
                return NULL_SENTINEL;
            }
            let base = f64::from_bits(args[0] as u64);
            let exponent = f64::from_bits(args[1] as u64);
            float_result(base.powf(exponent))
        }
        BuiltinOp::AbsInt => {
            if args.len() != 1 || is_null(args[0]) {
                return NULL_SENTINEL;
            }
            // i64::MIN has no positive counterpart.
            args[0].checked_abs().unwrap_or(NULL_SENTINEL)
        }
        BuiltinOp::AbsFloat => float_builtin(args, f64::abs),
        // The generic op is resolved to AbsInt/AbsFloat by the typing pass; it
        // can only reach evaluation through a hand-built, untyped AST.
        BuiltinOp::Abs => NULL_SENTINEL,
        BuiltinOp::Similarity => {
            if args.len() != 2 || is_null(args[0]) || is_null(args[1]) {
                return NULL_SENTINEL;
            }
            match (decode(args[0]), decode(args[1])) {
                (Some(a), Some(b)) => similarity(a.as_ref(), b.as_ref()),
                _ => NULL_SENTINEL,
            }
        }
    }
}

/// Sørensen–Dice coefficient over character bigrams, scaled to 0..100.
/// Bigram *multisets* (repeats count), so "aaaa" vs "aa" isn't a perfect
/// match. Strings too short for bigrams compare by equality.
fn similarity(a: &str, b: &str) -> i64 {
    let bigrams = |s: &str| {
        let chars: Vec<char> = s.chars().collect();
        let mut counts: std::collections::HashMap<(char, char), i64> =
            std::collections::HashMap::new();
        for w in chars.windows(2) {
            *counts.entry((w[0], w[1])).or_insert(0) += 1;
        }
        counts
    };
    let (ca, cb) = (bigrams(a), bigrams(b));
    let (na, nb): (i64, i64) = (ca.values().sum(), cb.values().sum());
    if na == 0 || nb == 0 {
        return if a == b { 100 } else { 0 };
    }
    let overlap: i64 = ca
        .iter()
        .map(|(bg, n)| n.min(cb.get(bg).unwrap_or(&0)))
        .sum();
    (200 * overlap) / (na + nb)
}

/// One-float-argument builtin: NULL propagates in, NaN results become NULL.
fn float_builtin(args: &[i64], f: impl Fn(f64) -> f64) -> i64 {
    if args.len() != 1 || is_null(args[0]) {
        return NULL_SENTINEL;
    }
    float_result(f(f64::from_bits(args[0] as u64)))
}

fn float_result(v: f64) -> i64 {
    if v.is_nan() {
        NULL_SENTINEL
    } else {
        v.to_bits() as i64
    }
}

fn case_builtin(args: &[i64], fold: impl Fn(&str) -> String) -> i64 {
    if args.len() != 1 || is_null(args[0]) {
        return NULL_SENTINEL;
    }
    match decode(args[0]) {
        Some(s) => intern(&fold(s.as_ref())),
        None => NULL_SENTINEL,
    }
}

/// Unix epoch seconds of an ISO-8601 timestamp: `YYYY-MM-DD`, optionally
/// followed by `THH:MM` or `THH:MM:SS`; fractional seconds and a trailing `Z`
/// are ignored (times are taken as UTC — numeric offsets are not applied).
/// NULL on anything that doesn't parse. Days via Howard Hinnant's
/// `days_from_civil`, so no calendar tables.
fn date_epoch(s: &str) -> i64 {
    fn num(s: &str) -> Option<i64> {
        (!s.is_empty()).then(|| s.parse::<i64>().ok())?
    }
    fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + doe - 719468
    }
    let parse = || -> Option<i64> {
        let (date, time) = match s.split_once('T') {
            Some((date, time)) => (date, Some(time)),
            None => (s.trim_end_matches('Z'), None),
        };
        let mut date_parts = date.split('-');
        let y = num(date_parts.next()?)?;
        let m = num(date_parts.next()?)?;
        let d = num(date_parts.next()?)?;
        if date_parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return None;
        }
        let (h, min, sec) = match time {
            None => (0, 0, 0),
            Some(time) => {
                // "17:59:00.123Z" -> "17:59:00.123"; offsets like "+02:00" are
                // not applied (treated as parse failure to avoid silent skew).
                let time = time.trim_end_matches('Z');
                let time = time.split_once('.').map(|(t, _)| t).unwrap_or(time);
                if time.contains('+') {
                    return None;
                }
                let mut time_parts = time.split(':');
                let h = num(time_parts.next()?)?;
                let min = num(time_parts.next()?)?;
                let sec = match time_parts.next() {
                    Some(sec) => num(sec)?,
                    None => 0,
                };
                if time_parts.next().is_some() || h > 23 || min > 59 || sec > 60 {
                    return None;
                }
                (h, min, sec)
            }
        };
        Some(days_from_civil(y, m, d) * 86400 + h * 3600 + min * 60 + sec)
    };
    parse().unwrap_or(NULL_SENTINEL)
}

/// First integer in `s` as a raw value (not interned). A digit run may contain
/// `,` thousands separators, so `"a backlog of 47,500 rows"` yields 47500.
/// NULL when there is no digit or the number overflows i64.
fn extract_number(s: &str) -> i64 {
    let bytes = s.as_bytes();
    let Some(start) = bytes.iter().position(|b| b.is_ascii_digit()) else {
        return NULL_SENTINEL;
    };
    let mut digits = String::new();
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_digit() {
            digits.push(b as char);
        } else if b == b',' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            // thousands separator inside the run
        } else {
            break;
        }
        i += 1;
    }
    digits.parse().unwrap_or(NULL_SENTINEL)
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
                return intern(s.as_ref());
            }
            match s.rfind(sep.as_ref()) {
                Some(idx) => intern(pick(s.as_ref(), idx, sep.len())),
                None => intern(s.as_ref()),
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
            if f(a.as_ref(), b.as_ref()) {
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

/// Integer arithmetic that has no answer for some inputs, and says so.
///
/// Every operation here is fallible on a 64-bit integer: `+`, `-` and `*` can
/// leave the representable range, and `/` and `%` are undefined at zero. The
/// obvious implementation uses the plain operators, which in a debug build
/// panics and in a release build silently wraps — and this code runs on a
/// timely worker thread, where a panic does not merely fail the query but takes
/// the worker down and leaves the dataflow unable to complete an epoch. A
/// wrong-but-quiet answer in release and a wedged engine in debug are two
/// faces of the same missing case.
///
/// So overflow yields `NULL_SENTINEL`, exactly as division by zero already did.
/// That choice is not arbitrary: null already means "this expression has no
/// value", it already propagates through further arithmetic, and comparisons
/// against it are already false, so an overflowing row drops out of a result
/// instead of poisoning it with a wrapped number.
///
/// Note that `NULL_SENTINEL` is `i64::MIN`, so a computation landing exactly on
/// `i64::MIN` is indistinguishable from one that overflowed. That is a
/// pre-existing property of the encoding rather than something introduced here.
fn checked_int_arithmetic(init: i64, rest: &[(&ArithmeticOperator, i64)]) -> i64 {
    let mut result = init;
    for (op, value) in rest {
        let next = match op {
            ArithmeticOperator::Plus => result.checked_add(*value),
            ArithmeticOperator::Minus => result.checked_sub(*value),
            ArithmeticOperator::Multiply => result.checked_mul(*value),
            // `checked_div`/`checked_rem` cover division by zero and the one
            // overflowing division, `i64::MIN / -1`.
            ArithmeticOperator::Divide => result.checked_div(*value),
            ArithmeticOperator::Modulo => result.checked_rem(*value),
            // Bitwise operations are total on i64: every bit pattern is a
            // value, so there is nothing to check.
            ArithmeticOperator::BitAnd => Some(result & value),
            ArithmeticOperator::BitOr => Some(result | value),
            ArithmeticOperator::BitXor => Some(result ^ value),
        };
        match next {
            Some(v) => result = v,
            None => return NULL_SENTINEL,
        }
    }
    result
}

pub fn arithmetic_ints(init: i64, rest: &[(&ArithmeticOperator, i64)]) -> i64 {
    checked_int_arithmetic(init, rest)
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
/// Integer mode: division/modulo by zero and overflow return NULL_SENTINEL
/// (see [`checked_int_arithmetic`]).
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
                    // Bitwise operations have no meaning on a float. Applying
                    // them to the IEEE bit pattern would produce a number
                    // rather than an error, which is worse than nothing.
                    ArithmeticOperator::BitAnd
                    | ArithmeticOperator::BitOr
                    | ArithmeticOperator::BitXor => return NULL_SENTINEL,
                }
            }
            result.to_bits() as i64
        }
        _ => checked_int_arithmetic(init, rest),
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
            // Builtins take at most 3 args (split_nth); keep the evaluated
            // operands on the stack to avoid a heap Vec per row.
            let vals: ArrayVec<i64, 4> = args.iter().map(|a| factor_row(v, a)).collect();
            eval_builtin(*op, &vals)
        }
        FactorArgument::Paren(inner) => arithmetic_row(v, inner),
    }
}

pub fn arithmetic_row(v: &dyn Array, arithmetic_expr: &ArithmeticArgument) -> i64 {
    let init = factor_row(v, arithmetic_expr.init());
    // The common case is a bare factor (no +/-/*...): skip the per-row Vec alloc
    // and arithmetic fold and return the value directly (an empty `rest` leaves
    // the value unchanged anyway).
    let rest_raw = arithmetic_expr.rest();
    if rest_raw.is_empty() {
        return init;
    }
    let rest = rest_raw
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
    // Common case: a bare factor with no arithmetic — return it directly and
    // skip the per-row Vec alloc + fold (empty `rest` is a no-op anyway).
    let rest_raw = arithmetic_expr.rest();
    if rest_raw.is_empty() {
        return init;
    }
    let rest = rest_raw
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
            // Builtins take at most 3 args; evaluate operands on the stack.
            let vals: ArrayVec<i64, 4> = args.iter().map(|a| jn_factor(k, v1, v2, a)).collect();
            eval_builtin(*op, &vals)
        }
        FactorArgument::Paren(inner) => jn_arithmetic(k, v1, v2, inner),
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
    fn extract_number_first_integer() {
        let n = |s: &str| eval_builtin(BuiltinOp::ExtractNumber, &[intern(s)]);
        assert_eq!(n("a backlog of 47,500 rows"), 47500);
        assert_eq!(n("shipped 10,000 units by December 31, 2026"), 10000);
        assert_eq!(n("reach 120000 by 2027"), 120000);
        assert_eq!(n("7+ matches decided by penalties?"), 7);
        assert_eq!(n("trailing comma 1,x"), 1); // comma not followed by digit ends the run
        assert_eq!(n("no digits here"), NULL_SENTINEL);
        assert_eq!(n(""), NULL_SENTINEL);
    }

    #[test]
    fn conversions_round_trip() {
        let f = |v: i64| eval_builtin(BuiltinOp::ToFloat, &[v]);
        assert_eq!(f(6146468), (6146468.0_f64).to_bits() as i64);
        assert_eq!(
            eval_builtin(BuiltinOp::Round, &[(2.5_f64).to_bits() as i64]),
            3
        );
        assert_eq!(
            eval_builtin(BuiltinOp::Floor, &[(2.9_f64).to_bits() as i64]),
            2
        );
        assert_eq!(
            eval_builtin(BuiltinOp::Floor, &[(-0.5_f64).to_bits() as i64]),
            -1
        );
        assert_eq!(eval_builtin(BuiltinOp::Round, &[f(41)]), 41); // number -> float -> number
        assert_eq!(
            eval_builtin(BuiltinOp::ToFloat, &[NULL_SENTINEL]),
            NULL_SENTINEL
        );
    }

    #[test]
    fn case_folding() {
        let lower = eval_builtin(BuiltinOp::ToLower, &[intern("Read The README!")]);
        assert_eq!(decode(lower).unwrap().as_ref(), "read the readme!");
        let upper = eval_builtin(BuiltinOp::ToUpper, &[intern("some-crate")]);
        assert_eq!(decode(upper).unwrap().as_ref(), "SOME-CRATE");
    }

    #[test]
    fn float_math_builtins() {
        let f = |v: f64| v.to_bits() as i64;
        let as_f = |bits: i64| f64::from_bits(bits as u64);
        assert_eq!(as_f(eval_builtin(BuiltinOp::Exp, &[f(0.0)])), 1.0);
        assert_eq!(as_f(eval_builtin(BuiltinOp::Ln, &[f(1.0)])), 0.0);
        assert_eq!(as_f(eval_builtin(BuiltinOp::Sqrt, &[f(9.0)])), 3.0);
        assert_eq!(
            as_f(eval_builtin(BuiltinOp::Pow, &[f(2.0), f(10.0)])),
            1024.0
        );
        // round-trip: exp(ln(x)) = x
        let x = f(0.37);
        assert_eq!(
            as_f(eval_builtin(
                BuiltinOp::Exp,
                &[eval_builtin(BuiltinOp::Ln, &[x])]
            )),
            0.37
        );
        // Domain errors -> NULL; infinities survive.
        assert_eq!(eval_builtin(BuiltinOp::Ln, &[f(-1.0)]), NULL_SENTINEL);
        assert_eq!(eval_builtin(BuiltinOp::Sqrt, &[f(-4.0)]), NULL_SENTINEL);
        assert!(as_f(eval_builtin(BuiltinOp::Ln, &[f(0.0)])).is_infinite());
        assert_eq!(
            eval_builtin(BuiltinOp::Exp, &[NULL_SENTINEL]),
            NULL_SENTINEL
        );
    }

    #[test]
    fn abs_specializations() {
        assert_eq!(eval_builtin(BuiltinOp::AbsInt, &[-41]), 41);
        assert_eq!(eval_builtin(BuiltinOp::AbsInt, &[41]), 41);
        assert_eq!(eval_builtin(BuiltinOp::AbsInt, &[i64::MIN]), NULL_SENTINEL);
        let f = |v: f64| v.to_bits() as i64;
        assert_eq!(eval_builtin(BuiltinOp::AbsFloat, &[f(-2.5)]), f(2.5));
        // Unresolved generic op never survives typing; defensively NULL.
        assert_eq!(eval_builtin(BuiltinOp::Abs, &[-41]), NULL_SENTINEL);
    }

    #[test]
    fn similarity_scores() {
        let sim = |a: &str, b: &str| eval_builtin(BuiltinOp::Similarity, &[intern(a), intern(b)]);
        assert_eq!(sim("fed decision in july", "fed decision in july"), 100);
        assert_eq!(sim("abc", "xyz"), 0);
        assert!(
            sim(
                "will the fed cut rates in july?",
                "fed cut rates at the july meeting"
            ) > 60
        );
        assert!(
            sim(
                "will spain win the world cup?",
                "highest temperature in tokyo"
            ) < 30
        );
        // Multiset bigrams: repetition is not a free match.
        assert!(sim("aaaa", "aa") < 100);
        // Degenerate lengths compare by equality.
        assert_eq!(sim("a", "a"), 100);
        assert_eq!(sim("a", "b"), 0);
        assert_eq!(sim("", ""), 100);
        assert_eq!(
            eval_builtin(BuiltinOp::Similarity, &[NULL_SENTINEL, intern("x")]),
            NULL_SENTINEL
        );
    }

    #[test]
    fn date_epoch_parses_iso8601() {
        let e = |s: &str| eval_builtin(BuiltinOp::DateEpoch, &[intern(s)]);
        assert_eq!(e("1970-01-01"), 0);
        assert_eq!(e("1970-01-02T00:00:00Z"), 86400);
        assert_eq!(e("2026-07-20T00:00:00Z"), 1784505600);
        assert_eq!(
            e("2026-07-29T17:59:00Z"),
            e("2026-07-29") + 17 * 3600 + 59 * 60
        );
        assert_eq!(
            e("2026-07-02T22:11:38.30477Z"),
            e("2026-07-02") + 22 * 3600 + 11 * 60 + 38
        );
        assert_eq!(e("1969-12-31"), -86400); // pre-epoch dates work
        assert_eq!(e("not a date"), NULL_SENTINEL);
        assert_eq!(e("2026-13-01"), NULL_SENTINEL);
        assert_eq!(e("2026-07-29T17:59:00+02:00"), NULL_SENTINEL); // offsets rejected
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
        assert_eq!(
            call2(BuiltinOp::Concat, "crates/executing", "/"),
            "crates/executing/"
        );
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// The memo is invisible: repeated and interleaved calls agree with a
        /// fresh uncached evaluation for every memoized op.
        #[test]
        fn memoized_builtins_match_uncached(
            strings in proptest::collection::vec("[a-c/.]{0,8}", 1..6),
            n in 0i64..4,
        ) {
            let ids: Vec<i64> = strings.iter().map(|s| intern(s)).collect();
            let two = |op: BuiltinOp| {
                for &a in &ids {
                    for &b in &ids {
                        prop_assert_eq!(
                            eval_builtin(op, &[a, b]),
                            eval_builtin_uncached(op, &[a, b])
                        );
                        // Second (memo-hit) call must agree too.
                        prop_assert_eq!(
                            eval_builtin(op, &[a, b]),
                            eval_builtin_uncached(op, &[a, b])
                        );
                    }
                }
                Ok(())
            };
            two(BuiltinOp::BeforeLast)?;
            two(BuiltinOp::AfterLast)?;
            two(BuiltinOp::Concat)?;
            two(BuiltinOp::StartsWith)?;
            two(BuiltinOp::Contains)?;
            two(BuiltinOp::Replace)?;
            for &a in &ids {
                for &b in &ids {
                    prop_assert_eq!(
                        eval_builtin(BuiltinOp::SplitNth, &[a, b, n]),
                        eval_builtin_uncached(BuiltinOp::SplitNth, &[a, b, n])
                    );
                }
                prop_assert_eq!(
                    eval_builtin(BuiltinOp::ToLower, &[a]),
                    eval_builtin_uncached(BuiltinOp::ToLower, &[a])
                );
            }
        }
    }

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

#[cfg(test)]
mod bitwise_tests {
    use super::*;

    #[test]
    fn bitwise_ops_on_integers() {
        let and = ArithmeticOperator::BitAnd;
        let or = ArithmeticOperator::BitOr;
        let xor = ArithmeticOperator::BitXor;
        assert_eq!(arithmetic_ints(12, &[(&and, 10)]), 8);
        assert_eq!(arithmetic_ints(12, &[(&or, 10)]), 14);
        assert_eq!(arithmetic_ints(12, &[(&xor, 10)]), 6);
    }

    /// The capability test a provenance rewrite needs: every bit of `a` inside
    /// `b`, written as `a & b == a`.
    #[test]
    fn masking_expresses_subset() {
        let and = ArithmeticOperator::BitAnd;
        assert_eq!(arithmetic_ints(3, &[(&and, 7)]), 3, "3 is inside 7");
        assert_eq!(arithmetic_ints(8, &[(&and, 12)]), 8, "8 is inside 12");
        assert_ne!(
            arithmetic_ints(12, &[(&and, 10)]),
            12,
            "12 is not inside 10"
        );
    }

    /// Bitwise on a float has no meaning. Applying the operator to the IEEE bit
    /// pattern would yield a plausible number instead of an error, so the float
    /// path yields NULL rather than nonsense.
    #[test]
    fn bitwise_on_floats_is_null_rather_than_a_bit_pattern() {
        let or = ArithmeticOperator::BitOr;
        let got = arithmetic_values(
            1.5f64.to_bits() as i64,
            &[(&or, 2.5f64.to_bits() as i64)],
            &DataType::Float,
        );
        assert_eq!(got, NULL_SENTINEL);
    }
}

#[cfg(test)]
mod overflow_tests {
    use super::*;

    /// Arithmetic that leaves the i64 range has no answer, and says so.
    ///
    /// Before this was checked, `+`, `-` and `*` used the plain operators: a
    /// debug build panicked, and because this runs on a timely worker the panic
    /// took the worker down and left the dataflow unable to complete an epoch —
    /// the engine hung rather than failing. A release build wrapped instead and
    /// returned a plausible wrong number. Null is the same answer division by
    /// zero already gave.
    #[test]
    fn integer_overflow_is_null_rather_than_a_panic_or_a_wrapped_number() {
        let plus = ArithmeticOperator::Plus;
        let minus = ArithmeticOperator::Minus;
        let times = ArithmeticOperator::Multiply;

        assert_eq!(arithmetic_ints(i64::MAX, &[(&plus, 1)]), NULL_SENTINEL);
        assert_eq!(arithmetic_ints(i64::MIN, &[(&minus, 1)]), NULL_SENTINEL);
        assert_eq!(
            arithmetic_ints(10_000_000_000, &[(&times, 10_000_000_000)]),
            NULL_SENTINEL
        );
        // Wrapping would have produced these; none of them may appear.
        assert_ne!(arithmetic_ints(i64::MAX, &[(&plus, 1)]), i64::MIN + 1);
        assert_ne!(arithmetic_ints(i64::MIN, &[(&minus, 1)]), i64::MAX);
    }

    /// The one division that overflows: `i64::MIN / -1` has no i64 result, and
    /// is a hardware trap rather than a wrap, so a plain `/` aborts the process.
    #[test]
    fn the_overflowing_division_is_null_too() {
        let divide = ArithmeticOperator::Divide;
        let modulo = ArithmeticOperator::Modulo;
        assert_eq!(arithmetic_ints(i64::MIN, &[(&divide, -1)]), NULL_SENTINEL);
        assert_eq!(arithmetic_ints(i64::MIN, &[(&modulo, -1)]), NULL_SENTINEL);
        assert_eq!(arithmetic_ints(7, &[(&divide, 0)]), NULL_SENTINEL);
        assert_eq!(arithmetic_ints(7, &[(&modulo, 0)]), NULL_SENTINEL);
    }

    /// Overflow anywhere in a chain poisons the whole expression, so a later
    /// operation cannot bring a lost value back into range and hide it.
    #[test]
    fn an_overflow_partway_through_does_not_recover() {
        let plus = ArithmeticOperator::Plus;
        let minus = ArithmeticOperator::Minus;
        assert_eq!(
            arithmetic_ints(i64::MAX, &[(&plus, 1), (&minus, 1)]),
            NULL_SENTINEL
        );
    }

    /// Ordinary arithmetic is untouched — the check must not cost correctness
    /// on the values that do fit.
    #[test]
    fn arithmetic_in_range_is_unchanged() {
        let plus = ArithmeticOperator::Plus;
        let times = ArithmeticOperator::Multiply;
        assert_eq!(arithmetic_ints(2, &[(&plus, 3)]), 5);
        assert_eq!(arithmetic_ints(6, &[(&times, 7)]), 42);
        assert_eq!(arithmetic_values(2, &[(&plus, 3)], &DataType::Integer), 5);
        assert_eq!(
            arithmetic_values(i64::MAX, &[(&plus, 1)], &DataType::Integer),
            NULL_SENTINEL
        );
    }

    /// Floats saturate to infinity instead of overflowing, which is IEEE
    /// behaviour and stays as it was.
    #[test]
    fn float_arithmetic_still_goes_to_infinity() {
        let times = ArithmeticOperator::Multiply;
        let big = f64::MAX.to_bits() as i64;
        let result = arithmetic_values(big, &[(&times, big)], &DataType::Float);
        assert!(f64::from_bits(result as u64).is_infinite());
    }
}
