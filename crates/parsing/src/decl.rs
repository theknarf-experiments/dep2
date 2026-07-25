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

/// Sentinel value representing NULL. Uses `i64::MIN` which is unreachable by
/// the string table (starts at 0) and for floats decodes to -0.0 (remapped at encoding).
pub const NULL_SENTINEL: i64 = i64::MIN;

/// Check whether a value is the null sentinel.
pub fn is_null(v: i64) -> bool {
    v == NULL_SENTINEL
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
