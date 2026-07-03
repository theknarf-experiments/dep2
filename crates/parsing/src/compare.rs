use crate::arithmetic::Arithmetic;
use std::collections::HashSet;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ComparisonOperator {
    Equals,
    NotEquals,
    GreaterThan,
    GreaterEqualThan,
    LessThan,
    LessEqualThan,
}

impl ComparisonOperator {
    pub fn is_equals(&self) -> bool {
        matches!(self, Self::Equals)
    }
}

impl fmt::Display for ComparisonOperator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ComparisonOperator::Equals => {
                write!(f, "==")
            }
            ComparisonOperator::NotEquals => {
                write!(f, "≠")
            }
            ComparisonOperator::GreaterThan => {
                write!(f, ">")
            }
            ComparisonOperator::GreaterEqualThan => {
                write!(f, "≥")
            }
            ComparisonOperator::LessThan => {
                write!(f, "<")
            }
            ComparisonOperator::LessEqualThan => {
                write!(f, "≤")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ComparisonExpr {
    left: Arithmetic,
    operator: ComparisonOperator,
    right: Arithmetic,
}

impl ComparisonExpr {
    pub fn new(left: Arithmetic, operator: ComparisonOperator, right: Arithmetic) -> Self {
        Self {
            left,
            operator,
            right,
        }
    }

    pub fn left(&self) -> &Arithmetic {
        &self.left
    }

    pub fn operator(&self) -> &ComparisonOperator {
        &self.operator
    }

    pub fn right(&self) -> &Arithmetic {
        &self.right
    }

    /// Mutable access for the typing pass (each side is typed independently,
    /// then the modes are checked to agree).
    pub fn left_mut(&mut self) -> &mut Arithmetic {
        &mut self.left
    }

    /// Mutable access for the typing pass.
    pub fn right_mut(&mut self) -> &mut Arithmetic {
        &mut self.right
    }

    pub fn vars_set(&self) -> HashSet<&String> {
        self.left
            .vars_set()
            .union(&self.right.vars_set())
            .cloned()
            .collect()
    }

    pub fn left_vars(&self) -> Vec<&String> {
        self.left.vars()
    }

    pub fn right_vars(&self) -> Vec<&String> {
        self.right.vars()
    }
}

impl fmt::Display for ComparisonExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{} {} {}]", self.left, self.operator, self.right)
    }
}
