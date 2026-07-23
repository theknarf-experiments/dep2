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
fn float_math_builtins_type_as_float() {
    use parsing::arithmetic::{BuiltinOp, Factor};

    let src = "\
.in
.decl px(name: string, p: float)
.printsize
.decl logit(name: string, l: float)
.decl gap(name: string, g: float)
.decl igap(name: string, g: number)
.rule
logit(N, ln(P / (1.0 - P))) :- px(N, P).
gap(N, abs(P - 0.5)) :- px(N, P).
igap(N, abs(round(P) - 1)) :- px(N, P).
";
    let prog = syntax::parse(src).unwrap();
    let HeadArg::Arith(arith) = &prog.rules()[0].head().head_arguments()[1] else {
        panic!("expected arithmetic head arg");
    };
    assert_eq!(*arith.data_type(), DataType::Float);

    // abs specializes by argument kind: float here, number below.
    let abs_op = |rule: usize| {
        let HeadArg::Arith(arith) = &prog.rules()[rule].head().head_arguments()[1] else {
            panic!("expected arithmetic head arg");
        };
        match arith.init() {
            Factor::Builtin(op, _) => *op,
            other => panic!("expected builtin, got {:?}", other),
        }
    };
    assert_eq!(abs_op(1), BuiltinOp::AbsFloat);
    assert_eq!(
        *prog.rules()[1].head().head_arguments()[1].vars()[0],
        "P".to_string()
    );
    assert_eq!(abs_op(2), BuiltinOp::AbsInt);
}

#[test]
fn similarity_types_str_str_to_number() {
    let prog = syntax::parse(
        "\
.in
.decl a(x: string)
.decl b(y: string)
.printsize
.decl pair(x: string, y: string)
.rule
pair(X, Y) :- a(X), b(Y), similarity(to_lower(X), to_lower(Y)) > 85.
",
    )
    .unwrap();
    assert_eq!(prog.rules().len(), 1);

    let messages = {
        let d = syntax::parse(
            "\
.in
.decl e(x: number)
.printsize
.decl r(x: number)
.rule
r(X) :- e(X), similarity(X, X) > 50.
",
        )
        .expect_err("similarity over numbers must be rejected");
        d.iter()
            .map(|x| x.message.clone())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        messages.contains("similarity takes 2 arguments (string, string)"),
        "got: {messages}"
    );
}

#[test]
fn ln_of_a_number_is_rejected() {
    let messages = reject(
        "\
.in
.decl e(x: number)
.printsize
.decl r(x: float)
.rule
r(ln(X)) :- e(X).
",
    );
    assert!(
        messages.contains("ln takes one float argument"),
        "got: {messages}"
    );
}

#[test]
fn pow_arity_is_checked() {
    let messages = reject(
        "\
.in
.decl px(name: string, p: float)
.printsize
.decl r(name: string, v: float)
.rule
r(N, pow(P)) :- px(N, P).
",
    );
    assert!(
        messages.contains("pow takes 2 arguments (float, float)"),
        "got: {messages}"
    );
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

#[test]
fn undeclared_body_atom_is_rejected() {
    // A builtin used bare (instead of as a comparison) parses as an atom named
    // after the builtin; it must be a load-time error, not an assembly panic.
    let err = syntax::parse(
        ".in\n.decl item(name: string)\n.printsize\n.decl hit(name: string)\n.rule\nhit(N) :- item(N), starts_with(N, \"fo\").\n",
    )
    .expect_err("bare builtin atom must be rejected");
    let text = format!("{:?}", err);
    assert!(text.contains("unknown relation starts_with"), "got: {text}");
    assert!(text.contains("comparison"), "got: {text}");

    // A plain misspelled relation gets the same treatment.
    let err = syntax::parse(
        ".in\n.decl item(name: string)\n.printsize\n.decl hit(name: string)\n.rule\nhit(N) :- itme(N).\n",
    )
    .expect_err("misspelled relation must be rejected");
    assert!(format!("{:?}", err).contains("unknown relation itme"));
}

#[test]
fn undeclared_head_defines_an_intermediate() {
    // Undeclared HEADS are allowed: they define intermediate relations
    // (crdt_slow.dl in the executing corpus relies on this), and body atoms
    // may reference them.
    syntax::parse(
        ".in\n.decl item(name: string)\n.printsize\n.decl out(name: string)\n.rule\nmid(N) :- item(N).\nout(N) :- mid(N).\n",
    )
    .expect("undeclared intermediate head must be accepted");
}

#[test]
fn negation_through_recursion_is_rejected() {
    // a depends on !b while b depends on a: no stratification exists, and the
    // engine used to silently produce an internally inconsistent result.
    let err = syntax::parse(
        ".in\n.decl c(x: number)\n.printsize\n.decl a(x: number)\n.decl b(x: number)\n.rule\na(X) :- c(X), !b(X).\nb(X) :- a(X).\n",
    )
    .expect_err("negation inside a recursive cycle must be rejected");
    let text = format!("{:?}", err);
    assert!(text.contains("stratif"), "got: {text}");
    assert!(text.contains('b') && text.contains('a'), "got: {text}");

    // Direct self-negation is the smallest instance.
    let err = syntax::parse(
        ".in\n.decl c(x: number)\n.printsize\n.decl a(x: number)\n.rule\na(X) :- c(X), !a(X).\n",
    )
    .expect_err("self-negation must be rejected");
    assert!(format!("{:?}", err).contains("stratif"));

    // A longer cycle through an intermediate is still a cycle.
    let err = syntax::parse(
        ".in\n.decl c(x: number)\n.printsize\n.decl a(x: number)\n.decl m(x: number)\n.decl b(x: number)\n.rule\na(X) :- c(X), !b(X).\nm(X) :- a(X).\nb(X) :- m(X).\n",
    )
    .expect_err("negation cycle through an intermediate must be rejected");
    assert!(format!("{:?}", err).contains("stratif"));
}

#[test]
fn negation_of_a_lower_stratum_stays_legal() {
    // Classic stratified negation: unreach negates a fully-computed reach.
    syntax::parse(
        ".in\n.decl edge(x: number, y: number)\n.decl node(x: number)\n.printsize\n.decl reach(x: number)\n.decl unreach(x: number)\n.rule\nreach(1) :- node(1).\nreach(Y) :- reach(X), edge(X, Y).\nunreach(X) :- node(X), !reach(X).\n",
    )
    .expect("negation of a lower stratum must stay legal");

    // Mutual recursion WITHOUT negation is untouched.
    syntax::parse(
        ".in\n.decl e(x: number, y: number)\n.printsize\n.decl odd(x: number)\n.decl even(x: number)\n.rule\neven(1) :- e(1, _).\nodd(Y) :- even(X), e(X, Y).\neven(Y) :- odd(X), e(X, Y).\n",
    )
    .expect("negation-free mutual recursion must stay legal");
}

#[test]
fn avg_aggregation_parses_and_types() {
    // Integer column: avg keeps integer mode (truncated mean).
    syntax::parse(
        ".in\n.decl m(s: number, v: number)\n.printsize\n.decl a(s: number, m: number)\n.rule\na(S, avg(V)) :- m(S, V).\n",
    )
    .expect("integer avg must parse and type");

    // Float column: avg is float, like min/max over floats.
    syntax::parse(
        ".in\n.decl m(s: number, v: float)\n.printsize\n.decl a(s: number, m: float)\n.rule\na(S, avg(V)) :- m(S, V).\n",
    )
    .expect("float avg must parse and type");
}

#[test]
fn order_by_and_limit_annotations_parse() {
    let program = syntax::parse(
        ".in\n.decl m(s: number, v: number)\n.out\n.decl top(s: number, v: number) order_by(v desc, s) limit(3)\n.rule\ntop(S, V) :- m(S, V).\n",
    )
    .expect("order_by/limit annotations must parse");
    let top = program.idbs().iter().find(|d| d.name() == "top").unwrap();
    assert_eq!(top.order_by(), &[(1, true), (0, false)]);
    assert_eq!(top.limit(), Some(3));

    // Annotations are optional and order-independent.
    let program = syntax::parse(
        ".in\n.decl m(s: number)\n.printsize\n.decl a(s: number) limit(5)\n.rule\na(S) :- m(S).\n",
    )
    .unwrap();
    assert_eq!(
        program
            .idbs()
            .iter()
            .find(|d| d.name() == "a")
            .unwrap()
            .limit(),
        Some(5)
    );

    // Unknown order_by column is a parse-time error.
    syntax::parse(
        ".in\n.decl m(s: number)\n.printsize\n.decl a(s: number) order_by(nope)\n.rule\na(S) :- m(S).\n",
    )
    .expect_err("unknown order_by column must be rejected");
}
