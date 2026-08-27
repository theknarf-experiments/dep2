/*
    DataType: number | string
    Attribute: <name>: <DataType>
    RelDecl: <name>(<Attribute>, <Attribute>, ...)
*/

use crate::aggregation::AggregationOperator;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Integer,
    String,
    Float,
}

/// Sentinel value representing NULL.
///
/// `i64::MIN` is unreachable by the string table (ids start at 0), but it is
/// NOT unreachable by the other two column types, and both directions are part
/// of the engine's contract:
///
/// * `number` — the integer domain is every `i64` EXCEPT this one. A value that
///   lands on it reads as NULL: arithmetic reports it like an overflow, a fact
///   or plugin row holding it loads as NULL, and a program that writes the
///   literal is rejected at parse time rather than silently meaning NULL.
/// * `float` — `-0.0` has exactly this bit pattern, so it is canonicalized to
///   `+0.0` by [`encode_float`] on every path that stores a float. Nothing is
///   lost (IEEE says `-0.0 == 0.0`) and no float is ever unrepresentable.
///
/// The float case is fully repaired by that canonicalization; the integer one
/// is a real hole of one value, and closing it properly means carrying validity
/// out of band instead of in the value — a different engine.
pub const NULL_SENTINEL: i64 = i64::MIN;

/// Check whether a value is the null sentinel.
pub fn is_null(v: i64) -> bool {
    v == NULL_SENTINEL
}

/// Encode an `f64` as the `i64` the engine stores (its IEEE-754 bit pattern).
///
/// The only value transformed is `-0.0`, whose bit pattern IS [`NULL_SENTINEL`];
/// it becomes `+0.0`. This is not tidiness. Rows are joined, grouped and
/// deduplicated on their raw `i64` bits, so leaving both zeroes distinct would
/// hand two values that IEEE calls equal two different join keys — and leaving
/// `-0.0` unmapped would additionally let a legitimate result be read
/// back as NULL, which is how `to_float(X) * 0.0 + 5.0` came to silently drop
/// every row with a negative `X`.
///
/// Every path that turns an `f64` into a stored value goes through here:
/// program literals, fact and CSV tokens, plugin rows, arithmetic results and
/// aggregations. A float result that reaches storage another way is a bug.
pub fn encode_float(f: f64) -> i64 {
    let bits = f.to_bits() as i64;
    if bits == NULL_SENTINEL {
        // `+0.0`.
        0
    } else {
        bits
    }
}

impl DataType {
    pub fn parse_from(type_str: &str) -> Self {
        match type_str {
            "number" => Self::Integer,
            "string" => Self::String,
            "float" => Self::Float,
            _ => unreachable!(),
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer => write!(f, "number"), // f :: a formatter that can be used to write to a buffer
            Self::String => write!(f, "string"),
            Self::Float => write!(f, "float"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Attribute {
    name: String,
    data_type: DataType,
}

impl fmt::Display for Attribute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.name, self.data_type)
    }
}

impl Attribute {
    pub fn new(name: &str, data_type: DataType) -> Self {
        Self {
            name: name.to_string(),
            data_type,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn data_type(&self) -> &DataType {
        &self.data_type
    }
}

#[derive(Debug, Clone)]
pub struct RelDecl {
    name: String,
    attributes: Vec<Attribute>,
    path: Option<String>,
    /// Declared under `.out` (force-serve over the query API even if the relation
    /// is consumed by another rule). `.printsize` relations default to false.
    force_serve: bool,
    /// Presentation ordering for served output: (column index, descending).
    /// Empty = engine default (lexicographic display order). Shapes only how
    /// rows are SERVED/PRINTED — the relation itself stays an unordered set.
    order_by: Vec<(usize, bool)>,
    /// Presentation row cap for served output, applied after ordering.
    limit: Option<usize>,
    /// Lattice merge (egglog's `:merge`). The relation is a FUNCTION from its
    /// leading columns (the key) to its last column (a lattice value): every
    /// rule deriving into it contributes a candidate value, and the candidates
    /// are folded with this operator instead of being kept as distinct rows.
    ///
    /// Restricted to `min`/`max` — the lattice joins. Only an idempotent,
    /// associative, commutative fold is monotone in the lattice order, which
    /// is what lets the merge run INSIDE a recursive fixpoint and still
    /// converge. `sum`/`count`/`avg` are not idempotent and stay head
    /// aggregations (which the stratifier splits out of recursion).
    merge: Option<AggregationOperator>,
}

impl fmt::Display for RelDecl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}({})",
            self.name,
            self.attributes
                .iter()
                .map(|attr| attr.to_string()) // to_string() uses the Display impl for Attribute
                .collect::<Vec<String>>()
                .join(", ")
        )?;
        if let Some(ref path) = self.path {
            write!(f, " read as {}", path)?;
        }
        Ok(())
    }
}

impl RelDecl {
    pub fn new(name: &str, attributes: Vec<Attribute>, path: Option<&str>) -> Self {
        Self {
            name: name.to_string(),
            attributes,
            path: path.map(|p| p.to_string()),
            force_serve: false,
            order_by: Vec::new(),
            limit: None,
            merge: None,
        }
    }

    /// Presentation shaping for served/printed output (see the field docs).
    pub fn set_output_shape(&mut self, order_by: Vec<(usize, bool)>, limit: Option<usize>) {
        self.order_by = order_by;
        self.limit = limit;
    }

    /// Presentation ordering: (column index, descending), empty = default.
    pub fn order_by(&self) -> &[(usize, bool)] {
        &self.order_by
    }

    /// Presentation row cap, applied after ordering.
    pub fn limit(&self) -> Option<usize> {
        self.limit
    }

    /// Lattice merge operator for this relation's value column (see the field
    /// docs); `None` = an ordinary set-valued relation.
    pub fn merge(&self) -> Option<AggregationOperator> {
        self.merge
    }

    pub fn set_merge(&mut self, merge: Option<AggregationOperator>) {
        self.merge = merge;
    }

    pub fn push_attr(&mut self, attr: Attribute) {
        self.attributes.push(attr);
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn attributes(&self) -> &[Attribute] {
        &self.attributes
    }

    pub fn arity(&self) -> usize {
        self.attributes.len()
    }

    pub fn path(&self) -> Option<String> {
        self.path.clone()
    }

    pub fn force_serve(&self) -> bool {
        self.force_serve
    }

    pub fn set_force_serve(&mut self, force_serve: bool) {
        self.force_serve = force_serve;
    }
}

#[cfg(test)]
mod null_domain_tests {
    use super::*;

    /// The whole point of `encode_float`: no float, however it arose, may come
    /// back out as NULL.
    #[test]
    fn no_float_encodes_to_the_null_sentinel() {
        for f in [
            -0.0f64,
            0.0,
            -1.0,
            f64::MIN,
            f64::MAX,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN_POSITIVE,
            -f64::MIN_POSITIVE,
            f64::from_bits(1), // smallest subnormal
            -5e-324,
        ] {
            assert!(!is_null(encode_float(f)), "{f} encoded to NULL");
        }
        // NaN is the one f64 the engine deliberately refuses, but it is
        // rejected by the callers (as NULL), not silently encoded to it.
        assert!(!is_null(encode_float(f64::NAN)));
    }

    /// Both zeroes must share one encoding: rows join on these bits, and IEEE
    /// says the two values are equal.
    #[test]
    fn the_two_zeroes_share_an_encoding() {
        assert_eq!(encode_float(-0.0), encode_float(0.0));
        assert_eq!(encode_float(-0.0), 0);
        assert_eq!(f64::from_bits(encode_float(-0.0) as u64), 0.0);
        // and the canonical form is the positive one, so it decodes cleanly
        assert!(f64::from_bits(encode_float(-0.0) as u64).is_sign_positive());
    }

    /// Everything else is stored verbatim — the encoder is not allowed to
    /// perturb values that do not collide (the old `NULL_SENTINEL + 1` nudge
    /// returned a different number).
    #[test]
    fn every_other_float_round_trips_exactly() {
        for f in [1.5f64, -1.5, 1e308, -1e-308, 0.1 + 0.2, f64::MAX] {
            assert_eq!(f64::from_bits(encode_float(f) as u64), f);
        }
    }

    /// The integer hole, pinned so it is a documented contract rather than a
    /// surprise: `i64::MIN` is not a number, and everything else is.
    #[test]
    fn the_integer_domain_excludes_only_the_sentinel() {
        assert!(is_null(i64::MIN));
        for v in [i64::MIN + 1, -1, 0, 1, i64::MAX] {
            assert!(!is_null(v), "{v} should be a representable number");
        }
    }
}
