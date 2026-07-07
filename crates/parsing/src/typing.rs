//! Decl-driven typing and validation pass.
//!
//! The grammar types nothing: every `Arithmetic` parses with `data_type =
//! Integer`, so float columns would be compared and computed on as raw IEEE-754
//! bit patterns. This pass runs once per program, after parsing. It does two
//! jobs:
//!
//! **Typing.** Every arithmetic chain — including each parenthesised
//! sub-expression and builtin argument, independently — gets an evaluation
//! mode (Integer or Float) unified from its factors: declared column types,
//! literal types, builtin result types. Mixing float with number in one chain
//! is an error; the conversion builtins `to_float(n)` and `round(f)`/`floor(f)`
//! are the explicit bridges. Comparisons must agree across their two sides;
//! head expressions and aggregations must agree with the head column they
//! produce (`count` is always Integer). `string` operands take Integer mode
//! (interned ids: equality is exact, ordering meaningless — `str_before`
//! compares text).
//!
//! **Validation.** Mistakes that used to surface as cryptic panics deep in
//! planning (or as silently-empty relations) are reported here with the
//! offending rule:
//!   - an atom whose arity differs from its declaration;
//!   - a head variable not bound by any positive body atom (unsafe rule) —
//!     ditto variables used only in negated atoms or comparisons;
//!   - one variable bound to columns of different types (a string/number join
//!     never matches: string ids and numbers share no values);
//!   - conversion builtins applied to the wrong kind (`to_float` takes a
//!     number, `round`/`floor` take a float).
//!
//! Relations with no declaration are left alone (the engine already warns).

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::aggregation::AggregationOperator;
use crate::arithmetic::{Arithmetic, BuiltinOp, Factor};
use crate::decl::{DataType, RelDecl};
use crate::head::HeadArg;
use crate::rule::{Atom, AtomArg, FLRule, Predicate};

/// What an expression works over, before picking an evaluation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueKind {
    Int,
    Float,
    /// Interned string id — evaluates in Integer mode.
    Str,
}

impl ValueKind {
    fn mode(self) -> DataType {
        match self {
            ValueKind::Float => DataType::Float,
            ValueKind::Int | ValueKind::Str => DataType::Integer,
        }
    }

    fn of_decl(dt: DataType) -> Self {
        match dt {
            DataType::Integer => ValueKind::Int,
            DataType::Float => ValueKind::Float,
            DataType::String => ValueKind::Str,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            ValueKind::Int => "number",
            ValueKind::Float => "float",
            ValueKind::Str => "string",
        }
    }
}

/// A typing/validation error, carrying which rule it came from so span-aware
/// front-ends (the `syntax` crate) can point at it in the source.
#[derive(Debug, Clone)]
pub struct TypeError {
    /// Index into the program's rules (`None` for program-level errors).
    pub rule: Option<usize>,
    pub message: String,
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Shorthand: build a `TypeError` for rule `i` from a format string.
macro_rules! type_error {
    ($i:expr, $($arg:tt)*) => {
        return Err(TypeError { rule: Some($i), message: format!($($arg)*) })
    };
}

/// Type and validate `rules` against the relation declarations, reporting the
/// first error with the offending rule. `Program::new` panics on `Err` (the
/// historical behavior); `Program::try_new` surfaces it.
pub fn resolve_types(
    edbs: &[RelDecl],
    idbs: &[RelDecl],
    rules: &mut [FLRule],
) -> Result<(), TypeError> {
    let decls: HashMap<&str, &RelDecl> = edbs
        .iter()
        .chain(idbs.iter())
        .map(|d| (d.name(), d))
        .collect();
    // Undeclared-atom errors only apply to programs that declare relations at
    // all: programmatically-built test programs (e.g. strata's stratification
    // property tests) carry no decls and keep their pre-validation behavior.
    // Undeclared HEADS are fine (they define intermediate relations); a body
    // atom must be declared or defined by some rule.
    let strict = !decls.is_empty();
    let defined: HashSet<String> = rules.iter().map(|r| r.head().name().to_string()).collect();
    for (i, rule) in rules.iter_mut().enumerate() {
        resolve_rule(i, rule, &decls, strict, &defined)?;
    }
    Ok(())
}

fn resolve_rule(
    i: usize,
    rule: &mut FLRule,
    decls: &HashMap<&str, &RelDecl>,
    strict: bool,
    defined: &HashSet<String>,
) -> Result<(), TypeError> {
    let context = rule.to_string();

    // Every body atom must name a declared relation or one defined by some
    // rule's head. Without this, an unknown atom (e.g. a builtin used bare
    // instead of as a comparison) sails through typing and PANICS at dataflow
    // assembly — fatal for a program added to a running engine.
    if strict {
        for pred in rule.rhs() {
            let atom = match pred {
                Predicate::AtomPredicate(atom) => atom,
                Predicate::NegatedAtomPredicate(atom) => atom,
                Predicate::ComparePredicate(_) => continue,
            };
            if !decls.contains_key(atom.name()) && !defined.contains(atom.name()) {
                type_error!(
                    i,
                    "unknown relation {}: it is not declared and no rule derives it \
                     (if you meant a builtin filter, write it as a comparison, \
                     e.g. {}(...) = 1). In rule: {}",
                    atom.name(),
                    atom.name(),
                    context
                );
            }
        }
    }

    // --- collect body bindings, checking arity and type agreement ---------
    let mut var_kinds: HashMap<String, ValueKind> = HashMap::new();
    let mut positive_vars: HashSet<String> = HashSet::new();
    for pred in rule.rhs() {
        let (atom, positive) = match pred {
            Predicate::AtomPredicate(atom) => (atom, true),
            Predicate::NegatedAtomPredicate(atom) => (atom, false),
            Predicate::ComparePredicate(_) => continue,
        };
        bind_atom(
            i,
            atom,
            positive,
            decls,
            &mut var_kinds,
            &mut positive_vars,
            &context,
        )?;
    }

    // --- safety: every used variable must come from a positive atom -------
    // Only checked when the rule has a positive atom at all: dependency stubs
    // like `a(X) :- !b(X).` or empty bodies exist in programmatically-built
    // test programs (strata's stratification property tests) and keep their
    // pre-validation behavior.
    let synthetic_stub = positive_vars.is_empty()
        && !rule
            .rhs()
            .iter()
            .any(|p| matches!(p, Predicate::AtomPredicate(_)));
    if synthetic_stub {
        return Ok(());
    }
    for pred in rule.rhs() {
        let unbound: Vec<&String> = match pred {
            Predicate::AtomPredicate(_) => continue,
            Predicate::NegatedAtomPredicate(atom) => atom
                .arguments()
                .iter()
                .filter_map(|a| match a {
                    AtomArg::Var(v) if !positive_vars.contains(v) => Some(v),
                    _ => None,
                })
                .collect(),
            Predicate::ComparePredicate(cmp) => cmp
                .vars_set()
                .into_iter()
                .filter(|v| !positive_vars.contains(*v))
                .collect(),
        };
        if let Some(var) = unbound.first() {
            type_error!(
                i,
                "unsafe rule: variable {} is used in `{}` but not bound by any positive \
                 body atom. In rule: {}",
                var,
                pred,
                context
            );
        }
    }
    for var in rule.head().head_arguments().iter().flat_map(|a| a.vars()) {
        if !positive_vars.contains(var) {
            type_error!(
                i,
                "unsafe rule: head variable {} is not bound by any positive body atom. \
                 In rule: {}",
                var,
                context
            );
        }
    }

    // --- head arity ---------------------------------------------------------
    let head_name = rule.head().name().clone();
    let head_decl_types: Option<Vec<DataType>> = match decls.get(head_name.as_str()) {
        None => None,
        Some(decl) => {
            if rule.head().arity() != decl.arity() {
                type_error!(
                    i,
                    "arity mismatch: {} is declared with {} columns but the head has {} \
                     arguments. In rule: {}",
                    head_name,
                    decl.arity(),
                    rule.head().arity(),
                    context
                );
            }
            Some(decl.attributes().iter().map(|a| *a.data_type()).collect())
        }
    };

    // --- type comparisons ----------------------------------------------------
    for pred in rule.rhs_mut() {
        if let Predicate::ComparePredicate(cmp) = pred {
            let left = type_arithmetic(i, cmp.left_mut(), &var_kinds, &context)?;
            let right = type_arithmetic(i, cmp.right_mut(), &var_kinds, &context)?;
            unify(i, left, right, &context)?;
        }
    }

    // --- type head expressions and aggregations against their columns -------
    for (arg_idx, arg) in rule.head_mut().head_arguments_mut().iter_mut().enumerate() {
        let col = head_decl_types
            .as_ref()
            .and_then(|types| types.get(arg_idx).copied());
        match arg {
            HeadArg::Var(_) => {}
            HeadArg::Arith(arith) => {
                let computed = type_arithmetic(i, arith, &var_kinds, &context)?;
                if let Some(col) = col {
                    let kind = unify(i, computed, ValueKind::of_decl(col), &context)?;
                    arith.set_data_type(kind.mode());
                }
            }
            HeadArg::Aggregation(agg) => {
                let computed = type_arithmetic(i, agg.arithmetic_mut(), &var_kinds, &context)?;
                if matches!(agg.operator(), AggregationOperator::Count) {
                    agg.set_data_type(DataType::Integer);
                } else if let Some(col) = col {
                    let kind = unify(i, computed, ValueKind::of_decl(col), &context)?;
                    agg.set_data_type(kind.mode());
                } else {
                    agg.set_data_type(computed.mode());
                }
            }
        }
    }
    Ok(())
}

/// Record an atom's variable bindings, checking its arity against the decl and
/// that no variable is bound to columns of different types.
fn bind_atom(
    rule_idx: usize,
    atom: &Atom,
    positive: bool,
    decls: &HashMap<&str, &RelDecl>,
    var_kinds: &mut HashMap<String, ValueKind>,
    positive_vars: &mut HashSet<String>,
    context: &str,
) -> Result<(), TypeError> {
    let decl = decls.get(atom.name());
    if let Some(decl) = decl {
        if atom.arity() != decl.arity() {
            type_error!(
                rule_idx,
                "arity mismatch: {} is declared with {} columns but is used with {} \
                 arguments. In rule: {}",
                atom.name(),
                decl.arity(),
                atom.arity(),
                context
            );
        }
    }
    for (i, arg) in atom.arguments().iter().enumerate() {
        let AtomArg::Var(name) = arg else { continue };
        if positive {
            positive_vars.insert(name.clone());
        }
        let Some(decl) = decl else { continue };
        let kind = ValueKind::of_decl(*decl.attributes()[i].data_type());
        match var_kinds.get(name) {
            None => {
                var_kinds.insert(name.clone(), kind);
            }
            Some(&prev) if prev == kind => {}
            Some(&prev) => type_error!(
                rule_idx,
                "type conflict: variable {} is bound to a {} column and a {} column — \
                 such a join never matches. In rule: {}",
                name,
                prev.describe(),
                kind.describe(),
                context
            ),
        }
    }
    Ok(())
}

/// Type an arithmetic chain: unify its factors' kinds, recursively typing each
/// parenthesised sub-expression and builtin argument on its own, then record
/// the chain's evaluation mode.
fn type_arithmetic(
    rule_idx: usize,
    arith: &mut Arithmetic,
    var_kinds: &HashMap<String, ValueKind>,
    context: &str,
) -> Result<ValueKind, TypeError> {
    let mut kind = type_factor(rule_idx, arith.init_mut(), var_kinds, context)?;
    // Split borrows: rest_mut() borrows all of arith, so collect kinds first.
    for (_, factor) in arith.rest_mut().iter_mut() {
        let next = type_factor(rule_idx, factor, var_kinds, context)?;
        kind = unify(rule_idx, kind, next, context)?;
    }
    arith.set_data_type(kind.mode());
    Ok(kind)
}

fn type_factor(
    rule_idx: usize,
    factor: &mut Factor,
    var_kinds: &HashMap<String, ValueKind>,
    context: &str,
) -> Result<ValueKind, TypeError> {
    Ok(match factor {
        // Unbound variables are caught by the safety check; default here just
        // keeps error ordering sane.
        Factor::Var(name) => var_kinds.get(name).copied().unwrap_or(ValueKind::Int),
        Factor::Const(c) => match c {
            crate::rule::Const::Integer(_) => ValueKind::Int,
            crate::rule::Const::Float(_) => ValueKind::Float,
            crate::rule::Const::Text(_) => ValueKind::Str,
        },
        Factor::Paren(inner) => type_arithmetic(rule_idx, inner, var_kinds, context)?,
        Factor::Builtin(op, args) => {
            let mut arg_kinds: Vec<ValueKind> = Vec::with_capacity(args.len());
            for a in args.iter_mut() {
                arg_kinds.push(type_factor(rule_idx, a, var_kinds, context)?);
            }
            let expect = |want: &[ValueKind]| -> Result<(), TypeError> {
                if arg_kinds.as_slice() != want {
                    let wanted = match want {
                        [one] => format!("one {} argument", one.describe()),
                        many => format!(
                            "{} arguments ({})",
                            many.len(),
                            many.iter()
                                .map(|k| k.describe())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    };
                    return Err(TypeError {
                        rule: Some(rule_idx),
                        message: format!(
                            "type error: {} takes {}, got ({}). In rule: {}",
                            op,
                            wanted,
                            arg_kinds
                                .iter()
                                .map(|k| k.describe())
                                .collect::<Vec<_>>()
                                .join(", "),
                            context
                        ),
                    });
                }
                Ok(())
            };
            match op {
                // Conversions: strictly typed — they exist to cross the
                // no-implicit-mixing boundary on purpose.
                BuiltinOp::ToFloat => {
                    expect(&[ValueKind::Int])?;
                    ValueKind::Float
                }
                BuiltinOp::Round | BuiltinOp::Floor => {
                    expect(&[ValueKind::Float])?;
                    ValueKind::Int
                }
                // Float math.
                BuiltinOp::Ln | BuiltinOp::Exp | BuiltinOp::Sqrt => {
                    expect(&[ValueKind::Float])?;
                    ValueKind::Float
                }
                BuiltinOp::Pow => {
                    expect(&[ValueKind::Float, ValueKind::Float])?;
                    ValueKind::Float
                }
                // Polymorphic: resolved to a concrete op here, because
                // evaluation is type-blind and needs the mode baked in.
                BuiltinOp::Abs | BuiltinOp::AbsInt | BuiltinOp::AbsFloat => {
                    match arg_kinds.as_slice() {
                        [ValueKind::Int] => {
                            *op = BuiltinOp::AbsInt;
                            ValueKind::Int
                        }
                        [ValueKind::Float] => {
                            *op = BuiltinOp::AbsFloat;
                            ValueKind::Float
                        }
                        _ => {
                            return Err(TypeError {
                                rule: Some(rule_idx),
                                message: format!(
                                    "type error: abs takes one number or float argument, \
                                     got ({}). In rule: {}",
                                    arg_kinds
                                        .iter()
                                        .map(|k| k.describe())
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                    context
                                ),
                            });
                        }
                    }
                }
                BuiltinOp::Similarity => {
                    expect(&[ValueKind::Str, ValueKind::Str])?;
                    ValueKind::Int
                }
                // String-producing builtins.
                BuiltinOp::SplitNth
                | BuiltinOp::Replace
                | BuiltinOp::BeforeLast
                | BuiltinOp::AfterLast
                | BuiltinOp::Concat
                | BuiltinOp::ToLower
                | BuiltinOp::ToUpper => ValueKind::Str,
                // Integer-producing builtins (booleans return 1/0).
                BuiltinOp::StartsWith
                | BuiltinOp::Contains
                | BuiltinOp::StrBefore
                | BuiltinOp::ExtractNumber
                | BuiltinOp::DateEpoch => ValueKind::Int,
            }
        }
    })
}

/// Str unifies with Int (both are raw i64 ids/values in Integer mode); mixing
/// Float with either is an error — there is no implicit conversion between
/// integer values and IEEE-754 bit patterns (use `to_float` / `round`).
fn unify(
    rule_idx: usize,
    a: ValueKind,
    b: ValueKind,
    context: &str,
) -> Result<ValueKind, TypeError> {
    Ok(match (a, b) {
        (ValueKind::Float, ValueKind::Float) => ValueKind::Float,
        (ValueKind::Float, _) | (_, ValueKind::Float) => type_error!(
            rule_idx,
            "type error: float and number/string mixed in one expression — write float \
             literals with a decimal point (1.0), and convert explicitly with to_float(n) \
             or round(f)/floor(f) (no implicit conversion). In rule: {}",
            context
        ),
        (ValueKind::Str, ValueKind::Str) => ValueKind::Str,
        _ => ValueKind::Int,
    })
}
