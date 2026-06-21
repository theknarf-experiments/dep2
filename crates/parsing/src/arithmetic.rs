use crate::decl::DataType;
use crate::{parser::Lexeme, rule::Const, Rule};
use pest::iterators::Pair;
use std::collections::HashSet;
use std::fmt;

/** arithmic ops **/
// arithmic_op = { plus | minus | times | divide }
// plus = { "+" }
// minus = { "-" }
// times = { "*" }
// divide = { "/" }
// modulo = { "%" }
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum ArithmeticOperator {
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,
}

impl fmt::Display for ArithmeticOperator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ArithmeticOperator::Plus => {
                write!(f, "+")
            }
            ArithmeticOperator::Minus => {
                write!(f, "-")
            }
            ArithmeticOperator::Multiply => {
                write!(f, "*")
            }
            ArithmeticOperator::Divide => {
                write!(f, "/")
            }
            ArithmeticOperator::Modulo => {
                write!(f, "%")
            }
        }
    }
}

impl Lexeme for ArithmeticOperator {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self {
        let operator = parsed_rule.into_inner().next().unwrap();
        match operator.as_rule() {
            Rule::plus => ArithmeticOperator::Plus,
            Rule::minus => ArithmeticOperator::Minus,
            Rule::times => ArithmeticOperator::Multiply,
            Rule::divide => ArithmeticOperator::Divide,
            Rule::modulo => ArithmeticOperator::Modulo,
            _ => unreachable!(),
        }
    }
}

/// A value-producing string builtin. `SplitNth(s, sep, n)` returns the n-th
/// `sep`-separated segment of `s` (as a string); the boolean builtins return
/// `1`/`0` and are meant to be used as `f(..) = 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinOp {
    /// `split_nth(s, sep, n)` -> n-th segment of `s` split by `sep`.
    SplitNth,
    /// `starts_with(s, prefix)` -> 1 if `s` starts with `prefix`.
    StartsWith,
    /// `contains(s, needle)` -> 1 if `s` contains `needle`.
    Contains,
    /// `str_before(a, b)` -> 1 if `a` sorts lexicographically before `b`.
    StrBefore,
    /// `replace(s, from, to)` -> `s` with every `from` replaced by `to`.
    Replace,
    /// `before_last(s, sep)` -> the part of `s` before its last `sep` (all of `s`
    /// if `sep` is absent). E.g. dirname: `before_last("a/b/c", "/")` -> `"a/b"`.
    BeforeLast,
    /// `after_last(s, sep)` -> the part of `s` after its last `sep` (all of `s` if
    /// `sep` is absent). E.g. basename: `after_last("a/b/c", "/")` -> `"c"`.
    AfterLast,
}

impl BuiltinOp {
    pub fn from_name(name: &str) -> Self {
        match name {
            "split_nth" => Self::SplitNth,
            "starts_with" => Self::StartsWith,
            "contains" => Self::Contains,
            "str_before" => Self::StrBefore,
            "replace" => Self::Replace,
            "before_last" => Self::BeforeLast,
            "after_last" => Self::AfterLast,
            _ => unreachable!("unknown builtin: {name}"),
        }
    }
}

impl fmt::Display for BuiltinOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::SplitNth => "split_nth",
            Self::StartsWith => "starts_with",
            Self::Contains => "contains",
            Self::StrBefore => "str_before",
            Self::Replace => "replace",
            Self::BeforeLast => "before_last",
            Self::AfterLast => "after_last",
        };
        write!(f, "{}", s)
    }
}

// factor = { builtin_call | variable | constant }
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Factor {
    Var(String),
    Const(Const),
    /// A builtin call, e.g. `split_nth(Path, "/", 0)`.
    Builtin(BuiltinOp, Vec<Factor>),
}

impl Factor {
    pub fn is_var(&self) -> bool {
        matches!(self, Self::Var(_))
    }

    pub fn vars_set(&self) -> HashSet<&String> {
        self.vars().into_iter().collect()
    }

    /// Variables referenced by this factor, left-to-right. Builtin args recurse in
    /// order so this matches the lowering walk in catalog/planning.
    pub fn vars(&self) -> Vec<&String> {
        match self {
            Self::Var(var) => vec![var],
            Self::Const(_) => vec![],
            Self::Builtin(_, args) => args.iter().flat_map(|a| a.vars()).collect(),
        }
    }
}

impl fmt::Display for Factor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Factor::Var(var) => write!(f, "{}", var),
            Factor::Const(constant) => write!(f, "{}", constant),
            Factor::Builtin(op, args) => {
                let args = args
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{}({})", op, args)
            }
        }
    }
}

impl Lexeme for Factor {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self {
        let inner = parsed_rule.into_inner().next().unwrap();
        match inner.as_rule() {
            Rule::variable => Self::Var(inner.as_str().to_string()), // to_string() copies the string
            Rule::constant => Self::Const(Const::from_parsed_rule(inner)),
            Rule::builtin_call => {
                let mut parts = inner.into_inner();
                let op = BuiltinOp::from_name(parts.next().unwrap().as_str());
                let args = parts.map(Factor::from_parsed_rule).collect();
                Self::Builtin(op, args)
            }
            _ => unreachable!(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Arithmetic {
    init: Factor,
    rest: Vec<(ArithmeticOperator, Factor)>,
    data_type: DataType,
}

impl Arithmetic {
    pub fn new(init: Factor, rest: Vec<(ArithmeticOperator, Factor)>) -> Self {
        Self {
            init,
            rest,
            data_type: DataType::Integer,
        }
    }

    pub fn with_type(
        init: Factor,
        rest: Vec<(ArithmeticOperator, Factor)>,
        data_type: DataType,
    ) -> Self {
        Self {
            init,
            rest,
            data_type,
        }
    }

    pub fn init(&self) -> &Factor {
        &self.init
    }

    pub fn rest(&self) -> &[(ArithmeticOperator, Factor)] {
        &self.rest
    }

    pub fn data_type(&self) -> &DataType {
        &self.data_type
    }

    pub fn vars_set(&self) -> HashSet<&String> {
        self.vars().into_iter().collect()
    }

    pub fn vars(&self) -> Vec<&String> {
        let mut vec = self.init.vars();
        for (_, factor) in &self.rest {
            vec.extend(factor.vars_set().into_iter());
        }
        vec
    }

    // if it is a simple variable
    pub fn is_var(&self) -> bool {
        self.init.is_var() && self.rest.is_empty()
    }
}

impl fmt::Display for Arithmetic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let init = self.init.to_string();
        let rest = self
            .rest
            .iter()
            .map(|(op, factor)| format!("{} {}", op, factor))
            .collect::<Vec<String>>()
            .join(" ");

        if rest.is_empty() {
            write!(f, "{}", init)
        } else {
            write!(f, "{} {}", init, rest)
        }
    }
}

impl Lexeme for Arithmetic {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self {
        let mut inner_rules = parsed_rule.into_inner();
        let init = Factor::from_parsed_rule(inner_rules.next().unwrap());

        // consume every two next() calls as a pair (op, factor) until there is no more next()
        let mut rest = Vec::new();
        while let Some(op) = inner_rules.next() {
            let factor = Factor::from_parsed_rule(inner_rules.next().unwrap());
            rest.push((ArithmeticOperator::from_parsed_rule(op), factor));
        }

        Self {
            init,
            rest,
            data_type: DataType::Integer,
        }
    }
}
