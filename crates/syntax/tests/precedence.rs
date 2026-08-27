//! Arithmetic operator precedence, and the shape invariants it must not break.
//!
//! The dialect binds `* / %` tightest, then `+ -`, then `&`, then `^`, then `|`
//! (loosest); every level is left-associative. Precedence is expressed in the
//! flat `Arithmetic` AST by emitting a tighter-binding sub-chain as
//! `Factor::Paren`, so the correctness criterion is literally an AST equality:
//! `A - B * 2` must parse to exactly what `A - (B * 2)` parses to.
//!
//! Two properties matter as much as the precedence itself and are pinned here:
//!
//! 1. **No spurious wrapping.** A level that matched none of its own operators
//!    must pass the tighter level through untouched. Nine downstream sites read
//!    `rest().is_empty()` / `is_var()` as "this is a bare leaf"; one of them
//!    (`strata::rewrite`'s `bare = bare` guard) would silently turn a compare
//!    into a NULL-matching hash join if a bare variable came back as `(A)`.
//! 2. **Variable order.** The catalog and planning lowerings consume
//!    `ordered_vars()` positionally through a single shared cursor that recurses
//!    into `Paren`. Regrouping is only sound because it moves no variable in the
//!    left-to-right sequence.

use parsing::arithmetic::{Arithmetic, ArithmeticOperator, Factor};
use parsing::decl::DataType;
use parsing::head::HeadArg;
use parsing::rule::{Const, Predicate};

/// Parse `expr` in integer mode and hand back the parsed chain.
///
/// The expression is placed on the right of a comparison because that is the
/// one position where the typing pass does not unify it against a declared
/// column type, so the AST comes back exactly as the parser built it.
fn arith(expr: &str) -> Arithmetic {
    let src = format!(
        ".in
.decl p(a: number, b: number, c: number, d: number, e: number, x: number)
.printsize
.decl out(a: number)
.rule
out(A) :- p(A, B, C, D, E, X), A > {}.
",
        expr
    );
    let prog = syntax::parse(&src)
        .unwrap_or_else(|d| panic!("`{}` should parse: {:?}", expr, messages(&d)));
    let Predicate::ComparePredicate(cmp) = &prog.rules()[0].rhs()[1] else {
        panic!("expected a comparison for `{}`", expr);
    };
    cmp.right().clone()
}

/// As [`arith`], but in float mode.
fn arith_float(expr: &str) -> Arithmetic {
    let src = format!(
        ".in
.decl p(a: float, b: float, c: float, d: float, e: float, x: float)
.printsize
.decl out(a: float)
.rule
out(A) :- p(A, B, C, D, E, X), A > {}.
",
        expr
    );
    let prog = syntax::parse(&src)
        .unwrap_or_else(|d| panic!("`{}` should parse: {:?}", expr, messages(&d)));
    let Predicate::ComparePredicate(cmp) = &prog.rules()[0].rhs()[1] else {
        panic!("expected a comparison for `{}`", expr);
    };
    cmp.right().clone()
}

fn messages(diagnostics: &[syntax::Diagnostic]) -> Vec<String> {
    diagnostics.iter().map(|d| d.message.clone()).collect()
}

fn rejected(expr: &str) -> Vec<String> {
    let src = format!(
        ".in
.decl p(a: number, b: number, c: number, d: number, e: number, x: number)
.printsize
.decl out(a: number)
.rule
out(A) :- p(A, B, C, D, E, X), A > {}.
",
        expr
    );
    messages(&syntax::parse(&src).expect_err(&format!("`{}` should be rejected", expr)))
}

fn int(v: i64) -> Factor {
    Factor::Const(Const::Integer(v))
}

fn var(name: &str) -> Factor {
    Factor::Var(name.to_string())
}

/// `left` and `right` must parse to byte-identical ASTs. This is the whole
/// correctness criterion of the parser-only precedence strategy: adding real
/// precedence means an unparenthesised chain now produces exactly the AST the
/// explicitly-parenthesised chain has always produced.
fn same_ast(left: &str, right: &str) {
    assert_eq!(
        arith(left),
        arith(right),
        "`{}` must parse identically to `{}`",
        left,
        right
    );
}

// ---------------------------------------------------------------------------
// Unary sign: unchanged by the level restructure, because it lives in
// `constant()` at the factor level and every level matches its operator before
// its operand.
// ---------------------------------------------------------------------------

#[test]
fn a_minus_sign_after_an_operand_is_subtraction_not_a_signed_literal() {
    let a = arith("X-5");
    assert_eq!(a.rest().len(), 1, "`X-5` is a chain, not a single literal");
    assert_eq!(*a.init(), var("X"));
    assert_eq!(a.rest()[0], (ArithmeticOperator::Minus, int(5)));
    assert_ne!(
        *a.init(),
        int(-5),
        "the sign must not be absorbed into a literal"
    );
}

#[test]
fn a_second_adjacent_sign_reaches_the_literal() {
    let a = arith("A - -5");
    assert_eq!(*a.init(), var("A"));
    assert_eq!(a.rest(), [(ArithmeticOperator::Minus, int(-5))]);

    let a = arith("A * -5");
    assert_eq!(*a.init(), var("A"));
    assert_eq!(a.rest(), [(ArithmeticOperator::Multiply, int(-5))]);

    let a = arith("A + +5");
    assert_eq!(*a.init(), var("A"));
    assert_eq!(a.rest(), [(ArithmeticOperator::Plus, int(5))]);
}

#[test]
fn a_signed_literal_starting_a_chain_is_the_init() {
    let a = arith("-5 + A");
    assert_eq!(*a.init(), int(-5));
    assert_eq!(a.rest(), [(ArithmeticOperator::Plus, var("A"))]);

    let a = arith_float("-1.5 * X");
    assert_eq!(
        *a.init(),
        Factor::Const(Const::Float((-1.5f64).to_bits() as i64))
    );
    assert_eq!(a.rest(), [(ArithmeticOperator::Multiply, var("X"))]);
    assert_eq!(*a.data_type(), DataType::Float);
}

#[test]
fn there_is_still_no_unary_minus_on_variables_or_sub_expressions() {
    // The level stack introduces no new place for one; pin it so nobody
    // "fixes" it into existence while adding levels.
    assert!(!rejected("-A").is_empty());
    assert!(!rejected("-(A + B)").is_empty());
    assert!(!rejected("A + * B").is_empty());
}

// ---------------------------------------------------------------------------
// No spurious wrapping: an unmixed chain must be bit-identical to what the old
// single left-to-right fold produced.
// ---------------------------------------------------------------------------

#[test]
fn a_bare_variable_stays_a_bare_variable() {
    let a = arith("A");
    assert_eq!(*a.init(), var("A"));
    assert!(a.rest().is_empty());
    assert!(
        a.is_var(),
        "head()'s arith_arg and strata's bare-var guards depend on this"
    );
}

#[test]
fn a_bare_variable_head_argument_is_still_head_arg_var() {
    // The loud tripwire for the no-wrap invariant: if any level wrapped a
    // single operand, every plain head variable would become HeadArg::Arith
    // and reroute the head through the arithmetic projection path.
    let prog = syntax::parse(
        ".in
.decl p(a: number, b: number)
.printsize
.decl out(a: number, b: number)
.rule
out(A, B) :- p(A, B).
",
    )
    .unwrap();
    let args = prog.rules()[0].head().head_arguments();
    assert!(matches!(&args[0], HeadArg::Var(v) if v == "A"));
    assert!(matches!(&args[1], HeadArg::Var(v) if v == "B"));
}

#[test]
fn a_bare_var_equals_bare_var_compare_keeps_both_sides_bare() {
    // strata::rewrite deliberately leaves `bare = bare` alone: materializing it
    // into a hash join would match on NULL where the compare rejects it. That
    // decision is made with `is_var()`, so a spurious Paren here would be a
    // silent wrong-answers bug rather than a test failure.
    let prog = syntax::parse(
        ".in
.decl e(a: number)
.decl f(a: number)
.printsize
.decl x(a: number)
.rule
x(A) :- e(A), f(B), A = B.
",
    )
    .unwrap();
    let Predicate::ComparePredicate(cmp) = &prog.rules()[0].rhs()[2] else {
        panic!("expected a comparison");
    };
    assert!(cmp.left().is_var(), "left side must stay a bare var");
    assert!(cmp.right().is_var(), "right side must stay a bare var");
}

#[test]
fn a_single_level_chain_stays_flat() {
    let a = arith("A * B");
    assert_eq!(*a.init(), var("A"));
    assert_eq!(a.rest(), [(ArithmeticOperator::Multiply, var("B"))]);
}

#[test]
fn same_level_chains_are_left_associative_and_flat() {
    let a = arith("A / B / C");
    assert_eq!(*a.init(), var("A"));
    assert_eq!(
        a.rest(),
        [
            (ArithmeticOperator::Divide, var("B")),
            (ArithmeticOperator::Divide, var("C")),
        ],
        "`A / B / C` is ((A/B)/C) — a flat chain, no Paren inserted"
    );

    let a = arith("A - B - C");
    assert_eq!(*a.init(), var("A"));
    assert_eq!(
        a.rest(),
        [
            (ArithmeticOperator::Minus, var("B")),
            (ArithmeticOperator::Minus, var("C")),
        ],
        "`A - B - C` is ((A-B)-C) — a flat chain, no Paren inserted"
    );

    for expr in [
        "A | B | C",
        "A ^ B ^ C",
        "A & B & C",
        "A + B - C",
        "A * B / C % D",
    ] {
        let a = arith(expr);
        assert!(
            a.rest().iter().all(|(_, f)| !matches!(f, Factor::Paren(_))),
            "`{}` is one precedence level and must stay flat",
            expr
        );
        assert!(!matches!(a.init(), Factor::Paren(_)));
    }
}

#[test]
fn a_user_written_paren_renders_and_shapes_exactly_as_before() {
    let a = arith("(A + B) / 2");
    let Factor::Paren(inner) = a.init() else {
        panic!("expected the user's paren as init, got {:?}", a.init());
    };
    assert_eq!(*inner.init(), var("A"));
    assert_eq!(inner.rest(), [(ArithmeticOperator::Plus, var("B"))]);
    assert_eq!(a.rest(), [(ArithmeticOperator::Divide, int(2))]);
    // The one exact Display assertion in the tree (tests/typing.rs:53) lives on
    // this shape; keep its canary here too.
    assert_eq!(a.to_string(), "(A + B) / 2");
}

// ---------------------------------------------------------------------------
// Precedence proper.
// ---------------------------------------------------------------------------

#[test]
fn multiplicative_binds_tighter_than_additive() {
    let a = arith("A - B * 2");
    assert_eq!(*a.init(), var("A"));
    assert_eq!(a.rest().len(), 1);
    let (op, rhs) = &a.rest()[0];
    assert_eq!(*op, ArithmeticOperator::Minus);
    let Factor::Paren(inner) = rhs else {
        panic!("expected `B * 2` held as a sub-expression, got {:?}", rhs);
    };
    assert_eq!(*inner.init(), var("B"));
    assert_eq!(inner.rest(), [(ArithmeticOperator::Multiply, int(2))]);

    same_ast("A - B * 2", "A - (B * 2)");
    same_ast("A + B * C - D", "A + (B * C) - D");
    same_ast("A * B + C * D", "(A * B) + (C * D)");
    same_ast("A + B / C", "A + (B / C)");
}

#[test]
fn modulo_sits_at_the_multiplicative_level() {
    same_ast("A % B + C", "(A % B) + C");
    same_ast("A + B % C", "A + (B % C)");
}

#[test]
fn bitwise_levels_are_and_then_xor_then_or() {
    same_ast("A | B & C ^ D", "A | ((B & C) ^ D)");
    same_ast("A & B | C ^ D", "(A & B) | (C ^ D)");
    same_ast("A ^ B & C", "A ^ (B & C)");
    same_ast("A | B ^ C", "A | (B ^ C)");
    assert_eq!(arith("A | B & C ^ D").to_string(), "A | ((B & C) ^ D)");
}

#[test]
fn arithmetic_binds_tighter_than_every_bitwise_level() {
    same_ast("A & B + C", "A & (B + C)");
    same_ast("A ^ B * C", "A ^ (B * C)");
    same_ast("A | B - C", "A | (B - C)");
    same_ast("A | B & C + D * E", "A | (B & (C + (D * E)))");
}

#[test]
fn a_comparison_operator_separates_two_complete_chains() {
    // C's `a & b == c` wart cannot arise: `predicate()` is
    // `arithmetic() comparison_op() arithmetic()`, so a comparison can never
    // appear inside a chain regardless of where `&` sits.
    let prog = syntax::parse(
        ".in
.decl p(a: number, b: number, c: number, d: number)
.printsize
.decl out(a: number)
.rule
out(A) :- p(A, B, C, D), A & B > C.
",
    )
    .unwrap();
    let Predicate::ComparePredicate(cmp) = &prog.rules()[0].rhs()[1] else {
        panic!("expected a comparison");
    };
    assert_eq!(cmp.left().to_string(), "A & B");
    assert_eq!(cmp.right().to_string(), "C");
}

// ---------------------------------------------------------------------------
// The invariant the whole parser-only strategy rests on.
// ---------------------------------------------------------------------------

#[test]
fn regrouping_preserves_the_left_to_right_variable_order() {
    // catalog::ArithmeticPos::from_arithmetic_shared and
    // planning::ArithmeticArgument::from_arithmetic_shared thread ONE var_id
    // cursor through nested Parens and consume the signature list positionally.
    // Regrouping is sound only because it moves no variable in this sequence.
    let a = arith("A - B * 2 + C * D");
    let names: Vec<&str> = a.ordered_vars().iter().map(|s| s.as_str()).collect();
    assert_eq!(names, ["A", "B", "C", "D"]);

    for (mixed, parenthesised) in [
        ("A - B * 2 + C * D", "A - (B * 2) + (C * D)"),
        ("A | B & C ^ D", "A | ((B & C) ^ D)"),
        ("A + B * C - D % A", "A + (B * C) - (D % A)"),
    ] {
        assert_eq!(
            arith(mixed).ordered_vars(),
            arith(parenthesised).ordered_vars(),
            "`{}` must walk its variables in the same order as `{}`",
            mixed,
            parenthesised
        );
    }
}

// ---------------------------------------------------------------------------
// Nesting: parens and builtin arguments re-enter at the loosest level.
// ---------------------------------------------------------------------------

#[test]
fn a_builtin_argument_is_a_full_expression_and_a_bare_one_is_not_wrapped() {
    let a = arith("abs(A)");
    let Factor::Builtin(_, args) = a.init() else {
        panic!("expected a builtin, got {:?}", a.init());
    };
    assert_eq!(
        args,
        &[var("A")],
        "a bare builtin arg must not become `(A)`"
    );

    // A mixed chain inside a builtin argument gets precedence too.
    same_ast("abs(A - B * 2)", "abs(A - (B * 2))");
    // Two arguments: precedence must not leak across the comma either.
    assert_eq!(
        arith_float("pow(A + B * 2.0, C)"),
        arith_float("pow(A + (B * 2.0), C)")
    );
}

#[test]
fn user_parens_still_override_precedence() {
    let a = arith("(A + B) * C");
    assert!(matches!(a.init(), Factor::Paren(_)));
    assert_eq!(a.rest(), [(ArithmeticOperator::Multiply, var("C"))]);
    assert_ne!(arith("(A + B) * C"), arith("A + B * C"));
}

#[test]
fn nested_float_chains_still_resolve_to_float_mode() {
    let src = ".in
.decl cost(item: string, cents: number)
.printsize
.decl whole(item: string, dollars: number)
.rule
whole(P, round(to_float(C) / 100.0 + 1.0)) :- cost(P, C).
";
    let prog = syntax::parse(src).unwrap();
    let HeadArg::Arith(arith) = &prog.rules()[0].head().head_arguments()[1] else {
        panic!("expected an arithmetic head arg");
    };
    let Factor::Builtin(_, args) = arith.init() else {
        panic!("expected `round(..)`, got {:?}", arith.init());
    };
    let Factor::Paren(sum) = &args[0] else {
        panic!("expected the `+` chain as round's arg, got {:?}", args[0]);
    };
    assert_eq!(*sum.data_type(), DataType::Float);
    let Factor::Paren(quotient) = sum.init() else {
        panic!(
            "expected `/` regrouped tighter than `+`, got {:?}",
            sum.init()
        );
    };
    assert_eq!(*quotient.data_type(), DataType::Float);
    assert_eq!(
        *arith.data_type(),
        DataType::Integer,
        "round(..) is a number"
    );

    // And it is byte-identical to writing the grouping out.
    let explicit = src.replace(
        "round(to_float(C) / 100.0 + 1.0)",
        "round((to_float(C) / 100.0) + 1.0)",
    );
    let explicit = syntax::parse(&explicit).unwrap();
    let HeadArg::Arith(explicit) = &explicit.rules()[0].head().head_arguments()[1] else {
        panic!("expected an arithmetic head arg");
    };
    assert_eq!(arith, explicit);
}

#[test]
fn an_aggregate_over_a_mixed_chain_parses_and_types() {
    // The parser can now synthesize a Paren inside an aggregate argument, a
    // position that previously only ever held user-written parens (it flows
    // through strata's subst_vars rewrite).
    let prog = syntax::parse(
        ".in
.decl q(k: number, a: number, b: number)
.printsize
.decl p(k: number, total: number)
.rule
p(K, sum(A * 2 + B)) :- q(K, A, B).
",
    )
    .unwrap();
    let HeadArg::Aggregation(agg) = &prog.rules()[0].head().head_arguments()[1] else {
        panic!("expected an aggregation");
    };
    let a = agg.arithmetic();
    assert!(!a.is_var(), "a mixed chain is not a bare var");
    assert!(matches!(a.init(), Factor::Paren(_)), "`A * 2` regrouped");
    assert_eq!(a.rest(), [(ArithmeticOperator::Plus, var("B"))]);
    assert_eq!(a.ordered_vars(), vec![&"A".to_string(), &"B".to_string()]);
}

#[test]
fn the_corpus_lexicographic_compare_keeps_its_meaning() {
    // crates/executing/tests/flowlog_programs/crdt.dl:49 — a (counter, node)
    // pair packed into one integer. Left-to-right already grouped it the C way;
    // precedence must not move it.
    let prog = syntax::parse(
        ".in
.decl p(a: number, b: number, c: number, d: number)
.printsize
.decl out(a: number)
.rule
out(Ctr1) :- p(Ctr1, N1, Ctr2, N2), Ctr1 * 10 + N1 > Ctr2 * 10 + N2.
",
    )
    .unwrap();
    let Predicate::ComparePredicate(cmp) = &prog.rules()[0].rhs()[1] else {
        panic!("expected a comparison");
    };
    assert_eq!(cmp.left().to_string(), "(Ctr1 * 10) + N1");
    assert_eq!(cmp.right().to_string(), "(Ctr2 * 10) + N2");
    assert_eq!(
        cmp.left().ordered_vars(),
        vec![&"Ctr1".to_string(), &"N1".to_string()],
        "the positional lowering must still see Ctr1 then N1"
    );
}

#[test]
fn deeply_nested_parens_still_parse() {
    // Every factor now descends five levels, and a `(` re-enters at the
    // loosest, so recursion depth per paren grows. Keep an adversarial input
    // in the suite.
    let expr = format!("{}A + B{}", "(".repeat(50), ")".repeat(50));
    let a = arith(&expr);
    assert!(matches!(a.init(), Factor::Paren(_)));
    assert!(a.rest().is_empty());
}
