//! Chumsky front-end for FlowLog `.dl` programs with ariadne diagnostics.
//!
//! THE parser for `.dl` programs: builds the `parsing` crate's AST
//! (`parsing::parser::Program`), keeping byte spans through parsing so errors
//! report as pretty, labelled source snippets instead of the typing pass's
//! bare panics:
//!
//! ```text
//! error: unsafe rule: head variable Y is not bound by any positive body atom
//!    ╭─[rules.dl:6:1]
//!  6 │ r(X, Y) :- e(X).
//!    · ───────┬───────
//!    ·        ╰── in this rule
//! ```
//!
//! [`parse`] returns the `Program` or a list of [`Diagnostic`]s; [`render`]
//! turns diagnostics into an ariadne report string. Typing/validation errors
//! (from `parsing::typing`, via `Program::try_new`) come back with the
//! offending rule's span attached.
//!
//! Sections (`.in` / `.printsize` / `.out` / `.rule`) may repeat and
//! interleave freely. The parser is exercised by the repo's example corpus,
//! by error-quality tests, and by property tests that check generated
//! programs against a structural model (`tests/proptests.rs`).

use std::ops::Range;

use ariadne::{Color, Config, Label, Report, ReportKind, Source};
use chumsky::prelude::*;

use parsing::aggregation::{Aggregation, AggregationOperator};
use parsing::arithmetic::{Arithmetic, ArithmeticOperator, BuiltinOp, Factor};
use parsing::compare::{ComparisonExpr, ComparisonOperator};
use parsing::decl::{Attribute, DataType, RelDecl};
use parsing::head::{Head, HeadArg};
use parsing::parser::Program;
use parsing::rule::{Atom, AtomArg, FLRule, Predicate};

/// One error, with the byte span it points at.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub span: Range<usize>,
    pub message: String,
    /// Short text for the underline label (defaults to "here").
    pub label: String,
}

impl Diagnostic {
    fn new(span: Range<usize>, message: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
            label: label.into(),
        }
    }
}

/// Render diagnostics as ariadne reports against `src`. `color` should be
/// false for tests/logs and true for terminals.
pub fn render(filename: &str, src: &str, diagnostics: &[Diagnostic], color: bool) -> String {
    let mut out = Vec::new();
    for d in diagnostics {
        // Clamp the span: typing errors on the last rule can end at src.len().
        let span = d.span.start.min(src.len())..d.span.end.min(src.len());
        Report::build(ReportKind::Error, (filename, span.clone()))
            .with_config(Config::default().with_color(color))
            .with_message(&d.message)
            .with_label(
                Label::new((filename, span))
                    .with_message(&d.label)
                    .with_color(Color::Red),
            )
            .finish()
            .write((filename, Source::from(src)), &mut out)
            .expect("writing to a Vec cannot fail");
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a `.dl` program. On success the returned [`Program`] is fully typed
/// and validated (same pipeline as `parsing::parser::Program::new`).
pub fn parse(src: &str) -> Result<Program, Vec<Diagnostic>> {
    let (items, errors) = parser().parse(src).into_output_errors();
    if !errors.is_empty() {
        return Err(errors.into_iter().map(|e| enrich(src, &e)).collect());
    }
    assemble(src, items.unwrap_or_default())
}

/// Parse and, on error, render the report (with color) — the one-call form
/// for CLI use.
pub fn parse_or_render(filename: &str, src: &str, color: bool) -> Result<Program, String> {
    parse(src).map_err(|diagnostics| render(filename, src, &diagnostics, color))
}

/// Turn a raw chumsky error into a [`Diagnostic`], rewriting the one shape
/// the expected-token list explains badly: a failure at `(` right after an
/// identifier in expression position means an unknown function was called
/// (relation atoms parse the parenthesis, so they never fail here).
fn enrich(src: &str, e: &Rich<'_, char>) -> Diagnostic {
    let range = e.span().into_range();
    if src[range.start.min(src.len())..].starts_with('(') {
        let before = src[..range.start].trim_end();
        let name: String = before
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let known =
            BUILTINS.iter().any(|(n, _)| *n == name) || AGGREGATES.iter().any(|(n, _)| *n == name);
        if !name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit()) && !known {
            let name_start = before.len() - name.len();
            return Diagnostic::new(
                name_start..range.start + 1,
                format!(
                    "unknown function `{}` — builtins are {}; aggregates (head only) are {}",
                    name,
                    BUILTINS
                        .iter()
                        .map(|(n, _)| *n)
                        .collect::<Vec<_>>()
                        .join(", "),
                    AGGREGATES
                        .iter()
                        .map(|(n, _)| *n)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                "not a builtin",
            );
        }
    }
    Diagnostic::new(
        range,
        format!("syntax error: {}", e),
        e.reason().to_string(),
    )
}

// ---------------------------------------------------------------------------
// Flat item stream -> Program
// ---------------------------------------------------------------------------

/// The program is parsed as a flat list of spanned items and assembled after,
/// which keeps the combinators simple and the "declaration outside a section"
/// class of errors precise.
#[derive(Clone)]
enum Item {
    /// `.in` / `.printsize` / `.out`
    Section(SectionKind),
    /// `.rule` (a marker only — rules are recognised on their own)
    RuleMarker,
    Decl(RelDecl),
    Rule(FLRule),
}

#[derive(Clone, Copy, PartialEq)]
enum SectionKind {
    In,
    PrintSize,
    Out,
}

type Spanned<T> = (T, Range<usize>);

fn assemble(src: &str, items: Vec<Spanned<Item>>) -> Result<Program, Vec<Diagnostic>> {
    let mut edbs: Vec<RelDecl> = Vec::new();
    let mut idbs: Vec<RelDecl> = Vec::new();
    let mut rules: Vec<FLRule> = Vec::new();
    let mut rule_spans: Vec<Range<usize>> = Vec::new();
    let mut section: Option<SectionKind> = None;

    for (item, span) in items {
        match item {
            Item::Section(kind) => section = Some(kind),
            Item::RuleMarker => {}
            Item::Decl(mut decl) => match section {
                None => {
                    return Err(vec![Diagnostic::new(
                        span,
                        "declaration outside a section — put it under `.in` (input \
                         relations), `.printsize` or `.out` (derived relations)",
                        "this declaration",
                    )])
                }
                Some(SectionKind::In) => edbs.push(decl),
                Some(kind) => {
                    decl.set_force_serve(kind == SectionKind::Out);
                    idbs.push(decl);
                }
            },
            Item::Rule(rule) => {
                if let Some(d) = check_aggregate_positions(&rule, &span) {
                    return Err(vec![d]);
                }
                rules.push(rule);
                rule_spans.push(span);
            }
        }
    }

    Program::try_new(edbs, idbs, rules).map_err(|e| {
        let span = e
            .rule
            .and_then(|i| rule_spans.get(i).cloned())
            .unwrap_or(0..src.len().min(1));
        // The panic-era message embeds "In rule: <text>"; the span shows the
        // rule, so drop that suffix from the headline.
        let message = match e.message.split(". In rule:").next() {
            Some(head) if head.len() < e.message.len() => head.to_string(),
            _ => e.message.clone(),
        };
        vec![Diagnostic::new(span, message, "in this rule")]
    })
}

/// At most one aggregate per head, in the last argument position (the
/// planner computes it as the group's reduction). Reported here (post-parse)
/// so the message can point at the rule rather than being a generic
/// expectation failure.
fn check_aggregate_positions(rule: &FLRule, rule_span: &Range<usize>) -> Option<Diagnostic> {
    let args = rule.head().head_arguments();
    let aggregates: Vec<usize> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| matches!(a, HeadArg::Aggregation(_)))
        .map(|(i, _)| i)
        .collect();
    match aggregates.as_slice() {
        [] => None,
        [i] if *i == args.len() - 1 => None,
        [_] => Some(Diagnostic::new(
            rule_span.clone(),
            "an aggregate must be the LAST argument of the head",
            "in this rule",
        )),
        _ => Some(Diagnostic::new(
            rule_span.clone(),
            "at most one aggregate per head",
            "in this rule",
        )),
    }
}

// ---------------------------------------------------------------------------
// The parser
// ---------------------------------------------------------------------------

type Extra<'a> = extra::Err<Rich<'a, char>>;

const BUILTINS: &[(&str, BuiltinOp)] = &[
    ("split_nth", BuiltinOp::SplitNth),
    ("starts_with", BuiltinOp::StartsWith),
    ("contains", BuiltinOp::Contains),
    ("str_before", BuiltinOp::StrBefore),
    ("replace", BuiltinOp::Replace),
    ("before_last", BuiltinOp::BeforeLast),
    ("after_last", BuiltinOp::AfterLast),
    ("concat", BuiltinOp::Concat),
    ("extract_number", BuiltinOp::ExtractNumber),
    ("date_epoch", BuiltinOp::DateEpoch),
    ("to_float", BuiltinOp::ToFloat),
    ("round", BuiltinOp::Round),
    ("floor", BuiltinOp::Floor),
    ("to_lower", BuiltinOp::ToLower),
    ("to_upper", BuiltinOp::ToUpper),
];

const AGGREGATES: &[(&str, AggregationOperator)] = &[
    ("count", AggregationOperator::Count),
    ("sum", AggregationOperator::Sum),
    ("min", AggregationOperator::Min),
    ("max", AggregationOperator::Max),
];

/// Whitespace and `//` / `#` line comments.
fn trivia<'a>() -> impl Parser<'a, &'a str, (), Extra<'a>> + Clone {
    let comment = choice((just("//"), just("#")))
        .then(any().and_is(just('\n').not()).repeated())
        .ignored();
    choice((one_of(" \t\r\n").ignored(), comment))
        .repeated()
        .ignored()
}

/// A fixed token, surrounded by trivia.
fn tok<'a>(s: &'static str) -> impl Parser<'a, &'a str, &'a str, Extra<'a>> + Clone {
    just(s).padded_by(trivia())
}

/// `[A-Za-z_][A-Za-z0-9_]*`, surrounded by trivia.
fn ident<'a>() -> impl Parser<'a, &'a str, &'a str, Extra<'a>> + Clone {
    text::ascii::ident().padded_by(trivia())
}

/// Numeric or string constant. Strings keep their surrounding quotes — that
/// is the form `Const::Text` carries, and downstream literal interning
/// (`reading::intern_literal`) expects it.
fn constant<'a>() -> impl Parser<'a, &'a str, parsing::rule::Const, Extra<'a>> + Clone {
    let sign = one_of("+-").or_not();
    let digits = text::digits(10);
    let number = sign
        .then(digits)
        .then(just('.').then(digits).or_not())
        .to_slice()
        .map(|s: &str| {
            if s.contains('.') {
                parsing::rule::Const::Float(s.parse::<f64>().unwrap().to_bits() as i64)
            } else {
                parsing::rule::Const::Integer(s.parse::<i64>().unwrap())
            }
        });
    let string = just('"')
        .then(choice((just('\\').then(any()).ignored(), none_of("\"").ignored())).repeated())
        .then(just('"'))
        .to_slice()
        .map(|s: &str| parsing::rule::Const::Text(s.to_string()));
    choice((number, string)).padded_by(trivia())
}

/// An arithmetic expression (left-to-right chain of factors).
fn arithmetic<'a>() -> impl Parser<'a, &'a str, Arithmetic, Extra<'a>> + Clone {
    recursive(|arith| {
        // A builtin argument is a full expression; a bare factor stays a
        // factor, anything with operators is held as a sub-expression.
        let builtin_arg = arith.clone().map(|a: Arithmetic| {
            if a.rest().is_empty() {
                a.init().clone()
            } else {
                Factor::Paren(Box::new(a))
            }
        });
        let known_builtin = ident().try_map(|name: &str, span| {
            BUILTINS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, op)| *op)
                .ok_or_else(|| Rich::custom(span, format!("`{}` is not a builtin", name)))
        });
        let builtin_call = known_builtin
            .then(
                builtin_arg
                    .separated_by(tok(","))
                    .at_least(1)
                    .collect::<Vec<_>>()
                    .delimited_by(tok("("), tok(")")),
            )
            .map(|(op, args)| Factor::Builtin(op, args));

        let paren = arith
            .delimited_by(tok("("), tok(")"))
            .map(|a| Factor::Paren(Box::new(a)));

        let variable = ident().map(|s: &str| Factor::Var(s.to_string()));

        let factor = choice((builtin_call, paren, constant().map(Factor::Const), variable));

        let op = choice((
            tok("+").to(ArithmeticOperator::Plus),
            tok("-").to(ArithmeticOperator::Minus),
            tok("*").to(ArithmeticOperator::Multiply),
            tok("/").to(ArithmeticOperator::Divide),
            tok("%").to(ArithmeticOperator::Modulo),
        ));

        factor
            .clone()
            .then(op.then(factor).repeated().collect::<Vec<_>>())
            .map(|(init, rest)| Arithmetic::new(init, rest))
    })
}

fn comparison_op<'a>() -> impl Parser<'a, &'a str, ComparisonOperator, Extra<'a>> + Clone {
    choice((
        tok("!=").to(ComparisonOperator::NotEquals),
        tok(">=").to(ComparisonOperator::GreaterEqualThan),
        tok("<=").to(ComparisonOperator::LessEqualThan),
        tok("=").to(ComparisonOperator::Equals),
        tok(">").to(ComparisonOperator::GreaterThan),
        tok("<").to(ComparisonOperator::LessThan),
    ))
}

fn atom<'a>() -> impl Parser<'a, &'a str, Atom, Extra<'a>> + Clone {
    let arg = choice((
        tok("_").to(AtomArg::Placeholder),
        constant().map(AtomArg::Const),
        ident().map(|s: &str| AtomArg::Var(s.to_string())),
    ));
    ident()
        .then(
            arg.separated_by(tok(","))
                .collect::<Vec<_>>()
                .delimited_by(tok("("), tok(")")),
        )
        .map(|(name, args): (&str, Vec<AtomArg>)| Atom::from_str(name, args))
}

fn predicate<'a>() -> impl Parser<'a, &'a str, Predicate, Extra<'a>> + Clone {
    let compare =
        arithmetic()
            .then(comparison_op())
            .then(arithmetic())
            .map(|((left, op), right)| {
                Predicate::ComparePredicate(ComparisonExpr::new(left, op, right))
            });
    let negated = tok("!")
        .ignore_then(atom())
        .map(Predicate::NegatedAtomPredicate);
    // compare first (`starts_with(S, P) = 1` must not be swallowed as an
    // atom); it fails fast on a real atom because no operator follows.
    choice((compare, negated, atom().map(Predicate::AtomPredicate)))
}

fn head<'a>() -> impl Parser<'a, &'a str, Head, Extra<'a>> + Clone {
    let aggregate = ident()
        .try_map(|name: &str, span| {
            AGGREGATES
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, op)| *op)
                .ok_or_else(|| Rich::custom(span, format!("`{}` is not an aggregate", name)))
        })
        .then(arithmetic().delimited_by(tok("("), tok(")")))
        .map(|(op, arith)| HeadArg::Aggregation(Aggregation::new(op, arith)));
    let arith_arg = arithmetic().map(|a| {
        if a.is_var() {
            HeadArg::Var(a.vars()[0].to_string())
        } else {
            HeadArg::Arith(a)
        }
    });
    let head_arg = choice((aggregate, arith_arg));
    ident()
        .then(
            head_arg
                .separated_by(tok(","))
                .collect::<Vec<_>>()
                .delimited_by(tok("("), tok(")")),
        )
        .map(|(name, args): (&str, Vec<HeadArg>)| Head::new(name.to_string(), args))
}

fn rule<'a>() -> impl Parser<'a, &'a str, FLRule, Extra<'a>> + Clone {
    let optimize = choice((
        tok(".plan").to((true, false)),
        tok(".sip").to((false, true)),
        tok(".optimize").to((true, true)),
    ));
    head()
        .then_ignore(tok(":-"))
        .then(
            predicate()
                .separated_by(tok(","))
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .then_ignore(tok("."))
        .then(optimize.or_not())
        .map(|((head, rhs), opt)| {
            let (is_planning, is_sip) = opt.unwrap_or((false, false));
            FLRule::new(head, rhs, is_planning, is_sip)
        })
}

fn decl<'a>() -> impl Parser<'a, &'a str, RelDecl, Extra<'a>> + Clone {
    let data_type = ident().try_map(|name: &str, span| match name {
        "number" => Ok(DataType::Integer),
        "string" => Ok(DataType::String),
        "float" => Ok(DataType::Float),
        other => Err(Rich::custom(
            span,
            format!(
                "unknown column type `{}` — the types are number, string and float",
                other
            ),
        )),
    });
    let attribute = ident()
        .then_ignore(tok(":"))
        .then(data_type)
        .map(|(name, dt): (&str, DataType)| Attribute::new(name, dt));
    // `.input Arc.csv` / `.output result.facts` — a filename token.
    let file_path = any()
        .filter(|c: &char| !c.is_whitespace())
        .repeated()
        .at_least(1)
        .to_slice()
        .padded_by(trivia());
    let io = choice((
        tok(".input").ignore_then(file_path.clone()),
        tok(".output").ignore_then(file_path),
    ));
    tok(".decl")
        .ignore_then(ident())
        .then(
            attribute
                .separated_by(tok(","))
                .collect::<Vec<_>>()
                .delimited_by(tok("("), tok(")")),
        )
        .then(io.or_not())
        .map(
            |((name, attrs), path): ((&str, Vec<Attribute>), Option<&str>)| {
                RelDecl::new(name, attrs, path)
            },
        )
}

fn parser<'a>() -> impl Parser<'a, &'a str, Vec<Spanned<Item>>, Extra<'a>> {
    let section = choice((
        tok(".in").to(Item::Section(SectionKind::In)),
        tok(".printsize").to(Item::Section(SectionKind::PrintSize)),
        tok(".out").to(Item::Section(SectionKind::Out)),
        tok(".rule").to(Item::RuleMarker),
    ));
    let item = choice((section, decl().map(Item::Decl), rule().map(Item::Rule)));
    item.map_with(|item, e| {
        let span: SimpleSpan = e.span();
        (item, span.into_range())
    })
    .repeated()
    .at_least(1)
    .collect::<Vec<_>>()
    .then_ignore(trivia())
    .then_ignore(end())
}
