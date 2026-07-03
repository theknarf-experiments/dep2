//! Error-quality tests: the whole point of this crate. Each case asserts the
//! rendered report contains the message and points at the right line.

fn report(src: &str) -> String {
    let diagnostics = syntax::parse(src).expect_err("program should be rejected");
    syntax::render("test.dl", src, &diagnostics, false)
}

#[test]
fn valid_program_parses() {
    let program = syntax::parse(
        "\
.in
.decl e(x: number, y: number)
.printsize
.decl tc(x: number, y: number)
.rule
tc(X, Y) :- e(X, Y).
tc(X, Y) :- tc(X, Z), e(Z, Y).
",
    )
    .unwrap();
    assert_eq!(program.edbs().len(), 1);
    assert_eq!(program.rules().len(), 2);
}

#[test]
fn missing_terminating_dot() {
    let src = "\
.in
.decl e(x: number)
.printsize
.decl r(x: number)
.rule
r(X) :- e(X)
";
    let out = report(src);
    assert!(out.contains("syntax error"), "got:\n{out}");
    assert!(out.contains("test.dl"), "got:\n{out}");
}

#[test]
fn unknown_column_type_names_the_options() {
    let src = "\
.in
.decl e(x: int)
";
    let out = report(src);
    assert!(out.contains("unknown column type `int`"), "got:\n{out}");
    assert!(out.contains("number, string and float"), "got:\n{out}");
    assert!(out.contains("2 │"), "should point at line 2, got:\n{out}");
}

#[test]
fn unknown_builtin_in_head_lists_builtins() {
    let src = "\
.in
.decl px(name: string, weight: float)
.printsize
.decl f(name: string, weight: float)
.rule
f(N, to_flot(U)) :- px(N, U).
";
    let out = report(src);
    assert!(out.contains("unknown function `to_flot`"), "got:\n{out}");
    assert!(
        out.contains("to_float"),
        "should list builtins, got:\n{out}"
    );
}

#[test]
fn declaration_outside_a_section() {
    let src = ".decl e(x: number)\n";
    let out = report(src);
    assert!(out.contains("declaration outside a section"), "got:\n{out}");
}

#[test]
fn typing_error_points_at_the_rule() {
    let src = "\
.in
.decl e(x: number)
.printsize
.decl r(x: number, y: number)
.rule
r(X, X) :- e(X).
r(X, Y) :- e(X).
";
    let out = report(src);
    assert!(out.contains("head variable Y is not bound"), "got:\n{out}");
    assert!(
        out.contains("7 │"),
        "should point at line 7 (the bad rule), got:\n{out}"
    );
    assert!(out.contains("in this rule"), "got:\n{out}");
}

#[test]
fn float_mixing_error_points_at_the_rule() {
    let src = "\
.in
.decl px(name: string, weight: float)
.printsize
.decl light(name: string)
.rule
light(N) :- px(N, U), U < 1.
";
    let out = report(src);
    assert!(out.contains("float and number/string mixed"), "got:\n{out}");
    assert!(out.contains("6 │"), "should point at line 6, got:\n{out}");
}

#[test]
fn arity_mismatch_points_at_the_rule() {
    let src = "\
.in
.decl e(x: number, y: number)
.printsize
.decl r(x: number)
.rule
r(X) :- e(X).
";
    let out = report(src);
    assert!(
        out.contains("arity mismatch: e is declared with 2 columns"),
        "got:\n{out}"
    );
}

#[test]
fn aggregate_must_be_last() {
    let src = "\
.in
.decl e(x: number, y: number)
.printsize
.decl r(c: number, x: number)
.rule
r(count(Y), X) :- e(X, Y).
";
    let out = report(src);
    assert!(
        out.contains("aggregate must be the LAST argument"),
        "got:\n{out}"
    );
}

#[test]
fn comments_and_optimize_suffixes_parse() {
    let program = syntax::parse(
        "\
// line comment
# hash comment
.in
.decl e(x: number) .input e.facts
.printsize
.decl r(x: number)
.rule
r(X) :- e(X). .plan
",
    )
    .unwrap();
    assert!(program.rules()[0].is_planning());
    assert_eq!(program.edbs()[0].path(), Some("e.facts".to_string()));
}
