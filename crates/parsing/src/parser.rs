use pest::iterators::Pair;
use std::{fmt, fs};

use crate::decl::RelDecl; // crate :: the root of the module tree
use crate::rule::{Const, FLRule};
use crate::{FlowLogParser, Parser, Rule};

pub trait Lexeme {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self;
}

#[derive(Debug, Clone)]
pub struct Program {
    edbs: Vec<RelDecl>,
    idbs: Vec<RelDecl>,
    rules: Vec<FLRule>,
}

impl fmt::Display for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let edbs = self
            .edbs
            .iter()
            .map(|rel_decl| rel_decl.to_string())
            .collect::<Vec<String>>()
            .join("\n");

        let idbs = self
            .idbs
            .iter()
            .map(|rel_decl| rel_decl.to_string())
            .collect::<Vec<String>>()
            .join("\n");

        let rules = self
            .rules
            .iter()
            .map(|rule| rule.to_string())
            .collect::<Vec<String>>()
            .join("\n");

        write!(
            f,
            ".in \n{}\n.printsize \n{}\n.rule \n{}",
            edbs, idbs, rules
        )
    }
}

impl Program {
    pub fn new(edbs: Vec<RelDecl>, idbs: Vec<RelDecl>, rules: Vec<FLRule>) -> Self {
        // The historical contract: program loading panics on type/validation
        // errors. Span-aware front-ends use `try_new` instead.
        Self::try_new(edbs, idbs, rules).unwrap_or_else(|e| panic!("{}", e.message))
    }

    /// Like [`Program::new`] but reporting typing/validation failures as a
    /// [`crate::typing::TypeError`] (with the offending rule's index) instead
    /// of panicking.
    pub fn try_new(
        edbs: Vec<RelDecl>,
        idbs: Vec<RelDecl>,
        mut rules: Vec<FLRule>,
    ) -> Result<Self, crate::typing::TypeError> {
        // Resolve float-vs-integer evaluation modes from the declared column
        // types before anything downstream reads them (the parser defaults
        // every expression to Integer).
        crate::typing::resolve_types(&edbs, &idbs, &mut rules)?;
        Ok(Self { edbs, idbs, rules })
    }

    pub fn edbs(&self) -> &[RelDecl] {
        &self.edbs
    }

    pub fn idbs(&self) -> &[RelDecl] {
        &self.idbs
    }

    pub fn rules(&self) -> &[FLRule] {
        &self.rules
    }

    /// Apply `f` to every constant in every rule (head expressions,
    /// aggregations, atom arguments, comparisons — including nested
    /// sub-expressions). `f` returning `Some` replaces the constant. Used to
    /// intern string literals into ids at the AST level, replacing the
    /// pre-parse textual rewrite.
    pub fn map_constants(&mut self, mut f: impl FnMut(&Const) -> Option<Const>) {
        use crate::head::HeadArg;
        use crate::rule::{AtomArg, Predicate};
        for rule in &mut self.rules {
            for arg in rule.head_mut().head_arguments_mut() {
                match arg {
                    HeadArg::Var(_) => {}
                    HeadArg::Arith(arith) => arith.map_constants(&mut f),
                    HeadArg::Aggregation(agg) => agg.arithmetic_mut().map_constants(&mut f),
                }
            }
            for pred in rule.rhs_mut() {
                match pred {
                    Predicate::AtomPredicate(atom) | Predicate::NegatedAtomPredicate(atom) => {
                        for arg in atom.arguments_mut() {
                            if let AtomArg::Const(c) = arg {
                                if let Some(new) = f(c) {
                                    *c = new;
                                }
                            }
                        }
                    }
                    Predicate::ComparePredicate(cmp) => {
                        cmp.left_mut().map_constants(&mut f);
                        cmp.right_mut().map_constants(&mut f);
                    }
                }
            }
        }
    }

    pub fn parse_from(path: &str) -> Self {
        let unparsed_str = fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("can't read program from \"{}\"", path));

        let parsed_rule = FlowLogParser::parse(Rule::main_grammar, &unparsed_str)
            .unwrap_or_else(|error| panic!("can't parse program from \"{}\": \n{:?}", path, error))
            .next()
            .unwrap();
        Self::from_parsed_rule(parsed_rule)
    }
}

impl Lexeme for Program {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self {
        let inner_rules = parsed_rule.into_inner();
        let mut edbs: Vec<RelDecl> = Vec::new();
        let mut idbs: Vec<RelDecl> = Vec::new();
        let mut rules: Vec<FLRule> = Vec::new();

        fn parse_rel_decls(vec: &mut Vec<RelDecl>, rule: Pair<Rule>) {
            for rel_decl in rule.into_inner() {
                vec.push(RelDecl::from_parsed_rule(rel_decl));
            }
        }

        // idb sections lead with the section keyword (idb_section); `.out` marks
        // its relations force-serve.
        fn parse_idb_decls(vec: &mut Vec<RelDecl>, rule: Pair<Rule>) {
            let mut inner = rule.into_inner();
            let section = inner.next().unwrap();
            let force_serve = section.as_str() == ".out";
            for rel_decl in inner {
                let mut decl = RelDecl::from_parsed_rule(rel_decl);
                decl.set_force_serve(force_serve);
                vec.push(decl);
            }
        }

        fn parse_rules(vec: &mut Vec<FLRule>, rule: Pair<Rule>) {
            for rule in rule.into_inner() {
                vec.push(FLRule::from_parsed_rule(rule));
            }
        }

        for inner_rule in inner_rules {
            match inner_rule.as_rule() {
                Rule::edb_decl => parse_rel_decls(&mut edbs, inner_rule),
                Rule::idb_decl => parse_idb_decls(&mut idbs, inner_rule),
                Rule::rule_decl => parse_rules(&mut rules, inner_rule),
                _ => {}
            }
        }

        // Through `new` so the typing pass runs on every parsed program.
        Self::new(edbs, idbs, rules)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Program {
        let pair = FlowLogParser::parse(Rule::main_grammar, src)
            .unwrap()
            .next()
            .unwrap();
        Program::from_parsed_rule(pair)
    }

    #[test]
    fn float_literals_and_parens_parse_and_type() {
        use crate::decl::DataType;
        use crate::rule::Predicate;

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
        let prog = parse(src);

        // The comparison against a float literal runs in Float mode.
        let light = &prog.rules()[0];
        let Predicate::ComparePredicate(cmp) = &light.rhs()[1] else {
            panic!("expected a comparison");
        };
        assert_eq!(*cmp.left().data_type(), DataType::Float);
        assert_eq!(*cmp.right().data_type(), DataType::Float);

        // The parenthesised head expression parses, keeps variable order, and
        // is typed Float from the head decl.
        let mid = &prog.rules()[1];
        let crate::head::HeadArg::Arith(arith) = &mid.head().head_arguments()[1] else {
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
    #[should_panic(expected = "type error: float and number/string mixed")]
    fn mixing_float_and_integer_is_rejected() {
        parse(
            "\
.in
.decl sample(name: string, weight: float)
.printsize
.decl light(name: string)
.rule
light(N) :- sample(N, U), U < 1.
",
        );
    }

    #[test]
    fn conversion_builtins_bridge_the_modes() {
        use crate::decl::DataType;
        use crate::head::HeadArg;

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
        let prog = parse(src);
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
    #[should_panic(expected = "to_float takes one number argument")]
    fn to_float_of_a_float_is_rejected() {
        parse(
            "\
.in
.decl sample(name: string, weight: float)
.printsize
.decl f(name: string, weight: float)
.rule
f(N, to_float(U)) :- sample(N, U).
",
        );
    }

    #[test]
    #[should_panic(expected = "arity mismatch: e is declared with 2 columns")]
    fn body_arity_mismatch_is_rejected() {
        parse(
            "\
.in
.decl e(x: number, y: number)
.printsize
.decl r(x: number)
.rule
r(X) :- e(X).
",
        );
    }

    #[test]
    #[should_panic(expected = "head variable Y is not bound")]
    fn unbound_head_variable_is_rejected() {
        parse(
            "\
.in
.decl e(x: number)
.printsize
.decl r(x: number, y: number)
.rule
r(X, Y) :- e(X).
",
        );
    }

    #[test]
    #[should_panic(expected = "used in `!f(X, Z)` but not bound")]
    fn negated_only_variable_is_rejected() {
        parse(
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
    }

    #[test]
    #[should_panic(
        expected = "type conflict: variable X is bound to a string column and a number column"
    )]
    fn string_number_join_is_rejected() {
        parse(
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
    }

    #[test]
    fn integer_expressions_stay_integer_mode() {
        use crate::decl::DataType;
        use crate::rule::Predicate;

        let src = "\
.in
.decl e(x: number, y: number)
.printsize
.decl big(x: number)
.rule
big(X) :- e(X, Y), X > Y + 100.
";
        let prog = parse(src);
        let Predicate::ComparePredicate(cmp) = &prog.rules()[0].rhs()[1] else {
            panic!("expected a comparison");
        };
        assert_eq!(*cmp.left().data_type(), DataType::Integer);
    }

    #[test]
    fn aggregation_over_float_column_types_float() {
        use crate::decl::DataType;
        use crate::head::HeadArg;

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
        let prog = parse(src);
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
    fn out_section_marks_force_serve() {
        // `a` is declared `.printsize`, `b` is declared `.out`; both are consumed.
        let src = "\
.in
.decl e(x: number)
.printsize
.decl a(x: number)
.out
.decl b(x: number)
.rule
a(X) :- e(X).
b(X) :- a(X).
c(X) :- b(X).
";
        let prog = parse(src);
        let a = prog.idbs().iter().find(|d| d.name() == "a").unwrap();
        let b = prog.idbs().iter().find(|d| d.name() == "b").unwrap();
        assert!(
            !a.force_serve(),
            "`.printsize` relation must not force-serve"
        );
        assert!(b.force_serve(), "`.out` relation must force-serve");
    }
}
