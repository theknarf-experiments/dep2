use crate::arithmetic::Arithmetic;
use crate::decl::DataType;
use std::fmt;

/// Represents the different types of aggregation operations that can be performed
/// on data sets (e.g., finding minimum, maximum, count, or sum of values).
#[derive(Debug, Clone, Eq, Hash, PartialEq, Copy)]
pub enum AggregationOperator {
    /// Find the minimum value in a dataset
    Min,
    /// Find the maximum value in a dataset
    Max,
    /// Count the number of items in a dataset
    Count,
    /// Calculate the sum of all values in a dataset
    Sum,
    /// Arithmetic mean of the values (integer mode truncates toward zero)
    Avg,
}

impl fmt::Display for AggregationOperator {
    /// Formats the aggregation operator as a lowercase string for display purposes.
    /// This is useful for generating human-readable output or SQL-like syntax.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AggregationOperator::Min => write!(f, "min"),
            AggregationOperator::Max => write!(f, "max"),
            AggregationOperator::Count => write!(f, "count"),
            AggregationOperator::Sum => write!(f, "sum"),
            AggregationOperator::Avg => write!(f, "avg"),
        }
    }
}

/// Represents a complete aggregation expression consisting of an operator
/// and the arithmetic expression it operates on.
///
/// Examples: `sum(x + y)`, `max(price * quantity)`, `count(id)`
#[derive(Debug, Clone)]
pub struct Aggregation {
    /// The aggregation operation to perform (min, max, count, sum)
    operator: AggregationOperator,
    /// The arithmetic expression to aggregate over
    arithmetic: Arithmetic,
    /// The data type of the values being aggregated
    data_type: DataType,
}

impl fmt::Display for Aggregation {
    /// Formats the aggregation as "operator(arithmetic_expression)".
    ///
    /// # Examples
    /// - `sum(x + y)`
    /// - `max(price * 0.8)`
    /// - `count(user_id)`
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}({})", self.operator, self.arithmetic)
    }
}

impl Aggregation {
    /// Creates a new `Aggregation` with the given operator and arithmetic expression.
    /// Defaults to `DataType::Integer`.
    pub fn new(operator: AggregationOperator, arithmetic: Arithmetic) -> Self {
        Self {
            operator,
            arithmetic,
            data_type: DataType::Integer,
        }
    }

    /// Creates a new `Aggregation` with an explicit data type.
    pub fn with_type(
        operator: AggregationOperator,
        arithmetic: Arithmetic,
        data_type: DataType,
    ) -> Self {
        Self {
            operator,
            arithmetic,
            data_type,
        }
    }

    /// Returns the data type of the values being aggregated.
    pub fn data_type(&self) -> &DataType {
        &self.data_type
    }

    /// Set the data type of the values being aggregated (typing pass). The
    /// aggregated expression's own mode is typed separately.
    pub fn set_data_type(&mut self, data_type: DataType) {
        self.data_type = data_type;
    }

    /// Mutable access for the typing pass's recursive walk.
    pub fn arithmetic_mut(&mut self) -> &mut Arithmetic {
        &mut self.arithmetic
    }

    /// Returns a vector of references to all variable names used in the arithmetic expression.
    /// This is useful for dependency analysis and query planning.
    ///
    /// # Returns
    /// A vector containing references to all variable names in the arithmetic expression
    pub fn vars(&self) -> Vec<&String> {
        self.arithmetic.vars()
    }

    /// Returns a reference to the arithmetic expression being aggregated.
    ///
    /// # Returns
    /// A reference to the internal `Arithmetic` expression
    pub fn arithmetic(&self) -> &Arithmetic {
        &self.arithmetic
    }

    /// Returns a reference to the aggregation operator.
    ///
    /// # Returns
    /// A reference to the `AggregationOperator` (min, max, count, or sum)
    pub fn operator(&self) -> &AggregationOperator {
        &self.operator
    }
}
