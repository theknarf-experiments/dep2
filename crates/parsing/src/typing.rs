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

/// Type and validate `rules` against the relation declarations. Panics (like
/// the rest of program loading) on errors, naming the offending rule.
pub fn resolve_types(edbs: &[RelDecl], idbs: &[RelDecl], rules: &mut [FLRule]) {
    let decls: HashMap<&str, &RelDecl> = edbs
        .iter()
        .chain(idbs.iter())
        .map(|d| (d.name(), d))
        .collect();
    for rule in rules {
        resolve_rule(rule, &decls);
    }
}

fn resolve_rule(rule: &mut FLRule, decls: &HashMap<&str, &RelDecl>) {
    let context = rule.to_string();

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
            atom,
            positive,
            decls,
            &mut var_kinds,
            &mut positive_vars,
            &context,
        );
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
        return;
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
            panic!(
                "unsafe rule: variable {} is used in `{}` but not bound by any positive \
                 body atom. In rule: {}",
                var, pred, context
            );
        }
    }
    for var in rule.head().head_arguments().iter().flat_map(|a| a.vars()) {
        if !positive_vars.contains(var) {
            panic!(
                "unsafe rule: head variable {} is not bound by any positive body atom. \
                 In rule: {}",
                var, context
            );
        }
    }

    // --- head arity ---------------------------------------------------------
    let head_name = rule.head().name().clone();
    let head_decl_types: Option<Vec<DataType>> = decls.get(head_name.as_str()).map(|decl| {
        if rule.head().arity() != decl.arity() {
            panic!(
                "arity mismatch: {} is declared with {} columns but the head has {} \
                 arguments. In rule: {}",
                head_name,
                decl.arity(),
                rule.head().arity(),
                context
            );
        }
        decl.attributes().iter().map(|a| *a.data_type()).collect()
    });

    // --- type comparisons ----------------------------------------------------
    for pred in rule.rhs_mut() {
        if let Predicate::ComparePredicate(cmp) = pred {
            let left = type_arithmetic(cmp.left_mut(), &var_kinds, &context);
            let right = type_arithmetic(cmp.right_mut(), &var_kinds, &context);
            unify(left, right, &context);
        }
    }

    // --- type head expressions and aggregations against their columns -------
    for (i, arg) in rule.head_mut().head_arguments_mut().iter_mut().enumerate() {
        let col = head_decl_types
            .as_ref()
            .and_then(|types| types.get(i).copied());
        match arg {
            HeadArg::Var(_) => {}
            HeadArg::Arith(arith) => {
                let computed = type_arithmetic(arith, &var_kinds, &context);
                if let Some(col) = col {
                    let kind = unify(computed, ValueKind::of_decl(col), &context);
                    arith.set_data_type(kind.mode());
                }
            }
            HeadArg::Aggregation(agg) => {
                let computed = type_arithmetic(agg.arithmetic_mut(), &var_kinds, &context);
                if matches!(agg.operator(), AggregationOperator::Count) {
                    agg.set_data_type(DataType::Integer);
                } else if let Some(col) = col {
                    let kind = unify(computed, ValueKind::of_decl(col), &context);
                    agg.set_data_type(kind.mode());
                } else {
                    agg.set_data_type(computed.mode());
                }
            }
        }
    }
}

/// Record an atom's variable bindings, checking its arity against the decl and
/// that no variable is bound to columns of different types.
fn bind_atom(
    atom: &Atom,
    positive: bool,
    decls: &HashMap<&str, &RelDecl>,
    var_kinds: &mut HashMap<String, ValueKind>,
    positive_vars: &mut HashSet<String>,
    context: &str,
) {
    let decl = decls.get(atom.name());
    if let Some(decl) = decl {
        if atom.arity() != decl.arity() {
            panic!(
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
            Some(&prev) => panic!(
                "type conflict: variable {} is bound to a {} column and a {} column — \
                 such a join never matches. In rule: {}",
                name,
                prev.describe(),
                kind.describe(),
                context
            ),
        }
    }
}

/// Type an arithmetic chain: unify its factors' kinds, recursively typing each
/// parenthesised sub-expression and builtin argument on its own, then record
/// the chain's evaluation mode.
fn type_arithmetic(
    arith: &mut Arithmetic,
    var_kinds: &HashMap<String, ValueKind>,
    context: &str,
) -> ValueKind {
    let mut kind = type_factor(arith.init_mut(), var_kinds, context);
    // Split borrows: rest_mut() borrows all of arith, so collect kinds first.
    for (_, factor) in arith.rest_mut().iter_mut() {
        let next = type_factor(factor, var_kinds, context);
        kind = unify(kind, next, context);
    }
    arith.set_data_type(kind.mode());
    kind
}

fn type_factor(
    factor: &mut Factor,
    var_kinds: &HashMap<String, ValueKind>,
    context: &str,
) -> ValueKind {
    match factor {
        // Unbound variables are caught by the safety check; default here just
        // keeps error ordering sane.
        Factor::Var(name) => var_kinds.get(name).copied().unwrap_or(ValueKind::Int),
        Factor::Const(c) => match c {
            crate::rule::Const::Integer(_) => ValueKind::Int,
            crate::rule::Const::Float(_) => ValueKind::Float,
            crate::rule::Const::Text(_) => ValueKind::Str,
        },
        Factor::Paren(inner) => type_arithmetic(inner, var_kinds, context),
        Factor::Builtin(op, args) => {
            let arg_kinds: Vec<ValueKind> = args
                .iter_mut()
                .map(|a| type_factor(a, var_kinds, context))
                .collect();
            let expect = |want: ValueKind| {
                if arg_kinds.len() != 1 || arg_kinds[0] != want {
                    panic!(
                        "type error: {} takes one {} argument, got ({}). In rule: {}",
                        op,
                        want.describe(),
                        arg_kinds
                            .iter()
                            .map(|k| k.describe())
                            .collect::<Vec<_>>()
                            .join(", "),
                        context
                    );
                }
            };
            match op {
                // Conversions: strictly typed — they exist to cross the
                // no-implicit-mixing boundary on purpose.
                BuiltinOp::ToFloat => {
                    expect(ValueKind::Int);
                    ValueKind::Float
                }
                BuiltinOp::Round | BuiltinOp::Floor => {
                    expect(ValueKind::Float);
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
    }
}

/// Str unifies with Int (both are raw i64 ids/values in Integer mode); mixing
/// Float with either is an error — there is no implicit conversion between
/// integer values and IEEE-754 bit patterns (use `to_float` / `round`).
fn unify(a: ValueKind, b: ValueKind, context: &str) -> ValueKind {
    match (a, b) {
        (ValueKind::Float, ValueKind::Float) => ValueKind::Float,
        (ValueKind::Float, _) | (_, ValueKind::Float) => panic!(
            "type error: float and number/string mixed in one expression — write float \
             literals with a decimal point (1.0), and convert explicitly with to_float(n) \
             or round(f)/floor(f) (no implicit conversion). In rule: {}",
            context
        ),
        (ValueKind::Str, ValueKind::Str) => ValueKind::Str,
        _ => ValueKind::Int,
    }
}
