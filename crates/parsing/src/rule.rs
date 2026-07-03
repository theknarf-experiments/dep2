use crate::compare::ComparisonExpr;
use crate::head::Head;
use std::fmt;

/*
    Atom: NAME(AtomArg, AtomArg, ...)
    AtomArg: Var(String) | Const(Const) | Placeholder
    Const: Integer(i32) | Text(String)
*/

// atom_arg = var | const | placeholder
#[derive(Debug, Clone)]
pub enum AtomArg {
    Var(String),
    Const(Const),
    Placeholder,
}

impl AtomArg {
    pub fn is_var(&self) -> bool {
        matches!(self, Self::Var(_))
    }

    pub fn is_const(&self) -> bool {
        matches!(self, Self::Const(_))
    }

    pub fn is_placeholder(&self) -> bool {
        matches!(self, Self::Placeholder)
    }

    pub fn as_var(&self) -> &String {
        match self {
            Self::Var(var) => var,
            _ => panic!("expects var: {:?}", self),
        }
    }
}

impl fmt::Display for AtomArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Var(var) => write!(f, "{}", var),
            Self::Const(constant) => write!(f, "{}", constant),
            Self::Placeholder => write!(f, "_"),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum Const {
    Integer(i64),
    Text(String),
    /// A float constant stored as its IEEE 754 bit pattern (via `f64::to_bits() as i64`).
    Float(i64),
}

impl Const {
    pub fn integer(&self) -> i64 {
        match self {
            Self::Integer(int) => *int,
            _ => panic!("expects ints: {:?}", self),
        }
    }

    /// Return the i64 representation regardless of variant.
    /// Integer and Float both store i64 directly.
    pub fn as_i64(&self) -> i64 {
        match self {
            Self::Integer(int) => *int,
            Self::Float(bits) => *bits,
            Self::Text(_) => panic!("as_i64 on Text constant: {:?}", self),
        }
    }
}

impl fmt::Display for Const {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(int) => write!(f, "{}", int),
            Self::Text(text) => write!(f, "{}", text),
            Self::Float(bits) => {
                let val = f64::from_bits(*bits as u64);
                write!(f, "{}", val)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Atom {
    name: String,
    arguments: Vec<AtomArg>,
}

impl fmt::Display for Atom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}({})",
            self.name,
            self.arguments
                .iter()
                .map(|arg| arg.to_string())
                .collect::<Vec<String>>()
                .join(", ")
        )
    }
}

impl Atom {
    pub fn from_str(name: &str, arguments: Vec<AtomArg>) -> Self {
        Self {
            name: name.to_string(),
            arguments,
        }
    }

    pub fn push_arg(&mut self, arg: AtomArg) {
        self.arguments.push(arg);
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn arguments(&self) -> &Vec<AtomArg> {
        &self.arguments
    }

    /// Mutable access for AST transforms (literal interning).
    pub fn arguments_mut(&mut self) -> &mut Vec<AtomArg> {
        &mut self.arguments
    }

    pub fn arity(&self) -> usize {
        self.arguments.len()
    }
}

/*
    FLRule: <Head> :- <Predicate>, <Predicate>, ...
    Predicate: <Atom> | !<Atom> | <Comparison>
*/

#[derive(Debug, Clone)]
pub enum Predicate {
    AtomPredicate(Atom),
    NegatedAtomPredicate(Atom),
    ComparePredicate(ComparisonExpr),
}

impl Predicate {
    pub fn arguments(&self) -> Vec<&AtomArg> {
        match self {
            Self::AtomPredicate(atom) => atom.arguments().iter().collect(),
            Self::NegatedAtomPredicate(atom) => atom.arguments().iter().collect(),
            Self::ComparePredicate(_) => panic!("Predicate.arguments() on cmpr"),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::AtomPredicate(atom) => atom.name(),
            Self::NegatedAtomPredicate(atom) => atom.name(),
            Self::ComparePredicate(_) => panic!("Predicate.name() on cmpr"),
        }
    }
}

impl fmt::Display for Predicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AtomPredicate(atom) => write!(f, "{}", atom),
            Self::NegatedAtomPredicate(atom) => write!(f, "!{}", atom),
            Self::ComparePredicate(expr) => write!(f, "{}", expr),
        }
    }
}

/*
    FLRule: <Head> :- <Predicate>, <Predicate>, ...
*/
#[derive(Debug, Clone)]
pub struct FLRule {
    head: Head,
    rhs: Vec<Predicate>,
    is_planning: bool,
    is_sip: bool,
}

impl fmt::Display for FLRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} :- {}.",
            self.head,
            self.rhs
                .iter()
                .map(|pred| pred.to_string())
                .collect::<Vec<String>>()
                .join(", ")
        )
    }
}

impl FLRule {
    pub fn new(head: Head, rhs: Vec<Predicate>, is_planning: bool, is_sip: bool) -> Self {
        Self {
            head,
            rhs,
            is_planning,
            is_sip,
        }
    }

    pub fn head(&self) -> &Head {
        &self.head
    }

    pub fn head_mut(&mut self) -> &mut Head {
        &mut self.head
    }

    pub fn rhs(&self) -> &Vec<Predicate> {
        &self.rhs
    }

    pub fn rhs_mut(&mut self) -> &mut Vec<Predicate> {
        &mut self.rhs
    }

    pub fn is_planning(&self) -> bool {
        self.is_planning
    }

    pub fn is_sip(&self) -> bool {
        self.is_sip
    }

    pub fn get(&self, i: usize) -> &Predicate {
        &self.rhs[i]
    }
}
