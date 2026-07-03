use crate::{aggregation::Aggregation, arithmetic::Arithmetic};
use std::fmt;

/// Represents different types of arguments that can appear in a head expression.
/// Head arguments can be simple variables, arithmetic expressions, or aggregation functions.
///
/// # Examples
/// - Variable: `x`, `name`, `id`
/// - Arithmetic: `x + y`, `price * 0.8`, `count - 1`
/// - Aggregation: `sum(x)`, `max(price)`, `count(id)`
#[derive(Debug, Clone)]
pub enum HeadArg {
    /// A simple variable name (e.g., `x`, `name`, `user_id`)
    Var(String),
    /// An arithmetic expression (e.g., `x + y`, `price * tax_rate`)
    Arith(Arithmetic),
    /// An aggregation function (e.g., `sum(sales)`, `max(score)`)
    Aggregation(Aggregation),
}

impl HeadArg {
    /// Returns all variable names referenced in this head argument.
    ///
    /// # Returns
    /// A vector of references to variable names used in the argument
    ///
    /// # Examples
    /// - `Var("x")` returns `vec!["x"]`
    /// - `Arith(x + y)` returns `vec!["x", "y"]`
    /// - `Aggregation(sum(price))` returns `vec!["price"]`
    pub fn vars(&self) -> Vec<&String> {
        match self {
            Self::Var(var) => vec![var],
            Self::Arith(arith) => arith.vars(),
            Self::Aggregation(aggregation) => aggregation.vars(),
        }
    }
}

impl fmt::Display for HeadArg {
    /// Formats the head argument for display by delegating to the underlying type's Display implementation.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Var(var) => write!(f, "{}", var),
            Self::Arith(arith) => write!(f, "{}", arith),
            Self::Aggregation(aggregation) => write!(f, "{}", aggregation),
        }
    }
}

/// Represents a head expression in a logic rule, consisting of a predicate name
/// and a list of arguments.
///
/// In logic programming, the head is the conclusion part of a rule.
///
/// # Examples
/// - `person(john, 25)` - predicate "person" with arguments "john" and "25"
/// - `salary(emp_id, sum(hours * rate))` - predicate with variable and aggregation
/// - `result(x + y)` - predicate with arithmetic expression
#[derive(Debug, Clone)]
pub struct Head {
    /// The name of the predicate (e.g., "person", "salary", "result")
    name: String,
    /// The list of arguments for this head expression
    head_arguments: Vec<HeadArg>,
}

impl fmt::Display for Head {
    /// Formats the head as "predicate_name(arg1, arg2, ...)".
    ///
    /// # Examples
    /// - `person(john, 25)`
    /// - `result(x + y, sum(z))`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let head_arguments = self
            .head_arguments
            .iter()
            .map(|head_arg| head_arg.to_string())
            .collect::<Vec<String>>()
            .join(", ");

        write!(f, "{}({})", self.name, head_arguments)
    }
}

impl Head {
    /// Creates a new Head with the given predicate name and arguments.
    ///
    /// # Arguments
    /// * `name` - The predicate name
    /// * `head_arguments` - Vector of head arguments
    ///
    /// # Returns
    /// A new Head instance
    pub fn new(name: String, head_arguments: Vec<HeadArg>) -> Self {
        Self {
            name,
            head_arguments,
        }
    }

    /// Returns a reference to the predicate name.
    ///
    /// # Returns
    /// A reference to the predicate name string
    pub fn name(&self) -> &String {
        &self.name
    }

    /// Returns a reference to the list of head arguments.
    ///
    /// # Returns
    /// A reference to the vector of HeadArg instances
    pub fn head_arguments(&self) -> &Vec<HeadArg> {
        &self.head_arguments
    }

    /// Mutable access to the head arguments (used by the typing pass).
    pub fn head_arguments_mut(&mut self) -> &mut Vec<HeadArg> {
        &mut self.head_arguments
    }

    /// Returns the arity (number of arguments) of this head expression.
    ///
    /// # Returns
    /// The number of arguments in this head
    ///
    /// # Examples
    /// - `person(john, 25)` has arity 2
    /// - `result(x)` has arity 1
    /// - `empty()` has arity 0
    pub fn arity(&self) -> usize {
        self.head_arguments.len()
    }
}
