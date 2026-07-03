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
                match gen_cmp.right_var {
                    None => prop_assert!(
                        cmp.right().rest().is_empty()
                            && matches!(cmp.right().init(),
                                parsing::arithmetic::Factor::Const(Const::Integer(v)) if *v == gen_cmp.right_add)
                    ),
                    Some(v) => {
                        let expect = var_name(bound[v % bound.len()]);
                        prop_assert!(
                            matches!(cmp.right().init(), parsing::arithmetic::Factor::Var(x) if *x == expect)
                        );
                        prop_assert_eq!(cmp.right().rest().len(), 1);
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
