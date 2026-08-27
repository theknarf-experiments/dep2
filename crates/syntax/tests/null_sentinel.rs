//! The two ways a *written* value can collide with the NULL sentinel.
//!
//! `NULL_SENTINEL` is `i64::MIN`, which is both a writable integer literal and
//! the bit pattern of `-0.0`. Neither may reach the engine as a value that
//! silently means NULL — NULL compares false against everything, so such a rule
//! produces nothing at all and gives no clue why.

use parsing::arithmetic::Factor;
use parsing::decl::is_null;
use parsing::head::HeadArg;
use parsing::rule::Const;

/// Pull the head's single constant argument out of a one-rule program. A bare
/// constant parses as a one-factor arithmetic expression.
fn head_const(src: &str) -> Const {
    let program = syntax::parse(src).expect("program should parse");
    let rule = &program.rules()[0];
    let arg = rule
        .head()
        .head_arguments()
        .first()
        .expect("a head argument");
    match arg {
        HeadArg::Arith(a) if a.rest().is_empty() => match a.init() {
            Factor::Const(c) => c.clone(),
            other => panic!("expected a constant factor, got {other:?}"),
        },
        other => panic!("expected a constant head argument, got {other:?}"),
    }
}

#[test]
fn a_negative_zero_literal_is_not_null() {
    let c = head_const(
        ".in\n.decl e(x: float)\n.printsize\n.decl p(x: float)\n.rule\np(-0.0) :- e(_).\n",
    );
    let Const::Float(bits) = c else {
        panic!("expected a float constant, got {c:?}")
    };
    assert!(!is_null(bits), "the literal -0.0 encoded to NULL");
    assert_eq!(f64::from_bits(bits as u64), 0.0);
    assert!(f64::from_bits(bits as u64).is_sign_positive());
}

#[test]
fn the_reserved_integer_literal_is_rejected() {
    let src = ".in\n.decl e(x: number)\n.printsize\n.decl p(x: number)\n\
               .rule\np(-9223372036854775808) :- e(_).\n";
    let diagnostics = syntax::parse(src).expect_err("i64::MIN must not parse as a number");
    let rendered = syntax::render("t.dl", src, &diagnostics, false);
    assert!(
        rendered.contains("reserved") && rendered.contains("NULL"),
        "the error should explain the reservation, got:\n{rendered}"
    );
}

#[test]
fn one_past_the_reserved_value_is_an_ordinary_number() {
    let c = head_const(
        ".in\n.decl e(x: number)\n.printsize\n.decl p(x: number)\n\
         .rule\np(-9223372036854775807) :- e(_).\n",
    );
    assert_eq!(c, Const::Integer(i64::MIN + 1));
    // ... and so is the positive end of the range.
    let c = head_const(
        ".in\n.decl e(x: number)\n.printsize\n.decl p(x: number)\n\
         .rule\np(9223372036854775807) :- e(_).\n",
    );
    assert_eq!(c, Const::Integer(i64::MAX));
}
