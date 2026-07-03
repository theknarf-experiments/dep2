//! Property-based tests for the chumsky front-end.
//!
//! The flagship property generates random *valid* programs as source text —
//! random names (including keyword-containing ones like `counter`), arities,
//! nested parenthesised arithmetic, aggregates, negation, comparisons, string
//! constants, comments, and compact-vs-spaced layout — and asserts the chumsky
//! parser and the pest reference parser produce identical ASTs. The corpus
//! equivalence test pins the parsers together on real programs; this pins them
//! together on the weird corners no human writes.
//!
//! A second property feeds arbitrary and mutated input and asserts the parser
//! (and the ariadne renderer) never panic — errors only.

use proptest::prelude::*;

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
    right_var: Option<usize>,
    right_add: i64,
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
                let right = match cmp.right_var {
                    Some(v) => format!(
                        "{}{sep}+{sep}{}",
                        var_name(bound[v % bound.len()]),
                        cmp.right_add
                    ),
                    None => cmp.right_add.to_string(),
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
    ];
    let compare = (
        0usize..4,
        0usize..CMPS.len(),
        proptest::option::of(0usize..4),
        -100i64..100,
    )
        .prop_map(|(left, op, right_var, right_add)| GenCompare {
            left,
            op,
            right_var,
            right_add,
        });
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
// Properties
// ---------------------------------------------------------------------------

proptest! {
    /// Generated valid programs parse identically through chumsky and pest.
    #[test]
    fn generated_programs_match_pest(program in program_strategy()) {
        let src = program.source();

        let chumsky = syntax::parse(&src).unwrap_or_else(|d| {
            panic!(
                "chumsky rejected a generated program:\n{}\n{}",
                src,
                syntax::render("gen.dl", &src, &d, false)
            )
        });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gen.dl");
        std::fs::write(&path, &src).unwrap();
        let pest = parsing::parser::Program::parse_from(&path.to_string_lossy());

        prop_assert_eq!(
            pest.to_string(),
            chumsky.to_string(),
            "AST mismatch on generated program:\n{}",
            src
        );
        let serve = |p: &parsing::parser::Program| -> Vec<(String, bool)> {
            p.idbs().iter().map(|d| (d.name().to_string(), d.force_serve())).collect()
        };
        prop_assert_eq!(serve(&pest), serve(&chumsky), "force-serve mismatch:\n{}", src);
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
        splice in "[(){}:,.\"!<>=+*/%a-z0-9 ]{0,6}",
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
