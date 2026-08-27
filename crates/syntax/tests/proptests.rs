//! Property-based tests for the parser.
//!
//! The flagship property generates random *valid* programs as source text —
//! random names (including keyword-containing ones like `counter`), arities,
//! nested parenthesised arithmetic, aggregates, negation, comparisons, string
//! constants, comments, and compact-vs-spaced layout — and checks the parsed
//! `Program` against the generator's own model: a structural oracle covering
//! every declaration, rule, head argument, body predicate and flag. Unlike the
//! retired pest cross-check this compares against *intent*, not another
//! implementation.
//!
//! A second property feeds arbitrary and mutated input and asserts the parser
//! (and the ariadne renderer) never panic — errors only.
//!
//! A third family covers **operator precedence**. The generator emits flat
//! multi-operator chains over all eight arithmetic operators, and the model
//! groups them with its own precedence-climbing routine ([`group`], written
//! from the level table alone) to build the `Arithmetic`/`Factor::Paren` tree
//! the parser is expected to produce. That is a genuine cross-check: the parser
//! is compared against an independent implementation of the same spec, not
//! against itself.

use parsing::arithmetic::{Arithmetic, ArithmeticOperator, BuiltinOp, Factor};
use proptest::prelude::*;
use proptest::strategy::BoxedStrategy;

// ---------------------------------------------------------------------------
// A model of a valid program, rendered to source text.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct GenProgram {
    edbs: Vec<GenDecl>,
    rules: Vec<GenRule>,
    /// Layout: compact tokens, sprinkle comments, use `.out` for some idbs.
    compact: bool,
    comments: bool,
}

#[derive(Debug, Clone)]
struct GenDecl {
    name: String,
    arity: usize,
}

#[derive(Debug, Clone)]
struct GenRule {
    head_name: String,
    head_args: Vec<GenHeadArg>,
    /// Indices into edbs + per-atom args. The FIRST atom is all-variables so
    /// the bound pool is never empty.
    body: Vec<GenAtom>,
    negated: Option<GenAtom>,
    compares: Vec<GenCompare>,
    force_serve: bool,
    plan_suffix: usize,
}

#[derive(Debug, Clone)]
enum GenHeadArg {
    Var(usize),
    Expr(GenExpr),
    /// A multi-operator chain, checked against the precedence oracle.
    Chain(GenChain),
    /// (op index into AGGS, bound-var index) — always emitted last.
    Agg(usize, usize),
}

#[derive(Debug, Clone)]
struct GenAtom {
    edb: usize,
    args: Vec<GenAtomArg>,
}

#[derive(Debug, Clone)]
enum GenAtomArg {
    Var(usize),
    Int(i64),
    Str(String),
    Placeholder,
}

#[derive(Debug, Clone)]
enum GenExpr {
    Var(usize),
    Int(i64),
    /// Parenthesised binary node.
    Node(Box<GenExpr>, usize, Box<GenExpr>),
}

#[derive(Debug, Clone)]
struct GenCompare {
    left: usize,
    op: usize,
    right: GenCmpRight,
}

#[derive(Debug, Clone)]
enum GenCmpRight {
    /// The original single-operator shape: `V + n`, or a bare integer. Its
    /// assertions (including `rest().len() == 1`) are the pre-precedence
    /// contract and stay exactly as they were.
    Simple { var: Option<usize>, add: i64 },
    /// A multi-operator chain, checked against the precedence oracle.
    Chain(GenChain),
}

const OPS: &[&str] = &["+", "-", "*", "/", "%"];
const CMPS: &[&str] = &["=", "!=", ">", ">=", "<", "<="];
const AGGS: &[&str] = &["count", "sum", "min", "max"];
const PLANS: &[&str] = &["", " .plan", " .sip", " .optimize"];

/// Names the grammar treats specially in some position; generated relation
/// names avoid exact collisions (containing them as substrings is fine and
/// deliberately exercised).
const RESERVED: &[&str] = &[
    "count",
    "sum",
    "min",
    "max",
    "number",
    "string",
    "float",
    "split_nth",
    "starts_with",
    "contains",
    "str_before",
    "replace",
    "before_last",
    "after_last",
    "concat",
    "extract_number",
    "date_epoch",
    "to_float",
    "round",
    "floor",
    "to_lower",
    "to_upper",
    "ln",
    "exp",
    "sqrt",
    "pow",
    "abs",
    "similarity",
];

fn var_name(i: usize) -> String {
    format!("V{}", i)
}

impl GenExpr {
    fn render(&self, bound: &[usize]) -> String {
        match self {
            GenExpr::Var(i) => var_name(bound[i % bound.len()]),
            GenExpr::Int(v) => v.to_string(),
            GenExpr::Node(l, op, r) => format!(
                "({} {} {})",
                l.render(bound),
                OPS[op % OPS.len()],
                r.render(bound)
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Multi-operator chains, and an independent precedence oracle.
// ---------------------------------------------------------------------------

/// The eight arithmetic operators with their binding level — 0 loosest, 4
/// tightest. This table is the spec transcribed on its own; the oracle below
/// derives the expected grouping from it and nothing else, so agreement with
/// the parser is real evidence rather than a tautology.
const CHAIN_OPS: &[(&str, usize)] = &[
    ("|", 0),
    ("^", 1),
    ("&", 2),
    ("+", 3),
    ("-", 3),
    ("*", 4),
    ("/", 4),
    ("%", 4),
];

const NUM_LEVELS: usize = 5;

fn chain_op_ast(i: usize) -> ArithmeticOperator {
    match CHAIN_OPS[i].0 {
        "|" => ArithmeticOperator::BitOr,
        "^" => ArithmeticOperator::BitXor,
        "&" => ArithmeticOperator::BitAnd,
        "+" => ArithmeticOperator::Plus,
        "-" => ArithmeticOperator::Minus,
        "*" => ArithmeticOperator::Multiply,
        "/" => ArithmeticOperator::Divide,
        "%" => ArithmeticOperator::Modulo,
        other => unreachable!("unmapped operator {}", other),
    }
}

/// A flat chain of operands joined by operators — the surface form, with no
/// grouping decided. `rest` is never empty, so a chain always has at least one
/// operator (a bare operand is not a chain).
#[derive(Debug, Clone)]
struct GenChain {
    first: GenOperand,
    rest: Vec<(usize, GenOperand)>,
}

/// Everything the generator will put in an operand slot. All of them evaluate
/// in integer mode, so a whole chain stays within one numeric mode and the
/// typing pass never rejects it.
#[derive(Debug, Clone)]
enum GenOperand {
    Var(usize),
    Int(i64),
    /// A user-written `(...)` sub-chain — always a real `Factor::Paren`, even
    /// around a single operand (the parser's `paren` rule never collapses).
    Paren(Box<GenChain>),
    /// `abs(<chain>)` — a builtin argument re-enters the grammar at the
    /// loosest level, so precedence must not leak across the parenthesis.
    Abs(Box<GenChain>),
    /// `round(to_float(<chain>))` — nested builtins wrapped around a chain,
    /// integer in and integer out.
    Round(Box<GenChain>),
}

/// Hold a sub-expression in a factor slot, exactly as the parser's `as_factor`
/// does: a chain with no operators is its bare factor, anything else is a
/// parenthesised sub-expression.
fn as_factor(a: Arithmetic) -> Factor {
    if a.rest().is_empty() {
        a.init().clone()
    } else {
        Factor::Paren(Box::new(a))
    }
}

/// The operand index ranges that `level`'s operators cut this sequence into,
/// or empty if no operator here belongs to `level`. Range `(s, e)` covers
/// `operands[s..e]` and the operators `ops[s..e - 1]`; the operator joining it
/// to the previous segment is `ops[s - 1]`.
fn segments(ops: &[usize], operand_count: usize, level: usize) -> Vec<(usize, usize)> {
    let cuts: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| CHAIN_OPS[**op].1 == level)
        .map(|(i, _)| i)
        .collect();
    if cuts.is_empty() {
        return Vec::new();
    }
    let mut segs = Vec::with_capacity(cuts.len() + 1);
    let mut start = 0;
    for cut in cuts {
        segs.push((start, cut + 1));
        start = cut + 1;
    }
    segs.push((start, operand_count));
    segs
}

/// The oracle: group a flat chain by precedence, loosest level first.
///
/// A level that owns no operator in this segment passes straight through to
/// the tighter one — the rule that keeps an unmixed chain flat and a bare
/// operand bare. A level that owns at least one emits a flat left-associative
/// chain whose operands are the tighter level's results.
fn group(ops: &[usize], operands: &[&GenOperand], bound: &[usize], level: usize) -> Arithmetic {
    if level == NUM_LEVELS {
        assert_eq!(operands.len(), 1, "every operator should have been cut");
        return Arithmetic::new(operands[0].factor(bound), Vec::new());
    }
    let segs = segments(ops, operands.len(), level);
    if segs.is_empty() {
        return group(ops, operands, bound, level + 1);
    }
    let sub = |(s, e): (usize, usize)| group(&ops[s..e - 1], &operands[s..e], bound, level + 1);
    let init = as_factor(sub(segs[0]));
    let rest = segs[1..]
        .iter()
        .map(|&(s, e)| (chain_op_ast(ops[s - 1]), as_factor(sub((s, e)))))
        .collect();
    Arithmetic::new(init, rest)
}

/// The same grouping, rendered back to source with the implied parentheses
/// written out. A segment of one operand is left alone — wrapping it would add
/// a `Factor::Paren` the implicit form does not have.
fn render_grouped(
    ops: &[usize],
    operands: &[&GenOperand],
    bound: &[usize],
    sep: &str,
    level: usize,
) -> String {
    if level == NUM_LEVELS {
        return operands[0].render(bound, sep);
    }
    let segs = segments(ops, operands.len(), level);
    if segs.is_empty() {
        return render_grouped(ops, operands, bound, sep, level + 1);
    }
    let piece = |(s, e): (usize, usize)| {
        let inner = render_grouped(&ops[s..e - 1], &operands[s..e], bound, sep, level + 1);
        if e - s > 1 {
            format!("({})", inner)
        } else {
            inner
        }
    };
    let mut out = piece(segs[0]);
    for &(s, e) in &segs[1..] {
        out.push_str(&format!("{sep}{}{sep}", CHAIN_OPS[ops[s - 1]].0));
        out.push_str(&piece((s, e)));
    }
    out
}

impl GenOperand {
    fn render(&self, bound: &[usize], sep: &str) -> String {
        match self {
            GenOperand::Var(i) => var_name(bound[i % bound.len()]),
            GenOperand::Int(v) => v.to_string(),
            GenOperand::Paren(c) => format!("({})", c.render(bound, sep)),
            GenOperand::Abs(c) => format!("abs({})", c.render(bound, sep)),
            GenOperand::Round(c) => format!("round(to_float({}))", c.render(bound, sep)),
        }
    }

    fn factor(&self, bound: &[usize]) -> Factor {
        match self {
            GenOperand::Var(i) => Factor::Var(var_name(bound[i % bound.len()])),
            GenOperand::Int(v) => Factor::Const(parsing::rule::Const::Integer(*v)),
            GenOperand::Paren(c) => Factor::Paren(Box::new(c.expected(bound))),
            // The typing pass resolves the polymorphic `abs` to its integer
            // specialization, so that — not `Abs` — is what lands in the AST.
            GenOperand::Abs(c) => {
                Factor::Builtin(BuiltinOp::AbsInt, vec![as_factor(c.expected(bound))])
            }
            GenOperand::Round(c) => Factor::Builtin(
                BuiltinOp::Round,
                vec![Factor::Builtin(
                    BuiltinOp::ToFloat,
                    vec![as_factor(c.expected(bound))],
                )],
            ),
        }
    }

    /// Variables this operand mentions, left to right, duplicates kept.
    fn vars(&self, bound: &[usize]) -> Vec<String> {
        match self {
            GenOperand::Var(i) => vec![var_name(bound[i % bound.len()])],
            GenOperand::Int(_) => Vec::new(),
            GenOperand::Paren(c) | GenOperand::Abs(c) | GenOperand::Round(c) => c.vars(bound),
        }
    }
}

impl GenChain {
    fn flat(&self) -> (Vec<usize>, Vec<&GenOperand>) {
        let mut ops = Vec::with_capacity(self.rest.len());
        let mut operands = Vec::with_capacity(self.rest.len() + 1);
        operands.push(&self.first);
        for (op, operand) in &self.rest {
            ops.push(*op);
            operands.push(operand);
        }
        (ops, operands)
    }

    /// The chain as source text, with no grouping parentheses.
    fn render(&self, bound: &[usize], sep: &str) -> String {
        let mut out = self.first.render(bound, sep);
        for (op, operand) in &self.rest {
            out.push_str(&format!("{sep}{}{sep}", CHAIN_OPS[*op].0));
            out.push_str(&operand.render(bound, sep));
        }
        out
    }

    /// The chain as source text with the precedence-implied parentheses
    /// written out explicitly.
    fn render_explicit(&self, bound: &[usize], sep: &str) -> String {
        let (ops, operands) = self.flat();
        render_grouped(&ops, &operands, bound, sep, 0)
    }

    /// The AST the parser must produce.
    fn expected(&self, bound: &[usize]) -> Arithmetic {
        let (ops, operands) = self.flat();
        group(&ops, &operands, bound, 0)
    }

    /// The flat left-to-right variable sequence — what `ordered_vars()` on the
    /// parsed chain must equal, and the invariant the catalog/planning
    /// positional lowering consumes through a single shared cursor.
    fn vars(&self, bound: &[usize]) -> Vec<String> {
        let mut out = self.first.vars(bound);
        for (_, operand) in &self.rest {
            out.extend(operand.vars(bound));
        }
        out
    }

    /// Record which non-leaf operand forms appear anywhere in this chain:
    /// slot 0 a `(...)` sub-chain, 1 an `abs(...)` call, 2 a
    /// `round(to_float(...))` call.
    fn operand_kinds(&self, seen: &mut [bool; 3]) {
        for operand in std::iter::once(&self.first).chain(self.rest.iter().map(|(_, o)| o)) {
            let (slot, inner) = match operand {
                GenOperand::Var(_) | GenOperand::Int(_) => continue,
                GenOperand::Paren(c) => (0, c),
                GenOperand::Abs(c) => (1, c),
                GenOperand::Round(c) => (2, c),
            };
            seen[slot] = true;
            inner.operand_kinds(seen);
        }
    }

    /// Does this chain mix operators from two different levels anywhere? Used
    /// by the generator-coverage guard: a generator that only ever produced
    /// single-level chains could not catch a precedence bug.
    fn mixes_levels(&self) -> bool {
        let levels: Vec<usize> = self.rest.iter().map(|(op, _)| CHAIN_OPS[*op].1).collect();
        if levels.windows(2).any(|w| w[0] != w[1]) {
            return true;
        }
        std::iter::once(&self.first)
            .chain(self.rest.iter().map(|(_, o)| o))
            .any(|o| match o {
                GenOperand::Var(_) | GenOperand::Int(_) => false,
                GenOperand::Paren(c) | GenOperand::Abs(c) | GenOperand::Round(c) => {
                    c.mixes_levels()
                }
            })
    }
}

impl GenProgram {
    fn source(&self) -> String {
        let mut out = String::new();
        let sep = if self.compact { "" } else { " " };
        let mut comment_counter = 0;
        let mut comment = |out: &mut String| {
            if self.comments {
                comment_counter += 1;
                let marker = if comment_counter % 2 == 0 { "//" } else { "#" };
                out.push_str(&format!(
                    "{} comment {} :- not code.\n",
                    marker, comment_counter
                ));
            }
        };

        comment(&mut out);
        out.push_str(".in\n");
        for edb in &self.edbs {
            let cols: Vec<String> = (0..edb.arity).map(|i| format!("c{}: number", i)).collect();
            out.push_str(&format!(".decl {}({})\n", edb.name, cols.join(", ")));
            comment(&mut out);
        }

        for (ri, rule) in self.rules.iter().enumerate() {
            let section = if rule.force_serve {
                ".out"
            } else {
                ".printsize"
            };
            let cols: Vec<String> = (0..rule.head_args.len())
                .map(|i| format!("h{}: number", i))
                .collect();
            out.push_str(&format!(
                "{}\n.decl h{}_{}({})\n",
                section,
                ri,
                rule.head_name,
                cols.join(", ")
            ));
        }

        out.push_str(".rule\n");
        for (ri, rule) in self.rules.iter().enumerate() {
            comment(&mut out);
            let bound = rule.bound_vars(&self.edbs);
            let head_args: Vec<String> = rule
                .head_args
                .iter()
                .map(|a| match a {
                    GenHeadArg::Var(i) => var_name(bound[i % bound.len()]),
                    GenHeadArg::Expr(e) => e.render(&bound),
                    GenHeadArg::Chain(c) => c.render(&bound, sep),
                    GenHeadArg::Agg(op, v) => format!(
                        "{}({})",
                        AGGS[op % AGGS.len()],
                        var_name(bound[v % bound.len()])
                    ),
                })
                .collect();
            let mut preds: Vec<String> = rule
                .body
                .iter()
                .map(|a| a.render(&self.edbs, sep))
                .collect();
            if let Some(neg) = &rule.negated {
                // Negated atoms may only use bound variables.
                preds.push(format!("!{}", neg.render_bound(&self.edbs, &bound, sep)));
            }
            for cmp in &rule.compares {
                let left = var_name(bound[cmp.left % bound.len()]);
                let right = match &cmp.right {
                    GenCmpRight::Simple { var: Some(v), add } => {
                        format!("{}{sep}+{sep}{}", var_name(bound[v % bound.len()]), add)
                    }
                    GenCmpRight::Simple { var: None, add } => add.to_string(),
                    GenCmpRight::Chain(c) => c.render(&bound, sep),
                };
                preds.push(format!(
                    "{}{sep}{}{sep}{}",
                    left,
                    CMPS[cmp.op % CMPS.len()],
                    right
                ));
            }
            out.push_str(&format!(
                "h{}_{}({}){sep}:-{sep}{}.{}\n",
                ri,
                rule.head_name,
                head_args.join(&format!(",{sep}")),
                preds.join(&format!(",{sep}")),
                PLANS[rule.plan_suffix % PLANS.len()]
            ));
        }
        out
    }
}

impl GenRule {
    /// Variable ids bound by positive body atoms, in first-seen order.
    fn bound_vars(&self, edbs: &[GenDecl]) -> Vec<usize> {
        let mut bound = Vec::new();
        for atom in &self.body {
            let arity = edbs[atom.edb % edbs.len()].arity;
            for arg in atom.args.iter().take(arity) {
                if let GenAtomArg::Var(v) = arg {
                    if !bound.contains(v) {
                        bound.push(*v);
                    }
                }
            }
        }
        bound
    }
}

impl GenAtom {
    fn render(&self, edbs: &[GenDecl], sep: &str) -> String {
        let decl = &edbs[self.edb % edbs.len()];
        let args: Vec<String> = (0..decl.arity)
            .map(|i| match self.args.get(i) {
                Some(GenAtomArg::Var(v)) => var_name(*v),
                Some(GenAtomArg::Int(n)) => n.to_string(),
                Some(GenAtomArg::Str(s)) => format!("\"{}\"", s),
                Some(GenAtomArg::Placeholder) | None => "_".to_string(),
            })
            .collect();
        format!("{}({})", decl.name, args.join(&format!(",{sep}")))
    }

    /// Like `render` but variables are remapped into the bound pool (negated
    /// atoms must not introduce new variables).
    fn render_bound(&self, edbs: &[GenDecl], bound: &[usize], sep: &str) -> String {
        let decl = &edbs[self.edb % edbs.len()];
        let args: Vec<String> = (0..decl.arity)
            .map(|i| match self.args.get(i) {
                Some(GenAtomArg::Var(v)) => var_name(bound[v % bound.len()]),
                Some(GenAtomArg::Int(n)) => n.to_string(),
                Some(GenAtomArg::Str(s)) => format!("\"{}\"", s),
                Some(GenAtomArg::Placeholder) | None => "_".to_string(),
            })
            .collect();
        format!("{}({})", decl.name, args.join(&format!(",{sep}")))
    }
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn ident_strategy() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,7}".prop_filter("reserved", |s| !RESERVED.contains(&s.as_str()))
}

fn expr_strategy() -> impl Strategy<Value = GenExpr> {
    let leaf = prop_oneof![
        (0usize..4).prop_map(GenExpr::Var),
        (-1000i64..1000).prop_map(GenExpr::Int),
    ];
    leaf.prop_recursive(3, 12, 2, |inner| {
        (inner.clone(), 0usize..OPS.len(), inner)
            .prop_map(|(l, op, r)| GenExpr::Node(Box::new(l), op, Box::new(r)))
    })
}

/// A flat chain of 2..=5 operands over all eight operators. Arity and operator
/// mix are both random, so a single generated chain routinely spans three or
/// four precedence levels.
fn chain_of(operand: BoxedStrategy<GenOperand>) -> impl Strategy<Value = GenChain> {
    (
        operand.clone(),
        proptest::collection::vec((0usize..CHAIN_OPS.len(), operand), 1..5),
    )
        .prop_map(|(first, rest)| GenChain { first, rest })
}

fn operand_strategy() -> impl Strategy<Value = GenOperand> {
    let leaf = prop_oneof![
        3 => (0usize..4).prop_map(GenOperand::Var),
        1 => (-1000i64..1000).prop_map(GenOperand::Int),
    ];
    // Occasionally an operand is itself a bracketed sub-chain or a builtin
    // call over one — the two ways the grammar re-enters at the loosest level.
    leaf.prop_recursive(2, 24, 4, |inner| {
        prop_oneof![
            6 => inner.clone(),
            2 => chain_of(inner.clone()).prop_map(|c| GenOperand::Paren(Box::new(c))),
            1 => chain_of(inner.clone()).prop_map(|c| GenOperand::Abs(Box::new(c))),
            1 => chain_of(inner).prop_map(|c| GenOperand::Round(Box::new(c))),
        ]
    })
}

fn chain_strategy() -> impl Strategy<Value = GenChain> {
    chain_of(operand_strategy().boxed())
}

fn atom_arg_strategy() -> impl Strategy<Value = GenAtomArg> {
    prop_oneof![
        3 => (0usize..6).prop_map(GenAtomArg::Var),
        1 => (-1000i64..1000).prop_map(GenAtomArg::Int),
        1 => "[a-zA-Z0-9 _.-]{0,10}".prop_map(GenAtomArg::Str),
        1 => Just(GenAtomArg::Placeholder),
    ]
}

fn atom_strategy(all_vars: bool) -> impl Strategy<Value = GenAtom> {
    let arg = if all_vars {
        (0usize..6).prop_map(GenAtomArg::Var).boxed()
    } else {
        atom_arg_strategy().boxed()
    };
    (any::<usize>(), proptest::collection::vec(arg, 1..5))
        .prop_map(|(edb, args)| GenAtom { edb, args })
}

fn rule_strategy() -> impl Strategy<Value = GenRule> {
    let head_arg = prop_oneof![
        3 => (0usize..4).prop_map(GenHeadArg::Var),
        1 => expr_strategy().prop_map(GenHeadArg::Expr),
        1 => chain_strategy().prop_map(GenHeadArg::Chain),
    ];
    let compare = (
        0usize..4,
        0usize..CMPS.len(),
        prop_oneof![
            2 => (proptest::option::of(0usize..4), -100i64..100)
                .prop_map(|(var, add)| GenCmpRight::Simple { var, add }),
            1 => chain_strategy().prop_map(GenCmpRight::Chain),
        ],
    )
        .prop_map(|(left, op, right)| GenCompare { left, op, right });
    (
        ident_strategy(),
        proptest::collection::vec(head_arg, 1..4),
        proptest::option::of((0usize..AGGS.len(), 0usize..4)),
        atom_strategy(true),
        proptest::collection::vec(atom_strategy(false), 0..3),
        proptest::option::of(atom_strategy(false)),
        proptest::collection::vec(compare, 0..3),
        any::<bool>(),
        0usize..PLANS.len(),
    )
        .prop_map(
            |(
                head_name,
                mut head_args,
                agg,
                first,
                rest,
                negated,
                compares,
                force_serve,
                plan_suffix,
            )| {
                if let Some((op, v)) = agg {
                    head_args.push(GenHeadArg::Agg(op, v));
                }
                let mut body = vec![first];
                body.extend(rest);
                GenRule {
                    head_name,
                    head_args,
                    body,
                    negated,
                    compares,
                    force_serve,
                    plan_suffix,
                }
            },
        )
}

fn program_strategy() -> impl Strategy<Value = GenProgram> {
    (
        proptest::collection::vec((ident_strategy(), 1usize..5), 1..4),
        proptest::collection::vec(rule_strategy(), 1..4),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(edb_names, rules, compact, comments)| {
            let edbs = edb_names
                .into_iter()
                .enumerate()
                .map(|(i, (name, arity))| GenDecl {
                    name: format!("e{}_{}", i, name),
                    arity,
                })
                .collect();
            GenProgram {
                edbs,
                rules,
                compact,
                comments,
            }
        })
}

// ---------------------------------------------------------------------------
// The structural oracle: the parsed Program must match the model.
// ---------------------------------------------------------------------------

/// CMPS index -> expected operator, aligned with the rendering table.
const CMP_OPS: &[parsing::compare::ComparisonOperator] = &[
    parsing::compare::ComparisonOperator::Equals,
    parsing::compare::ComparisonOperator::NotEquals,
    parsing::compare::ComparisonOperator::GreaterThan,
    parsing::compare::ComparisonOperator::GreaterEqualThan,
    parsing::compare::ComparisonOperator::LessThan,
    parsing::compare::ComparisonOperator::LessEqualThan,
];

/// AGGS index -> expected operator.
const AGG_OPS: &[parsing::aggregation::AggregationOperator] = &[
    parsing::aggregation::AggregationOperator::Count,
    parsing::aggregation::AggregationOperator::Sum,
    parsing::aggregation::AggregationOperator::Min,
    parsing::aggregation::AggregationOperator::Max,
];

impl GenProgram {
    fn check(&self, p: &parsing::parser::Program) -> Result<(), TestCaseError> {
        use parsing::head::HeadArg;
        use parsing::rule::{AtomArg, Const, Predicate};

        // Declarations: names, arities, all-number columns, force-serve.
        prop_assert_eq!(p.edbs().len(), self.edbs.len());
        for (decl, gen) in p.edbs().iter().zip(&self.edbs) {
            prop_assert_eq!(decl.name(), gen.name.as_str());
            prop_assert_eq!(decl.arity(), gen.arity);
        }
        prop_assert_eq!(p.idbs().len(), self.rules.len());
        prop_assert_eq!(p.rules().len(), self.rules.len());
        for (ri, (idb, gen)) in p.idbs().iter().zip(&self.rules).enumerate() {
            let expect_name = format!("h{}_{}", ri, gen.head_name);
            prop_assert_eq!(idb.name(), expect_name.as_str());
            prop_assert_eq!(idb.arity(), gen.head_args.len());
            prop_assert_eq!(idb.force_serve(), gen.force_serve);
        }

        for (ri, (rule, gen)) in p.rules().iter().zip(&self.rules).enumerate() {
            let bound = gen.bound_vars(&self.edbs);
            let expect_name = format!("h{}_{}", ri, gen.head_name);
            prop_assert_eq!(rule.head().name().as_str(), expect_name.as_str());

            // Head arguments, including the leaf-collapse rule (a bare
            // variable expression parses as HeadArg::Var).
            prop_assert_eq!(rule.head().head_arguments().len(), gen.head_args.len());
            for (arg, gen_arg) in rule.head().head_arguments().iter().zip(&gen.head_args) {
                match gen_arg {
                    GenHeadArg::Var(i) => {
                        let expect = var_name(bound[i % bound.len()]);
                        prop_assert!(
                            matches!(arg, HeadArg::Var(v) if *v == expect),
                            "expected head var {}, got {:?}",
                            expect,
                            arg
                        );
                    }
                    GenHeadArg::Expr(GenExpr::Var(i)) => {
                        let expect = var_name(bound[i % bound.len()]);
                        prop_assert!(
                            matches!(arg, HeadArg::Var(v) if *v == expect),
                            "bare-var expr must collapse to a head var, got {:?}",
                            arg
                        );
                    }
                    GenHeadArg::Expr(GenExpr::Int(n)) => {
                        prop_assert!(
                            matches!(arg, HeadArg::Arith(a)
                                if a.rest().is_empty()
                                    && matches!(a.init(), parsing::arithmetic::Factor::Const(Const::Integer(v)) if v == n)),
                            "expected const-arith head arg {}, got {:?}",
                            n,
                            arg
                        );
                    }
                    GenHeadArg::Expr(GenExpr::Node(..)) => {
                        prop_assert!(
                            matches!(arg, HeadArg::Arith(a)
                                if a.rest().is_empty()
                                    && matches!(a.init(), parsing::arithmetic::Factor::Paren(_))),
                            "expected parenthesised head expression, got {:?}",
                            arg
                        );
                    }
                    GenHeadArg::Chain(chain) => {
                        let expect = chain.expected(&bound);
                        let HeadArg::Arith(a) = arg else {
                            prop_assert!(false, "expected an arithmetic head arg, got {:?}", arg);
                            unreachable!()
                        };
                        prop_assert_eq!(a, &expect, "head chain grouped against the oracle");
                        prop_assert_eq!(
                            a.ordered_vars()
                                .into_iter()
                                .cloned()
                                .collect::<Vec<String>>(),
                            chain.vars(&bound)
                        );
                    }
                    GenHeadArg::Agg(op, v) => {
                        let expect_op = AGG_OPS[op % AGG_OPS.len()];
                        let expect_var = var_name(bound[v % bound.len()]);
                        prop_assert!(
                            matches!(arg, HeadArg::Aggregation(agg)
                                if *agg.operator() == expect_op
                                    && agg.vars() == vec![&expect_var]),
                            "expected {:?}({}), got {:?}",
                            expect_op,
                            expect_var,
                            arg
                        );
                    }
                }
            }

            // Body: positive atoms (with full argument fidelity), then the
            // optional negation, then the comparisons — in order.
            let mut preds = rule.rhs().iter();
            for gen_atom in &gen.body {
                let decl = &self.edbs[gen_atom.edb % self.edbs.len()];
                let Some(Predicate::AtomPredicate(atom)) = preds.next() else {
                    prop_assert!(false, "expected positive atom");
                    unreachable!()
                };
                prop_assert_eq!(atom.name(), decl.name.as_str());
                prop_assert_eq!(atom.arity(), decl.arity);
                for (i, parsed) in atom.arguments().iter().enumerate() {
                    match gen_atom.args.get(i) {
                        Some(GenAtomArg::Var(v)) => {
                            let expect = var_name(*v);
                            prop_assert!(matches!(parsed, AtomArg::Var(x) if *x == expect));
                        }
                        Some(GenAtomArg::Int(n)) => {
                            prop_assert!(
                                matches!(parsed, AtomArg::Const(Const::Integer(v)) if v == n)
                            );
                        }
                        Some(GenAtomArg::Str(s)) => {
                            // String constants keep their quotes.
                            let expect = format!("\"{}\"", s);
                            prop_assert!(
                                matches!(parsed, AtomArg::Const(Const::Text(x)) if *x == expect)
                            );
                        }
                        Some(GenAtomArg::Placeholder) | None => {
                            prop_assert!(matches!(parsed, AtomArg::Placeholder));
                        }
                    }
                }
            }
            if let Some(neg) = &gen.negated {
                let decl = &self.edbs[neg.edb % self.edbs.len()];
                let Some(Predicate::NegatedAtomPredicate(atom)) = preds.next() else {
                    prop_assert!(false, "expected negated atom");
                    unreachable!()
                };
                prop_assert_eq!(atom.name(), decl.name.as_str());
                prop_assert_eq!(atom.arity(), decl.arity);
            }
            for gen_cmp in &gen.compares {
                let Some(Predicate::ComparePredicate(cmp)) = preds.next() else {
                    prop_assert!(false, "expected comparison");
                    unreachable!()
                };
                prop_assert_eq!(cmp.operator(), &CMP_OPS[gen_cmp.op % CMP_OPS.len()]);
                let expect_left = var_name(bound[gen_cmp.left % bound.len()]);
                prop_assert!(
                    cmp.left().is_var() && cmp.left().vars() == vec![&expect_left],
                    "compare left should be {}, got {}",
                    expect_left,
                    cmp.left()
                );
                match &gen_cmp.right {
                    // The single-operator contract, unchanged: a bare literal
                    // stays a leaf, and `V + n` stays a one-element chain with
                    // no synthesized parenthesis.
                    GenCmpRight::Simple { var: None, add } => prop_assert!(
                        cmp.right().rest().is_empty()
                            && matches!(cmp.right().init(),
                                parsing::arithmetic::Factor::Const(Const::Integer(v)) if v == add)
                    ),
                    GenCmpRight::Simple { var: Some(v), .. } => {
                        let expect = var_name(bound[v % bound.len()]);
                        prop_assert!(
                            matches!(cmp.right().init(), parsing::arithmetic::Factor::Var(x) if *x == expect)
                        );
                        prop_assert_eq!(cmp.right().rest().len(), 1);
                    }
                    // The multi-operator contract: grouped exactly as the
                    // precedence oracle says, with the variable walk intact.
                    GenCmpRight::Chain(chain) => {
                        prop_assert_eq!(
                            cmp.right(),
                            &chain.expected(&bound),
                            "compare chain grouped against the oracle"
                        );
                        prop_assert_eq!(
                            cmp.right()
                                .ordered_vars()
                                .into_iter()
                                .cloned()
                                .collect::<Vec<String>>(),
                            chain.vars(&bound)
                        );
                    }
                }
            }
            prop_assert!(preds.next().is_none(), "extra predicates parsed");

            // .plan / .sip / .optimize suffixes.
            let (plan, sip) = match PLANS[gen.plan_suffix % PLANS.len()] {
                "" => (false, false),
                " .plan" => (true, false),
                " .sip" => (false, true),
                _ => (true, true),
            };
            prop_assert_eq!(rule.is_planning(), plan);
            prop_assert_eq!(rule.is_sip(), sip);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    /// Generated valid programs parse, and the parsed AST matches the
    /// generator's model structurally.
    #[test]
    fn generated_programs_parse_to_their_model(program in program_strategy()) {
        let src = program.source();
        let parsed = syntax::parse(&src).unwrap_or_else(|d| {
            panic!(
                "parser rejected a generated program:\n{}\n{}",
                src,
                syntax::render("gen.dl", &src, &d, false)
            )
        });
        program.check(&parsed).map_err(|e| {
            TestCaseError::fail(format!("{}\nprogram was:\n{}", e, src))
        })?;
    }

    /// The parser and renderer never panic, whatever the input: arbitrary
    /// unicode junk...
    #[test]
    fn never_panics_on_arbitrary_input(src in "\\PC{0,200}") {
        if let Err(diagnostics) = syntax::parse(&src) {
            prop_assert!(!diagnostics.is_empty());
            let _ = syntax::render("junk.dl", &src, &diagnostics, false);
        }
    }

    /// ...and mutations of valid programs (truncations and splices — the
    /// half-typed states an editor sees).
    #[test]
    fn never_panics_on_mutated_programs(
        program in program_strategy(),
        cut in any::<proptest::sample::Index>(),
        // The splice alphabet includes the bitwise operators so a mutation can
        // land a half-typed `&`/`|`/`^` between the precedence levels.
        splice in "[(){}:,.\"!<>=+*/%&|^a-z0-9 ]{0,6}",
    ) {
        let src = program.source();
        let at = cut.index(src.len().max(1)).min(src.len());
        // Cut at a char boundary.
        let mut at = at;
        while !src.is_char_boundary(at) {
            at -= 1;
        }
        let mutated = format!("{}{}{}", &src[..at], splice, &src[at..]);
        if let Err(diagnostics) = syntax::parse(&mutated) {
            let _ = syntax::render("mut.dl", &mutated, &diagnostics, false);
        }
    }
}

// ---------------------------------------------------------------------------
// Precedence, on its own: one chain per case, four properties.
// ---------------------------------------------------------------------------

/// The four variables a standalone chain may reference.
const CHAIN_BOUND: &[usize] = &[0, 1, 2, 3];

/// Wrap an expression in the smallest program that binds `V0..V3` and puts the
/// expression somewhere the typing pass will not unify it against a declared
/// column — the right of a comparison — so the AST comes back exactly as the
/// parser built it.
fn chain_program(expr: &str) -> String {
    format!(
        ".in
.decl p(a: number, b: number, c: number, d: number)
.printsize
.decl out(v: number)
.rule
out(V0) :- p(V0, V1, V2, V3), V0 > {}.
",
        expr
    )
}

fn parse_chain(expr: &str) -> Result<Arithmetic, TestCaseError> {
    let src = chain_program(expr);
    let program = syntax::parse(&src).map_err(|diagnostics| {
        TestCaseError::fail(format!(
            "parser rejected `{}`:\n{}",
            expr,
            syntax::render("chain.dl", &src, &diagnostics, false)
        ))
    })?;
    let parsing::rule::Predicate::ComparePredicate(cmp) = &program.rules()[0].rhs()[1] else {
        return Err(TestCaseError::fail(format!(
            "`{}` did not parse as a comparison",
            expr
        )));
    };
    Ok(cmp.right().clone())
}

proptest! {
    /// The flagship precedence property. For a random flat chain over all
    /// eight operators:
    ///
    /// 1. **Oracle.** The parsed AST equals the tree the model's own
    ///    precedence-climbing routine builds from the level table.
    /// 2. **Round trip.** `Display` emits the synthesized parentheses, so
    ///    re-parsing the printed form must give back an identical AST.
    /// 3. **Explicit parens.** Writing the implied parentheses out by hand
    ///    parses to the same AST — the property that guarantees existing
    ///    hand-parenthesised programs are untouched by the change.
    /// 4. **Variable order.** `ordered_vars()` equals the left-to-right
    ///    variable sequence of the flat source, which is what the catalog and
    ///    planning lowerings consume positionally.
    #[test]
    fn chains_group_by_precedence(chain in chain_strategy(), compact in any::<bool>()) {
        let sep = if compact { "" } else { " " };
        let src = chain.render(CHAIN_BOUND, sep);
        let parsed = parse_chain(&src)?;

        let expected = chain.expected(CHAIN_BOUND);
        prop_assert_eq!(
            &parsed, &expected,
            "`{}` grouped as `{}`, oracle says `{}`", src, parsed, expected
        );

        let printed = parsed.to_string();
        let reparsed = parse_chain(&printed)?;
        prop_assert_eq!(
            &reparsed, &parsed,
            "`{}` printed as `{}` did not round trip", src, printed
        );

        let explicit = chain.render_explicit(CHAIN_BOUND, sep);
        let explicit_parsed = parse_chain(&explicit)?;
        prop_assert_eq!(
            &explicit_parsed, &parsed,
            "`{}` must parse identically to `{}`", src, explicit
        );

        prop_assert_eq!(
            parsed.ordered_vars().into_iter().cloned().collect::<Vec<String>>(),
            chain.vars(CHAIN_BOUND),
            "variable walk moved for `{}`", src
        );
    }

    /// A chain drawn from a single precedence level must stay FLAT: no
    /// synthesized parenthesis anywhere, one `rest` entry per operator, in
    /// source order.
    ///
    /// Flatness is how this AST spells left-associativity — the executor folds
    /// `rest` left to right — so this is also the assertion that rules out the
    /// parser right-associating: had it grouped `a - b - c` as `a - (b - c)`,
    /// `rest` would hold one `Factor::Paren` instead of two bare operands.
    #[test]
    fn single_level_chains_stay_flat(
        level in 0usize..NUM_LEVELS,
        ops in proptest::collection::vec(any::<proptest::sample::Index>(), 1..5),
        operands in proptest::collection::vec(
            prop_oneof![
                3 => (0usize..4).prop_map(GenOperand::Var),
                1 => (-1000i64..1000).prop_map(GenOperand::Int),
            ],
            5,
        ),
        compact in any::<bool>(),
    ) {
        let same_level: Vec<usize> = (0..CHAIN_OPS.len())
            .filter(|i| CHAIN_OPS[*i].1 == level)
            .collect();
        let ops: Vec<usize> = ops
            .iter()
            .map(|i| same_level[i.index(same_level.len())])
            .collect();
        let chain = GenChain {
            first: operands[0].clone(),
            rest: ops
                .iter()
                .zip(operands[1..].iter())
                .map(|(op, operand)| (*op, operand.clone()))
                .collect(),
        };

        let sep = if compact { "" } else { " " };
        let src = chain.render(CHAIN_BOUND, sep);
        let parsed = parse_chain(&src)?;

        prop_assert_eq!(
            parsed.rest().len(), chain.rest.len(),
            "`{}` should be one flat chain, got `{}`", src, parsed
        );
        prop_assert_eq!(parsed.init(), &chain.first.factor(CHAIN_BOUND));
        for ((op, factor), (gen_op, gen_operand)) in parsed.rest().iter().zip(&chain.rest) {
            prop_assert_eq!(op, &chain_op_ast(*gen_op));
            prop_assert_eq!(factor, &gen_operand.factor(CHAIN_BOUND));
            prop_assert!(
                !matches!(factor, Factor::Paren(_)),
                "`{}` grew a parenthesis: `{}`", src, parsed
            );
        }
        prop_assert!(
            !matches!(parsed.init(), Factor::Paren(_)),
            "`{}` grew a parenthesis: `{}`", src, parsed
        );
    }
}

/// The precedence generator must actually produce what the properties claim:
/// every one of the eight operators, chains that mix levels, and both operand
/// forms that re-enter the grammar at the loosest level. A generator that only
/// emitted single-level chains would keep `chains_group_by_precedence` green
/// while testing nothing.
#[test]
fn chain_generator_covers_the_operators() {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::TestRunner;

    let mut runner = TestRunner::deterministic();
    let strategy = chain_strategy();
    let mut seen_op = vec![false; CHAIN_OPS.len()];
    let mut mixed_levels = false;
    let mut long_chain = false;
    let mut kinds = [false; 3];
    for _ in 0..300 {
        let chain = strategy.new_tree(&mut runner).unwrap().current();
        mixed_levels |= chain.mixes_levels();
        long_chain |= chain.rest.len() >= 3;
        chain.operand_kinds(&mut kinds);
        let src = chain.render(CHAIN_BOUND, " ");
        for (i, (symbol, _)) in CHAIN_OPS.iter().enumerate() {
            seen_op[i] |= src.contains(&format!(" {} ", symbol));
        }
    }
    for (i, (symbol, _)) in CHAIN_OPS.iter().enumerate() {
        assert!(
            seen_op[i],
            "generator never produced the `{}` operator",
            symbol
        );
    }
    for (name, saw) in [
        ("a chain mixing precedence levels", mixed_levels),
        ("a chain of four or more operands", long_chain),
        ("a parenthesised operand", kinds[0]),
        ("an abs() operand", kinds[1]),
        ("a round(to_float()) operand", kinds[2]),
    ] {
        assert!(saw, "chain generator never produced: {}", name);
    }
}

/// Guard against a silently-degenerate generator: across a sample batch, every
/// construct the properties claim to exercise must actually occur.
#[test]
fn generator_covers_the_constructs() {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::TestRunner;

    let mut runner = TestRunner::deterministic();
    let strategy = program_strategy();
    let mut nested_parens = false;
    let mut aggregate = false;
    let mut negation = false;
    let mut string_const = false;
    let mut compact = false;
    let mut comments = false;
    let mut plan_suffix = false;
    let mut compare = false;
    for _ in 0..300 {
        let src = strategy.new_tree(&mut runner).unwrap().current().source();
        nested_parens |= src.contains("((");
        aggregate |= AGGS.iter().any(|a| src.contains(&format!("{}(", a)));
        negation |= src.contains('!');
        string_const |= src.contains('"');
        compact |= src.contains("):-");
        comments |= src.contains("# comment");
        plan_suffix |= src.contains(".plan") || src.contains(".sip") || src.contains(".optimize");
        compare |= CMPS.iter().any(|c| src.contains(&format!(" {} ", c)))
            || src.contains("<")
            || src.contains(">");
    }
    for (name, saw) in [
        ("nested parens", nested_parens),
        ("aggregate", aggregate),
        ("negation", negation),
        ("string const", string_const),
        ("compact layout", compact),
        ("comments", comments),
        ("plan suffix", plan_suffix),
        ("comparison", compare),
    ] {
        assert!(saw, "generator never produced: {}", name);
    }
}

/// Deep parenthesis nesting must not blow the stack (recursive-descent hazard);
/// Ok or Err are both fine, a crash is not.
#[test]
fn deep_nesting_does_not_overflow() {
    let depth = 300;
    let src = format!(
        ".in\n.decl e(x: number)\n.printsize\n.decl r(x: number)\n.rule\nr({}X{}) :- e(X).\n",
        "(".repeat(depth),
        ")".repeat(depth),
    );
    let _ = syntax::parse(&src);
}
