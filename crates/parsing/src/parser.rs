use std::fmt;

use crate::decl::RelDecl; // crate :: the root of the module tree
use crate::rule::{Const, FLRule};

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

    /// Construct WITHOUT the typing/validation pass. For engine-internal
    /// rewrites (e.g. the recursive-aggregation desugar) that rebuild a
    /// program from already-typed rules plus generated helper rules whose
    /// heads are deliberately undeclared — re-validating those would reject
    /// them, and re-typing is unnecessary since every expression already
    /// carries its resolved evaluation mode.
    pub fn new_unchecked(edbs: Vec<RelDecl>, idbs: Vec<RelDecl>, rules: Vec<FLRule>) -> Self {
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
}
