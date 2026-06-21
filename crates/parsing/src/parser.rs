use pest::iterators::Pair;
use std::{fmt, fs};

use crate::decl::RelDecl; // crate :: the root of the module tree
use crate::rule::FLRule;
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
        Self { edbs, idbs, rules }
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

        Self { edbs, idbs, rules }
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
