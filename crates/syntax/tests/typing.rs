//! Typing-pass behavior asserted through the parser: evaluation modes land on
//! the AST, and type errors reject the program. (Relocated from the parsing
//! crate's unit tests when its pest parser was retired; `errors.rs` covers the
//! rendering/span side of the rejections.)

use parsing::decl::DataType;
use parsing::head::HeadArg;
use parsing::rule::Predicate;

fn reject(src: &str) -> String {
    let diagnostics = syntax::parse(src).expect_err("program should be rejected");
    diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn float_literals_and_parens_parse_and_type() {
    let src = "\
.in
.decl sample(name: string, weight: float)
.printsize
.decl light(name: string)
.decl mid(name: string, m: float)
.rule
light(N) :- sample(N, U), U < 1.5.
mid(N, (A + B) / 2.0) :- sample(N, A), sample(N, B).
";
    let prog = syntax::parse(src).unwrap();

    // The comparison against a float literal runs in Float mode.
    let light = &prog.rules()[0];
    let Predicate::ComparePredicate(cmp) = &light.rhs()[1] else {
        panic!("expected a comparison");
    };
    assert_eq!(*cmp.left().data_type(), DataType::Float);
    assert_eq!(*cmp.right().data_type(), DataType::Float);

    // The parenthesised head expression keeps variable order and is typed
    // Float from the head decl.
    let mid = &prog.rules()[1];
    let HeadArg::Arith(arith) = &mid.head().head_arguments()[1] else {
        panic!("expected arithmetic head arg");
    };
    assert_eq!(*arith.data_type(), DataType::Float);
    assert_eq!(
        arith.vars(),
        vec![&"A".to_string(), &"B".to_string()],
        "paren sub-expression vars in strict order"
    );
    assert_eq!(arith.to_string(), "(A + B) / 2");
}

#[test]
fn conversion_builtins_bridge_the_modes() {
    let src = "\
.in
.decl cost(item: string, cents: number)
.printsize
.decl usd(item: string, dollars: float)
.decl whole(item: string, dollars: number)
.rule
usd(P, to_float(C) / 100.0) :- cost(P, C).
whole(P, round(to_float(C) / 100.0)) :- cost(P, C).
";
    let prog = syntax::parse(src).unwrap();
    let HeadArg::Arith(arith) = &prog.rules()[0].head().head_arguments()[1] else {
        panic!("expected arithmetic head arg");
    };
    assert_eq!(*arith.data_type(), DataType::Float);
    let HeadArg::Arith(arith) = &prog.rules()[1].head().head_arguments()[1] else {
        panic!("expected arithmetic head arg");
    };
    // round(...) produces a number even though its inside is float mode.
    assert_eq!(*arith.data_type(), DataType::Integer);
}

#[test]
fn integer_expressions_stay_integer_mode() {
    let src = "\
.in
.decl e(x: number, y: number)
.printsize
.decl big(x: number)
.rule
big(X) :- e(X, Y), X > Y + 100.
";
    let prog = syntax::parse(src).unwrap();
    let Predicate::ComparePredicate(cmp) = &prog.rules()[0].rhs()[1] else {
        panic!("expected a comparison");
    };
    assert_eq!(*cmp.left().data_type(), DataType::Integer);
}

#[test]
fn aggregation_over_float_column_types_float() {
    let src = "\
.in
.decl sample(name: string, weight: float)
.printsize
.decl total(name: string, sum_weight: float)
.decl n(name: string, c: number)
.rule
total(N, sum(U)) :- sample(N, U).
n(N, count(U)) :- sample(N, U).
";
    let prog = syntax::parse(src).unwrap();
    let HeadArg::Aggregation(sum) = &prog.rules()[0].head().head_arguments()[1] else {
        panic!("expected aggregation");
    };
    assert_eq!(*sum.data_type(), DataType::Float);
    // count is Integer no matter what it counts.
    let HeadArg::Aggregation(count) = &prog.rules()[1].head().head_arguments()[1] else {
        panic!("expected aggregation");
    };
    assert_eq!(*count.data_type(), DataType::Integer);
}

#[test]
fn to_float_of_a_float_is_rejected() {
    let messages = reject(
        "\
.in
.decl sample(name: string, weight: float)
.printsize
.decl f(name: string, weight: float)
.rule
f(N, to_float(U)) :- sample(N, U).
",
    );
    assert!(
        messages.contains("to_float takes one number argument"),
        "got: {messages}"
    );
}

#[test]
fn negated_only_variable_is_rejected() {
    let messages = reject(
        "\
.in
.decl e(x: number)
.decl f(x: number, y: number)
.printsize
.decl r(x: number)
.rule
r(X) :- e(X), !f(X, Z).
",
    );
    assert!(
        messages.contains("variable Z is used in `!f(X, Z)` but not bound"),
        "got: {messages}"
    );
}

#[test]
fn string_number_join_is_rejected() {
    let messages = reject(
        "\
.in
.decl names(x: string)
.decl ids(x: number)
.printsize
.decl r(x: string)
.rule
r(X) :- names(X), ids(X).
",
    );
    assert!(
        messages.contains("variable X is bound to a string column and a number column"),
        "got: {messages}"
    );
}

#[test]
fn out_section_marks_force_serve() {
    let src = "\
.in
.decl e(x: number)
.printsize
.decl a(x: number)
.out
.decl b(x: number)
.printsize
.decl c(x: number)
.rule
a(X) :- e(X).
b(X) :- a(X).
c(X) :- b(X).
";
    let prog = syntax::parse(src).unwrap();
    let serve = |name: &str| {
        prog.idbs()
            .iter()
            .find(|d| d.name() == name)
            .unwrap()
            .force_serve()
    };
    assert!(!serve("a"), "`.printsize` relation must not force-serve");
    assert!(serve("b"), "`.out` relation must force-serve");
    assert!(!serve("c"));
}
