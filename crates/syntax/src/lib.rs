//! The parser for FlowLog `.dl` programs, with ariadne diagnostics.
//!
//! Two stages: a [logos] lexer tokenizes the source (keywords, punctuation,
//! literals — whitespace and `//`/`#` comments skipped), then a [chumsky]
//! parser consumes the token stream and builds the `parsing` crate's AST
//! (`parsing::parser::Program`). Every token carries its byte span, and item
//! spans survive through parsing, so errors report as pretty, labelled source
//! snippets instead of the typing pass's bare panics:
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

use std::fmt;
use std::ops::Range;

use ariadne::{Color, Config, Label, Report, ReportKind, Source};
use chumsky::input::ValueInput;
use chumsky::prelude::*;
use logos::Logos;

use parsing::aggregation::{Aggregation, AggregationOperator};
use parsing::arithmetic::{Arithmetic, ArithmeticOperator, BuiltinOp, Factor};
use parsing::compare::{ComparisonExpr, ComparisonOperator};
use parsing::decl::{Attribute, DataType, RelDecl};
use parsing::head::{Head, HeadArg};
use parsing::parser::Program;
use parsing::rule::{Atom, AtomArg, Const, FLRule, Predicate};

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
    parse_with_directives(src).map(|(program, _)| program)
}

/// Like [`parse`], also returning what the program declared about its runtime.
pub fn parse_with_directives(src: &str) -> Result<(Program, Directives), Vec<Diagnostic>> {
    assemble(src, parse_items(src)?)
}

/// Consume a `"""` raw string, returning its content.
///
/// Logos matches the longest token, so `"""` wins over the empty `""` the
/// ordinary string regex would otherwise take.
fn raw_string<'src>(lex: &mut logos::Lexer<'src, Token<'src>>) -> Option<&'src str> {
    let rest = lex.remainder();
    let end = rest.find("\"\"\"")?;
    let content = &rest[..end];
    lex.bump(end + 3);
    Some(content)
}

/// Lex + parse one source into items (stages 1 and 2, no assembly).
fn parse_items(src: &str) -> Result<Vec<Spanned<Item>>, Vec<Diagnostic>> {
    // Stage 1: lex. Lex errors (unrecognized bytes, unterminated strings) are
    // reported all at once, with spans.
    let mut tokens: Vec<(Token, SimpleSpan)> = Vec::new();
    let mut lex_errors: Vec<Diagnostic> = Vec::new();
    for (result, span) in Token::lexer(src).spanned() {
        match result {
            Ok(token) => tokens.push((token, SimpleSpan::from(span))),
            Err(()) => {
                let message = if src[span.clone()].starts_with('"') {
                    "unterminated string literal"
                } else {
                    "unrecognized token"
                };
                lex_errors.push(Diagnostic::new(span, message, "here"));
            }
        }
    }
    if !lex_errors.is_empty() {
        return Err(lex_errors);
    }

    // Stage 2: parse the token stream (spans stay byte-based).
    let eoi = SimpleSpan::from(src.len()..src.len());
    let input = tokens.as_slice().map(eoi, |(t, s)| (t, s));
    let (items, errors) = parser().parse(input).into_output_errors();
    if !errors.is_empty() {
        return Err(errors.into_iter().map(|e| enrich(src, &e)).collect());
    }
    Ok(items.unwrap_or_default())
}

/// Parse and, on error, render the report (with color) — the one-call form
/// for CLI use.
pub fn parse_or_render(filename: &str, src: &str, color: bool) -> Result<Program, String> {
    parse_or_render_with_directives(filename, src, color).map(|(program, _)| program)
}

/// Like [`parse_or_render`], also returning the program's runtime directives.
pub fn parse_or_render_with_directives(
    filename: &str,
    src: &str,
    color: bool,
) -> Result<(Program, Directives), String> {
    parse_with_directives(src).map_err(|diagnostics| render(filename, src, &diagnostics, color))
}

/// Turn a raw chumsky error into a [`Diagnostic`], rewriting the one shape
/// the expected-token list explains badly: a failure at `(` right after an
/// identifier in expression position means an unknown function was called
/// (relation atoms parse the parenthesis, so they never fail here).
fn enrich(src: &str, e: &Rich<'_, Token<'_>>) -> Diagnostic {
    let range = e.span().into_range();
    if matches!(e.found(), Some(Token::LParen)) {
        let before = src[..range.start.min(src.len())].trim_end();
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
// The lexer
// ---------------------------------------------------------------------------

/// The token set. Longest-match keeps the overlapping shapes honest:
/// `.input` beats `.in`, `!=` beats `!`, `_x` (an identifier) beats `_` (a
/// placeholder), and `Arc.csv` lexes as one [`Token::FilePath`] rather than
/// ident-dot-ident.
#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(skip r"[ \t\r\n]+")]
#[logos(skip(r"(//|#)[^\n]*", allow_greedy = true))]
pub enum Token<'src> {
    #[token(".in")]
    InSection,
    #[token(".printsize")]
    PrintSizeSection,
    #[token(".out")]
    OutSection,
    #[token(".rule")]
    RuleSection,
    #[token(".decl")]
    DeclKw,
    #[token(".input")]
    InputKw,
    #[token(".output")]
    OutputKw,
    #[token(".plan")]
    PlanKw,
    #[token(".sip")]
    SipKw,
    #[token(".optimize")]
    OptimizeKw,
    #[token(".import")]
    ImportKw,
    #[token(".require")]
    RequireKw,
    #[token(".source")]
    SourceKw,

    #[token(":-")]
    Turnstile,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token(",")]
    Comma,
    #[token(":")]
    Colon,
    #[token(".")]
    Dot,
    #[token("!")]
    Bang,
    #[token("_", priority = 10)]
    Underscore,

    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("&")]
    Amp,
    #[token("|")]
    Pipe,
    #[token("^")]
    Caret,

    #[token("!=")]
    Ne,
    #[token(">=")]
    Ge,
    #[token("<=")]
    Le,
    #[token("=")]
    Eq,
    #[token(">")]
    Gt,
    #[token("<")]
    Lt,

    /// `Arc.csv` / `result.facts` / `_out-1.txt` (after `.input`/`.output`).
    #[regex(r"[A-Za-z_][A-Za-z0-9_-]*\.(facts|csv|txt)", |lex| lex.slice())]
    FilePath(&'src str),
    #[regex(r"[A-Za-z_][A-Za-z0-9_]*", |lex| lex.slice())]
    Ident(&'src str),
    /// Unsigned; the parser applies unary sign at the factor level, so `X-5`
    /// stays a subtraction chain.
    #[regex(r"[0-9]+\.[0-9]+", |lex| lex.slice())]
    Float(&'src str),
    #[regex(r"[0-9]+", |lex| lex.slice())]
    Int(&'src str),
    /// Quotes included — the form `Const::Text` carries, and what downstream
    /// literal interning (`reading::intern_literal`) expects.
    #[regex(r#""(\\.|[^"\\])*""#, |lex| lex.slice())]
    Str(&'src str),
    /// `"""..."""` — content only, no escapes, newlines kept. Embedded SQL is
    /// full of quotes and spans lines; requiring it to be escaped onto one line
    /// would make the common case the ugly one.
    #[token("\"\"\"", raw_string)]
    RawStr(&'src str),
}

impl fmt::Display for Token<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::InSection => write!(f, ".in"),
            Token::PrintSizeSection => write!(f, ".printsize"),
            Token::OutSection => write!(f, ".out"),
            Token::RuleSection => write!(f, ".rule"),
            Token::DeclKw => write!(f, ".decl"),
            Token::ImportKw => write!(f, ".import"),
            Token::RequireKw => write!(f, ".require"),
            Token::SourceKw => write!(f, ".source"),
            Token::InputKw => write!(f, ".input"),
            Token::OutputKw => write!(f, ".output"),
            Token::PlanKw => write!(f, ".plan"),
            Token::SipKw => write!(f, ".sip"),
            Token::OptimizeKw => write!(f, ".optimize"),
            Token::Turnstile => write!(f, ":-"),
            Token::LParen => write!(f, "("),
            Token::RParen => write!(f, ")"),
            Token::Comma => write!(f, ","),
            Token::Colon => write!(f, ":"),
            Token::Dot => write!(f, "."),
            Token::Bang => write!(f, "!"),
            Token::Underscore => write!(f, "_"),
            Token::Plus => write!(f, "+"),
            Token::Minus => write!(f, "-"),
            Token::Star => write!(f, "*"),
            Token::Slash => write!(f, "/"),
            Token::Percent => write!(f, "%"),
            Token::Amp => write!(f, "&"),
            Token::Pipe => write!(f, "|"),
            Token::Caret => write!(f, "^"),
            Token::Ne => write!(f, "!="),
            Token::Ge => write!(f, ">="),
            Token::Le => write!(f, "<="),
            Token::Eq => write!(f, "="),
            Token::Gt => write!(f, ">"),
            Token::Lt => write!(f, "<"),
            Token::FilePath(s) | Token::Ident(s) | Token::Float(s) | Token::Int(s) => {
                write!(f, "{}", s)
            }
            Token::Str(s) => write!(f, "{}", s),
            Token::RawStr(s) => write!(f, "\"\"\"{}\"\"\"", s),
        }
    }
}

// ---------------------------------------------------------------------------
// Flat item stream -> Program
// ---------------------------------------------------------------------------

/// The program is parsed as a flat list of spanned items and assembled after,
/// which keeps the combinators simple and the "declaration outside a section"
/// class of errors precise.
#[derive(Clone)]
enum Item {
    /// `.import "other.dl"` — merge another file (resolved by `parse_file`).
    Import(String),
    /// `.require duckdb` — the program needs that plugin to be present.
    Require(String),
    /// `.source rel = provider(k = v, ...)` — bind a relation to a data source.
    Source(SourceSpec),
    /// `.in` / `.printsize` / `.out`
    Section(SectionKind),
    /// `.rule` (a marker only — rules are recognised on their own)
    RuleMarker,
    Decl(RelDecl),
    Rule(FLRule),
}

/// A data source declared in the program.
///
/// `relation` is `None` for a multi-output provider (treesitter feeds several
/// relations from one source), matching `Dep2::add_source`.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceSpec {
    pub relation: Option<String>,
    pub provider: String,
    pub config: Vec<(String, String)>,
}

/// What a program declares about its runtime, as opposed to its rules.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Directives {
    /// Plugin names from `.require`.
    pub requires: Vec<String>,
    /// Sources from `.source`.
    pub sources: Vec<SourceSpec>,
}

#[derive(Clone, Copy, PartialEq)]
enum SectionKind {
    In,
    PrintSize,
    Out,
}

type Spanned<T> = (T, Range<usize>);

/// One file's assembled pieces, before cross-file merging.
struct FilePart {
    edbs: Vec<RelDecl>,
    idbs: Vec<RelDecl>,
    rules: Vec<FLRule>,
    rule_spans: Vec<Range<usize>>,
    imports: Vec<(String, Range<usize>)>,
    requires: Vec<(String, Range<usize>)>,
    sources: Vec<SourceSpec>,
}

/// Section-assemble one file's items (each file tracks its own `.in`/`.out`
/// context — an import never leaks section state into its importer).
fn split_items(items: Vec<Spanned<Item>>) -> Result<FilePart, Vec<Diagnostic>> {
    let mut part = FilePart {
        edbs: Vec::new(),
        idbs: Vec::new(),
        rules: Vec::new(),
        rule_spans: Vec::new(),
        imports: Vec::new(),
        requires: Vec::new(),
        sources: Vec::new(),
    };
    let mut section: Option<SectionKind> = None;

    for (item, span) in items {
        match item {
            Item::Import(path) => part.imports.push((path, span)),
            Item::Require(name) => part.requires.push((name, span)),
            Item::Source(spec) => part.sources.push(spec),
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
                Some(SectionKind::In) => part.edbs.push(decl),
                Some(kind) => {
                    decl.set_force_serve(kind == SectionKind::Out);
                    part.idbs.push(decl);
                }
            },
            Item::Rule(rule) => {
                if let Some(d) = check_aggregate_positions(&rule, &span) {
                    return Err(vec![d]);
                }
                part.rules.push(rule);
                part.rule_spans.push(span);
            }
        }
    }
    Ok(part)
}

fn assemble(
    src: &str,
    items: Vec<Spanned<Item>>,
) -> Result<(Program, Directives), Vec<Diagnostic>> {
    let part = split_items(items)?;
    let directives = Directives {
        requires: part.requires.iter().map(|(n, _)| n.clone()).collect(),
        sources: part.sources.clone(),
    };
    if let Some((path, span)) = part.imports.first() {
        return Err(vec![Diagnostic::new(
            span.clone(),
            format!(
                "`.import \"{}\"` needs a file context — load the program by path \
                 (imports are not available in inline programs or runtime queries)",
                path
            ),
            "this import",
        )]);
    }
    let rule_spans = part.rule_spans.clone();
    let program = Program::try_new(part.edbs, part.idbs, part.rules).map_err(|e| {
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
    })?;
    Ok((program, directives))
}

/// Parse a `.dl` file, resolving `.import "other.dl"` statements relative to
/// the importing file. Files merge import-once by canonical path (diamond
/// imports and cycles are harmless); identical duplicate declarations
/// collapse; conflicting ones are errors. Errors render against the file
/// they occur in.
pub fn parse_file(path: &std::path::Path, color: bool) -> Result<Program, String> {
    parse_file_with_sources(path, color).map(|(program, _)| program)
}

/// Everything a file and its imports declare about the runtime.
///
/// Collected separately from the program because a requirement is about the
/// runtime, not the rules: the engine checks it before wiring anything up, so a
/// missing plugin is reported as a missing plugin rather than as whatever the
/// dataflow does when a relation has no source.
pub fn parse_file_directives(path: &std::path::Path, color: bool) -> Result<Directives, String> {
    fn rec(
        path: &std::path::Path,
        visited: &mut std::collections::HashSet<std::path::PathBuf>,
        out: &mut Directives,
        color: bool,
    ) -> Result<(), String> {
        let canon = path
            .canonicalize()
            .map_err(|e| format!("can't read `{}`: {}", path.display(), e))?;
        if !visited.insert(canon) {
            return Ok(());
        }
        let src = std::fs::read_to_string(path)
            .map_err(|e| format!("can't read `{}`: {}", path.display(), e))?;
        let name = path.display().to_string();
        let items = parse_items(&src).map_err(|d| render(&name, &src, &d, color))?;
        let part = split_items(items).map_err(|d| render(&name, &src, &d, color))?;
        for (r, _) in &part.requires {
            if !out.requires.contains(r) {
                out.requires.push(r.clone());
            }
        }
        for s in &part.sources {
            if !out.sources.contains(s) {
                out.sources.push(s.clone());
            }
        }
        for (import, _) in &part.imports {
            let child = match path.parent() {
                Some(parent) => parent.join(import),
                None => std::path::PathBuf::from(import),
            };
            rec(&child, visited, out, color)?;
        }
        Ok(())
    }
    let mut out = Directives::default();
    rec(path, &mut std::collections::HashSet::new(), &mut out, color)?;
    Ok(out)
}

/// Like [`parse_file`], also returning every loaded file's (display name,
/// source) — the entry first, then imports in load order — so callers can
/// present the whole multi-file program (e.g. the /program route).
pub fn parse_file_with_sources(
    path: &std::path::Path,
    color: bool,
) -> Result<(Program, Vec<(String, String)>), String> {
    struct Loaded {
        name: String,
        src: String,
        part: FilePart,
    }

    fn load_rec(
        path: &std::path::Path,
        visited: &mut std::collections::HashSet<std::path::PathBuf>,
        out: &mut Vec<Loaded>,
        color: bool,
    ) -> Result<(), String> {
        let canon = path
            .canonicalize()
            .map_err(|e| format!("can't read `{}`: {}", path.display(), e))?;
        if !visited.insert(canon) {
            return Ok(()); // import-once: diamonds merge, cycles terminate
        }
        let src = std::fs::read_to_string(path)
            .map_err(|e| format!("can't read `{}`: {}", path.display(), e))?;
        let name = path.display().to_string();
        let items = parse_items(&src).map_err(|d| render(&name, &src, &d, color))?;
        let part = split_items(items).map_err(|d| render(&name, &src, &d, color))?;
        for (import, _) in &part.imports {
            let child = match path.parent() {
                Some(parent) => parent.join(import),
                None => std::path::PathBuf::from(import),
            };
            load_rec(&child, visited, out, color)?;
        }
        out.push(Loaded { name, src, part });
        Ok(())
    }

    let mut files: Vec<Loaded> = Vec::new();
    load_rec(
        path,
        &mut std::collections::HashSet::new(),
        &mut files,
        color,
    )?;

    // Merge: identical re-declarations collapse (diamond imports), conflicts
    // are errors naming both files.
    let decl_eq = |a: &RelDecl, b: &RelDecl| {
        a.name() == b.name()
            && a.arity() == b.arity()
            && a.attributes()
                .iter()
                .zip(b.attributes())
                .all(|(x, y)| x.name() == y.name() && x.data_type() == y.data_type())
            && a.force_serve() == b.force_serve()
            && a.order_by() == b.order_by()
            && a.limit() == b.limit()
            && a.merge() == b.merge()
            && a.path() == b.path()
    };
    let mut edbs: Vec<RelDecl> = Vec::new();
    let mut idbs: Vec<RelDecl> = Vec::new();
    let mut owner: std::collections::HashMap<String, (bool, usize, String)> =
        std::collections::HashMap::new();
    let mut rules: Vec<FLRule> = Vec::new();
    let mut rule_spans: Vec<(usize, Range<usize>)> = Vec::new();

    for (fi, file) in files.iter().enumerate() {
        for (is_edb, decls) in [(true, &file.part.edbs), (false, &file.part.idbs)] {
            for decl in decls {
                if let Some((seen_edb, idx, seen_file)) = owner.get(decl.name()) {
                    let existing = if *seen_edb { &edbs[*idx] } else { &idbs[*idx] };
                    if *seen_edb == is_edb && decl_eq(existing, decl) {
                        continue; // identical re-declaration (diamond import)
                    }
                    return Err(format!(
                        "conflicting declarations of `{}`: `{}` and `{}` disagree",
                        decl.name(),
                        seen_file,
                        file.name
                    ));
                }
                let list = if is_edb { &mut edbs } else { &mut idbs };
                owner.insert(
                    decl.name().to_string(),
                    (is_edb, list.len(), file.name.clone()),
                );
                list.push(decl.clone());
            }
        }
        for (rule, span) in file.part.rules.iter().zip(&file.part.rule_spans) {
            rules.push(rule.clone());
            rule_spans.push((fi, span.clone()));
        }
    }

    let program = Program::try_new(edbs, idbs, rules).map_err(|e| {
        let (fi, span) = e
            .rule
            .and_then(|i| rule_spans.get(i).cloned())
            .unwrap_or((files.len().saturating_sub(1), 0..1));
        let file = &files[fi];
        let message = match e.message.split(". In rule:").next() {
            Some(head) if head.len() < e.message.len() => head.to_string(),
            _ => e.message.clone(),
        };
        render(
            &file.name,
            &file.src,
            &[Diagnostic::new(span, message, "in this rule")],
            color,
        )
    })?;
    // Entry first (load_rec pushes it LAST — imports load depth-first), then
    // the imported files in load order.
    let mut sources: Vec<(String, String)> = Vec::with_capacity(files.len());
    if let Some(entry) = files.pop() {
        sources.push((entry.name, entry.src));
    }
    for f in files {
        sources.push((f.name, f.src));
    }
    Ok((program, sources))
}

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
// The parser (over the token stream)
// ---------------------------------------------------------------------------

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
    ("ln", BuiltinOp::Ln),
    ("exp", BuiltinOp::Exp),
    ("sqrt", BuiltinOp::Sqrt),
    ("pow", BuiltinOp::Pow),
    ("abs", BuiltinOp::Abs),
    ("similarity", BuiltinOp::Similarity),
];

const AGGREGATES: &[(&str, AggregationOperator)] = &[
    ("count", AggregationOperator::Count),
    ("sum", AggregationOperator::Sum),
    ("avg", AggregationOperator::Avg),
    ("min", AggregationOperator::Min),
    ("max", AggregationOperator::Max),
];

/// The parsers are generic over any token input carrying byte spans; `parse`
/// instantiates them with the mapped logos token slice.
trait TokenInput<'a>: ValueInput<'a, Token = Token<'a>, Span = SimpleSpan> {}
impl<'a, I: ValueInput<'a, Token = Token<'a>, Span = SimpleSpan>> TokenInput<'a> for I {}

type Extra<'a> = extra::Err<Rich<'a, Token<'a>, SimpleSpan>>;

fn ident<'a, I: TokenInput<'a>>() -> impl Parser<'a, I, &'a str, Extra<'a>> + Clone {
    select! { Token::Ident(s) => s }
}

/// Numeric or string constant, with optional unary sign on numbers (the lexer
/// produces unsigned literals so `X-5` stays a subtraction chain).
fn constant<'a, I: TokenInput<'a>>() -> impl Parser<'a, I, Const, Extra<'a>> + Clone {
    let sign = choice((just(Token::Minus).to(-1i64), just(Token::Plus).to(1i64)))
        .or_not()
        .map(|s| s.unwrap_or(1));
    let number = sign.then(select! {
        Token::Int(s) => (s, false),
        Token::Float(s) => (s, true),
    });
    let number = number.try_map(|(sign, (digits, is_float)), span| {
        if is_float {
            let v: f64 = digits
                .parse()
                .map_err(|_| Rich::custom(span, "float literal out of range"))?;
            Ok(Const::Float((sign as f64 * v).to_bits() as i64))
        } else {
            let v: i128 = digits
                .parse()
                .map_err(|_| Rich::custom(span, "integer literal out of range"))?;
            let v = i64::try_from(sign as i128 * v)
                .map_err(|_| Rich::custom(span, "integer literal out of range"))?;
            Ok(Const::Integer(v))
        }
    });
    let string = select! { Token::Str(s) => Const::Text(s.to_string()) };
    number.or(string)
}

/// An arithmetic expression (left-to-right chain of factors).
fn arithmetic<'a, I: TokenInput<'a>>() -> impl Parser<'a, I, Arithmetic, Extra<'a>> + Clone {
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
                    .separated_by(just(Token::Comma))
                    .at_least(1)
                    .collect::<Vec<_>>()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map(|(op, args)| Factor::Builtin(op, args));

        let paren = arith
            .delimited_by(just(Token::LParen), just(Token::RParen))
            .map(|a| Factor::Paren(Box::new(a)));

        let variable = ident().map(|s: &str| Factor::Var(s.to_string()));

        let factor = choice((builtin_call, paren, constant().map(Factor::Const), variable));

        let op = choice((
            just(Token::Plus).to(ArithmeticOperator::Plus),
            just(Token::Minus).to(ArithmeticOperator::Minus),
            just(Token::Star).to(ArithmeticOperator::Multiply),
            just(Token::Slash).to(ArithmeticOperator::Divide),
            just(Token::Percent).to(ArithmeticOperator::Modulo),
            just(Token::Amp).to(ArithmeticOperator::BitAnd),
            just(Token::Pipe).to(ArithmeticOperator::BitOr),
            just(Token::Caret).to(ArithmeticOperator::BitXor),
        ));

        factor
            .clone()
            .then(op.then(factor).repeated().collect::<Vec<_>>())
            .map(|(init, rest)| Arithmetic::new(init, rest))
    })
}

fn comparison_op<'a, I: TokenInput<'a>>(
) -> impl Parser<'a, I, ComparisonOperator, Extra<'a>> + Clone {
    select! {
        Token::Ne => ComparisonOperator::NotEquals,
        Token::Ge => ComparisonOperator::GreaterEqualThan,
        Token::Le => ComparisonOperator::LessEqualThan,
        Token::Eq => ComparisonOperator::Equals,
        Token::Gt => ComparisonOperator::GreaterThan,
        Token::Lt => ComparisonOperator::LessThan,
    }
}

fn atom<'a, I: TokenInput<'a>>() -> impl Parser<'a, I, Atom, Extra<'a>> + Clone {
    let arg = choice((
        just(Token::Underscore).to(AtomArg::Placeholder),
        constant().map(AtomArg::Const),
        ident().map(|s: &str| AtomArg::Var(s.to_string())),
    ));
    ident()
        .then(
            arg.separated_by(just(Token::Comma))
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .map(|(name, args): (&str, Vec<AtomArg>)| Atom::from_str(name, args))
}

fn predicate<'a, I: TokenInput<'a>>() -> impl Parser<'a, I, Predicate, Extra<'a>> + Clone {
    let compare =
        arithmetic()
            .then(comparison_op())
            .then(arithmetic())
            .map(|((left, op), right)| {
                Predicate::ComparePredicate(ComparisonExpr::new(left, op, right))
            });
    let negated = just(Token::Bang)
        .ignore_then(atom())
        .map(Predicate::NegatedAtomPredicate);
    // compare first (`starts_with(S, P) = 1` must not be swallowed as an
    // atom); it fails fast on a real atom because no operator follows.
    choice((compare, negated, atom().map(Predicate::AtomPredicate)))
}

fn head<'a, I: TokenInput<'a>>() -> impl Parser<'a, I, Head, Extra<'a>> + Clone {
    let aggregate = ident()
        .try_map(|name: &str, span| {
            AGGREGATES
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, op)| *op)
                .ok_or_else(|| Rich::custom(span, format!("`{}` is not an aggregate", name)))
        })
        .then(arithmetic().delimited_by(just(Token::LParen), just(Token::RParen)))
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
                .separated_by(just(Token::Comma))
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .map(|(name, args): (&str, Vec<HeadArg>)| Head::new(name.to_string(), args))
}

fn rule<'a, I: TokenInput<'a>>() -> impl Parser<'a, I, FLRule, Extra<'a>> + Clone {
    let optimize = select! {
        Token::PlanKw => (true, false),
        Token::SipKw => (false, true),
        Token::OptimizeKw => (true, true),
    };
    head()
        .then_ignore(just(Token::Turnstile))
        .then(
            predicate()
                .separated_by(just(Token::Comma))
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .then_ignore(just(Token::Dot))
        .then(optimize.or_not())
        .map(|((head, rhs), opt)| {
            let (is_planning, is_sip) = opt.unwrap_or((false, false));
            FLRule::new(head, rhs, is_planning, is_sip)
        })
}

fn decl<'a, I: TokenInput<'a>>() -> impl Parser<'a, I, RelDecl, Extra<'a>> + Clone {
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
        .then_ignore(just(Token::Colon))
        .then(data_type)
        .map(|(name, dt): (&str, DataType)| Attribute::new(name, dt));
    // `.input Arc.csv` / `.output result.facts`.
    let file_path = select! { Token::FilePath(s) => s };
    let io = choice((
        just(Token::InputKw).ignore_then(file_path),
        just(Token::OutputKw).ignore_then(file_path),
    ));
    // Presentation annotations: `order_by(col [desc|asc], ...)` and
    // `limit(N)`, in either order after the optional io path. They shape how
    // a served relation's rows are ORDERED/CAPPED at the query API — the
    // relation itself stays an unordered set. Contextual keywords (plain
    // idents), so no lexer impact.
    let order_col = ident().then(
        ident()
            .try_map(|dir: &str, span| match dir {
                "desc" => Ok(true),
                "asc" => Ok(false),
                other => Err(Rich::custom(
                    span,
                    format!("expected `asc` or `desc`, found `{}`", other),
                )),
            })
            .or_not(),
    );
    let order_by = ident().filter(|s: &&str| *s == "order_by").ignore_then(
        order_col
            .separated_by(just(Token::Comma))
            .at_least(1)
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LParen), just(Token::RParen)),
    );
    let limit = ident().filter(|s: &&str| *s == "limit").ignore_then(
        select! { Token::Int(s) => s }
            .try_map(|s: &str, span| {
                s.parse::<usize>()
                    .map_err(|_| Rich::custom(span, format!("bad limit `{}`", s)))
            })
            .delimited_by(just(Token::LParen), just(Token::RParen)),
    );
    // `merge(min|max)`: the relation is a function from its leading columns to
    // its last, folded by a lattice join (see `RelDecl::merge`). Only the two
    // lattice joins are accepted — a non-idempotent fold would not converge
    // inside a recursive fixpoint, which is the whole point of declaring it.
    // The operator is taken as a bare ident and validated in the decl's own
    // `try_map` below (like order_by's column names). Rejecting it inside this
    // sub-parser would only end the `shape.repeated()` run — chumsky discards
    // a failed alternative's error when the enclosing parser still succeeds,
    // so the real message would be lost behind a bogus later syntax error.
    let merge = ident()
        .filter(|s: &&str| *s == "merge")
        .ignore_then(ident().delimited_by(just(Token::LParen), just(Token::RParen)));
    enum Shape<'a> {
        Order(Vec<(&'a str, Option<bool>)>),
        Limit(usize),
        Merge(&'a str),
    }
    let shape = choice((
        order_by.map(Shape::Order),
        limit.map(Shape::Limit),
        merge.map(Shape::Merge),
    ));

    just(Token::DeclKw)
        .ignore_then(ident())
        .then(
            attribute
                .separated_by(just(Token::Comma))
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .then(io.or_not())
        .then(shape.repeated().collect::<Vec<_>>())
        .try_map(
            |(((name, attrs), path), shapes): (
                ((&str, Vec<Attribute>), Option<&str>),
                Vec<Shape>,
            ),
             span| {
                let mut decl = RelDecl::new(name, attrs, path);
                let mut order: Vec<(usize, bool)> = Vec::new();
                let mut cap: Option<usize> = None;
                for s in shapes {
                    match s {
                        Shape::Order(cols) => {
                            for (col, desc) in cols {
                                let idx = decl
                                    .attributes()
                                    .iter()
                                    .position(|a| a.name() == col)
                                    .ok_or_else(|| {
                                        Rich::custom(
                                            span,
                                            format!(
                                                "order_by names unknown column `{}` of {}",
                                                col,
                                                decl.name()
                                            ),
                                        )
                                    })?;
                                order.push((idx, desc.unwrap_or(false)));
                            }
                        }
                        Shape::Limit(n) => cap = Some(n),
                        Shape::Merge(op) => {
                            let operator = match op {
                                "min" => AggregationOperator::Min,
                                "max" => AggregationOperator::Max,
                                other => {
                                    return Err(Rich::custom(
                                        span,
                                        format!(
                                            "merge takes `min` or `max` (the lattice joins),                                              found `{}` — a non-idempotent fold would not                                              converge inside a recursive fixpoint",
                                            other
                                        ),
                                    ))
                                }
                            };
                            decl.set_merge(Some(operator));
                        }
                    }
                }
                decl.set_output_shape(order, cap);
                Ok(decl)
            },
        )
}

fn parser<'a, I: TokenInput<'a>>() -> impl Parser<'a, I, Vec<Spanned<Item>>, Extra<'a>> {
    let section = select! {
        Token::InSection => Item::Section(SectionKind::In),
        Token::PrintSizeSection => Item::Section(SectionKind::PrintSize),
        Token::OutSection => Item::Section(SectionKind::Out),
        Token::RuleSection => Item::RuleMarker,
    };
    let import = just(Token::ImportKw)
        .ignore_then(select! { Token::Str(s) => s })
        .map(|quoted: &str| {
            // The lexer keeps the surrounding quotes; paths take no escapes.
            Item::Import(quoted[1..quoted.len() - 1].to_string())
        });
    // `.require duckdb` — a bare name, because that is how plugins are named
    // everywhere else (the `--source` spec, the error messages, the crate).
    let require = just(Token::RequireKw)
        .ignore_then(select! { Token::Ident(s) => s })
        .map(|name: &str| Item::Require(name.to_string()));
    // `.source rel = provider(k = v, ...)`, or without `rel =` for a provider
    // that feeds several relations at once.
    let cfg_value = choice((
        select! { Token::RawStr(s) => s.to_string() },
        select! { Token::Str(s) => s[1..s.len() - 1].to_string() },
    ));
    let cfg_pair = select! { Token::Ident(s) => s.to_string() }
        .then_ignore(just(Token::Eq))
        .then(cfg_value);
    let source = just(Token::SourceKw)
        .ignore_then(
            select! { Token::Ident(s) => s.to_string() }
                .then_ignore(just(Token::Eq))
                .or_not(),
        )
        .then(select! { Token::Ident(s) => s.to_string() })
        .then(
            cfg_pair
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .or_not(),
        )
        .map(|((relation, provider), config)| {
            Item::Source(SourceSpec {
                relation,
                provider,
                config: config.unwrap_or_default(),
            })
        });
    let item = choice((
        import,
        require,
        source,
        section,
        decl().map(Item::Decl),
        rule().map(Item::Rule),
    ));
    item.map_with(|item, e| {
        let span: SimpleSpan = e.span();
        (item, span.into_range())
    })
    .repeated()
    .at_least(1)
    .collect::<Vec<_>>()
    .then_ignore(end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexer_longest_match_disambiguates() {
        let toks: Vec<Token> = Token::lexer(".input x.csv .in != ! _ _x 1.5 12")
            .map(|t| t.unwrap())
            .collect();
        assert_eq!(
            toks,
            vec![
                Token::InputKw,
                Token::FilePath("x.csv"),
                Token::InSection,
                Token::Ne,
                Token::Bang,
                Token::Underscore,
                Token::Ident("_x"),
                Token::Float("1.5"),
                Token::Int("12"),
            ]
        );
    }

    #[test]
    fn lexer_skips_comments_and_flags_junk() {
        let toks: Vec<_> = Token::lexer("a // c(mment\n# another\nb").collect();
        assert_eq!(toks, vec![Ok(Token::Ident("a")), Ok(Token::Ident("b"))]);
        assert!(Token::lexer("a ∞ b").any(|t| t.is_err()));
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;

    fn dirs(src: &str) -> Directives {
        parse_with_directives(src).expect("parses").1
    }

    const BODY: &str =
        "\n.in\n.decl a(x: number)\n.out\n.decl b(x: number)\n.rule\nb(X) :- a(X).\n";

    #[test]
    fn a_source_binds_a_relation_to_a_provider_with_config() {
        let d = dirs(&format!(
            "{}{}",
            r#".source a = csv(path = "x.csv", types = "integer")"#, BODY
        ));
        assert_eq!(
            d.sources,
            vec![SourceSpec {
                relation: Some("a".into()),
                provider: "csv".into(),
                config: vec![
                    ("path".into(), "x.csv".into()),
                    ("types".into(), "integer".into())
                ],
            }]
        );
    }

    #[test]
    fn a_multi_output_provider_needs_no_relation() {
        // treesitter feeds several relations from one source, which is why
        // `add_source` takes an Option and why this form has to parse.
        let d = dirs(&format!("{}{}", ".source treesitter(root = \".\")", BODY));
        assert_eq!(d.sources[0].relation, None);
        assert_eq!(d.sources[0].provider, "treesitter");
    }

    #[test]
    fn a_provider_with_no_config_parses() {
        let d = dirs(&format!("{}{}", ".source hyperliquid()", BODY));
        assert_eq!(d.sources[0].config, Vec::<(String, String)>::new());
    }

    #[test]
    fn embedded_sql_keeps_its_quotes_and_newlines() {
        // The whole point of the raw string: SQL is full of quotes and spans
        // lines, and escaping it onto one line makes the common case the ugly
        // one.
        let src = format!(
            "{}{}",
            ".source p = duckdb(sql = \"\"\"\n  SELECT 'x' AS a FROM \"t\"\n\"\"\")", BODY
        );
        let d = dirs(&src);
        assert_eq!(d.sources[0].config[0].1, "\n  SELECT 'x' AS a FROM \"t\"\n");
    }

    #[test]
    fn requires_and_sources_are_collected_together() {
        let d = dirs(&format!(
            "{}{}",
            ".require duckdb\n.source a = duckdb(sql = \"SELECT 1\")", BODY
        ));
        assert_eq!(d.requires, vec!["duckdb".to_string()]);
        assert_eq!(d.sources.len(), 1);
    }
}

#[cfg(test)]
mod raw_string_tests {
    use super::*;
    use logos::Logos;

    #[test]
    fn triple_quotes_win_over_the_empty_string() {
        // `"""` could lex as an empty `""` followed by a stray quote; logos
        // takes the longest match, and this pins that it does.
        let toks: Vec<_> = Token::lexer(r#""""a "b" c""""#).collect();
        assert_eq!(toks, vec![Ok(Token::RawStr("a \"b\" c"))]);
    }

    #[test]
    fn a_raw_string_keeps_newlines_and_needs_no_escapes() {
        let src = "\"\"\"\nSELECT a, 'x' FROM \"t\"\nWHERE b > 1\n\"\"\"";
        let toks: Vec<_> = Token::lexer(src).collect();
        assert_eq!(
            toks,
            vec![Ok(Token::RawStr(
                "\nSELECT a, 'x' FROM \"t\"\nWHERE b > 1\n"
            ))]
        );
    }

    #[test]
    fn ordinary_strings_still_lex() {
        let toks: Vec<_> = Token::lexer(r#""plain""#).collect();
        assert_eq!(toks, vec![Ok(Token::Str("\"plain\""))]);
    }
}
