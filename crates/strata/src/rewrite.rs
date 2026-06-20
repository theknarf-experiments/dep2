//! Program-level desugaring that runs before stratification.
//!
//! Currently this houses the **recursive-aggregation stratum split**, which makes
//! aggregates computed by a recursive rule sound under the incremental (`isize`)
//! semiring.

use parsing::aggregation::{Aggregation, AggregationOperator};
use parsing::arithmetic::{Arithmetic, Factor};
use parsing::decl::DataType;
use parsing::head::{Head, HeadArg};
use parsing::parser::Program;
use parsing::rule::{Atom, AtomArg, FLRule, Predicate};
use std::collections::{HashMap, HashSet};

/// Desugar self-recursive aggregation into a stratum split.
///
/// A recursive aggregated head `H(K.., agg(V))` is unsound under the incremental
/// `isize` semiring: the aggregation runs *inside* the recursive fixpoint loop,
/// and superseded values are not retracted across iterations (e.g. connected
/// components keeps a stale label). This rewrite turns
///
/// ```text
/// cc(N, min(N)) :- edge(N, _).
/// cc(N, min(C)) :- edge(O, N), cc(O, C).
/// ```
///
/// into a *non-aggregated* recursive helper plus a single *non-recursive*
/// aggregation stratum:
///
/// ```text
/// cc_aggsrc(N, N) :- edge(N, _).
/// cc_aggsrc(N, C) :- edge(O, N), cc_aggsrc(O, C).
/// cc(K0, min(V))  :- cc_aggsrc(K0, V).
/// ```
///
/// The helper recursion is a plain least fixpoint (handled correctly across
/// cycles) and the aggregation is now a downstream non-recursive stratum (also
/// correct). The aggregated head name, operator and arity are preserved, so an
/// aggregation catalog built from either the original or the rewritten program
/// stays valid.
///
/// Only *self*-recursive aggregated heads are rewritten (the documented bug).
/// The aggregate must range over a finite value domain for the helper fixpoint
/// to terminate — true for min/max label propagation (connected components);
/// shortest paths through a positive cycle would diverge, as in any pure-Datalog
/// encoding. Mutual recursion between two *distinct* aggregated heads is left
/// unchanged (no worse than before).
pub fn desugar_recursive_aggregation(program: Program) -> Program {
    let rules = program.rules();

    // Aggregated heads: any head carrying an `agg(..)` argument.
    let agg_heads: HashSet<String> = rules
        .iter()
        .filter(|r| {
            r.head()
                .head_arguments()
                .iter()
                .any(|a| matches!(a, HeadArg::Aggregation(_)))
        })
        .map(|r| r.head().name().clone())
        .collect();
    if agg_heads.is_empty() {
        return program;
    }

    // Head-name dependency graph over body atoms (positive + negated) that are
    // themselves rule heads.
    let head_names: HashSet<String> = rules.iter().map(|r| r.head().name().clone()).collect();
    let mut deps: HashMap<String, HashSet<String>> = HashMap::new();
    for rule in rules {
        let entry = deps.entry(rule.head().name().clone()).or_default();
        for pred in rule.rhs() {
            let name = match pred {
                Predicate::AtomPredicate(a) | Predicate::NegatedAtomPredicate(a) => a.name(),
                Predicate::ComparePredicate(_) => continue,
            };
            if head_names.contains(name) {
                entry.insert(name.to_string());
            }
        }
    }

    // Aggregated heads that (transitively) depend on themselves.
    let recursive_agg: HashSet<String> = agg_heads
        .iter()
        .filter(|h| reaches_self(h, &deps))
        .cloned()
        .collect();
    if recursive_agg.is_empty() {
        return program;
    }

    // Fresh helper name per recursive aggregated head, avoiding collisions.
    let mut helper_of: HashMap<String, String> = HashMap::new();
    for h in &recursive_agg {
        let mut name = format!("{}_aggsrc", h);
        while head_names.contains(&name)
            || program.edbs().iter().any(|d| d.name() == name)
            || program.idbs().iter().any(|d| d.name() == name)
        {
            name.push('_');
        }
        helper_of.insert(h.clone(), name);
    }

    // Aggregation template per recursive head: (operator, data type, arity,
    // position of the aggregate argument). Taken from the first matching rule.
    let mut agg_info: HashMap<String, (AggregationOperator, DataType, usize, usize)> =
        HashMap::new();
    for rule in rules {
        let h = rule.head().name();
        if recursive_agg.contains(h) && !agg_info.contains_key(h) {
            let args = rule.head().head_arguments();
            if let Some(pos) = args
                .iter()
                .position(|a| matches!(a, HeadArg::Aggregation(_)))
            {
                if let HeadArg::Aggregation(agg) = &args[pos] {
                    agg_info.insert(
                        h.clone(),
                        (*agg.operator(), *agg.data_type(), args.len(), pos),
                    );
                }
            }
        }
    }

    // Rewrite rules: recursive aggregated heads become un-aggregated helpers,
    // with self-references in their bodies pointed at the helper.
    let mut new_rules: Vec<FLRule> = Vec::with_capacity(rules.len() + recursive_agg.len());
    for rule in rules {
        let h = rule.head().name();
        match helper_of.get(h) {
            Some(helper) => {
                let new_args: Vec<HeadArg> = rule
                    .head()
                    .head_arguments()
                    .iter()
                    .map(|a| match a {
                        HeadArg::Aggregation(agg) => {
                            let arith = agg.arithmetic().clone();
                            if arith.is_var() {
                                HeadArg::Var(arith.vars()[0].clone())
                            } else {
                                HeadArg::Arith(arith)
                            }
                        }
                        other => other.clone(),
                    })
                    .collect();
                let new_head = Head::new(helper.clone(), new_args);
                let new_rhs: Vec<Predicate> = rule
                    .rhs()
                    .iter()
                    .map(|p| rename_atom(p, h, helper))
                    .collect();
                new_rules.push(FLRule::new(
                    new_head,
                    new_rhs,
                    rule.is_planning(),
                    rule.is_sip(),
                ));
            }
            None => new_rules.push(rule.clone()),
        }
    }

    // Emit the non-recursive aggregation rule for each split head, deterministically.
    let mut split: Vec<&String> = recursive_agg.iter().collect();
    split.sort();
    for h in split {
        let helper = &helper_of[h];
        let (op, dtype, arity, agg_pos) = agg_info[h];

        let mut head_args = Vec::with_capacity(arity);
        let mut body_args = Vec::with_capacity(arity);
        for i in 0..arity {
            if i == agg_pos {
                let arith = Arithmetic::with_type(Factor::Var("AggV".to_string()), vec![], dtype);
                head_args.push(HeadArg::Aggregation(Aggregation::with_type(
                    op, arith, dtype,
                )));
                body_args.push(AtomArg::Var("AggV".to_string()));
            } else {
                let v = format!("AggK{}", i);
                head_args.push(HeadArg::Var(v.clone()));
                body_args.push(AtomArg::Var(v));
            }
        }

        let head = Head::new(h.clone(), head_args);
        let body = vec![Predicate::AtomPredicate(Atom::from_str(helper, body_args))];
        new_rules.push(FLRule::new(head, body, false, false));
    }

    Program::new(program.edbs().to_vec(), program.idbs().to_vec(), new_rules)
}

/// Does `start` reach itself through the head-name dependency graph?
fn reaches_self(start: &str, deps: &HashMap<String, HashSet<String>>) -> bool {
    let mut stack: Vec<&str> = deps
        .get(start)
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let mut visited: HashSet<&str> = HashSet::new();
    while let Some(n) = stack.pop() {
        if n == start {
            return true;
        }
        if !visited.insert(n) {
            continue;
        }
        if let Some(next) = deps.get(n) {
            stack.extend(next.iter().map(String::as_str));
        }
    }
    false
}

/// Clone `pred`, renaming a positive/negated atom whose name is `from` to `to`.
fn rename_atom(pred: &Predicate, from: &str, to: &str) -> Predicate {
    match pred {
        Predicate::AtomPredicate(a) if a.name() == from => {
            Predicate::AtomPredicate(Atom::from_str(to, a.arguments().clone()))
        }
        Predicate::NegatedAtomPredicate(a) if a.name() == from => {
            Predicate::NegatedAtomPredicate(Atom::from_str(to, a.arguments().clone()))
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agg_min(var: &str) -> HeadArg {
        HeadArg::Aggregation(Aggregation::with_type(
            AggregationOperator::Min,
            Arithmetic::with_type(Factor::Var(var.to_string()), vec![], DataType::Integer),
            DataType::Integer,
        ))
    }

    fn atom(name: &str, args: Vec<AtomArg>) -> Predicate {
        Predicate::AtomPredicate(Atom::from_str(name, args))
    }

    fn body_atom_names(rule: &FLRule) -> Vec<String> {
        rule.rhs()
            .iter()
            .filter_map(|p| match p {
                Predicate::AtomPredicate(a) | Predicate::NegatedAtomPredicate(a) => {
                    Some(a.name().to_string())
                }
                Predicate::ComparePredicate(_) => None,
            })
            .collect()
    }

    /// `cc(N, min(C))` self-recursion is split into an un-aggregated recursive
    /// helper plus a downstream non-recursive aggregation that keeps the `cc`
    /// name, operator and arity.
    #[test]
    fn cc_is_split() {
        let base = FLRule::new(
            Head::new(
                "cc".to_string(),
                vec![HeadArg::Var("N".to_string()), agg_min("N")],
            ),
            vec![atom(
                "edge",
                vec![AtomArg::Var("N".to_string()), AtomArg::Placeholder],
            )],
            false,
            false,
        );
        let rec = FLRule::new(
            Head::new(
                "cc".to_string(),
                vec![HeadArg::Var("N".to_string()), agg_min("C")],
            ),
            vec![
                atom(
                    "edge",
                    vec![AtomArg::Var("O".to_string()), AtomArg::Var("N".to_string())],
                ),
                atom(
                    "cc",
                    vec![AtomArg::Var("O".to_string()), AtomArg::Var("C".to_string())],
                ),
            ],
            false,
            false,
        );
        let out = desugar_recursive_aggregation(Program::new(vec![], vec![], vec![base, rec]));
        let rules = out.rules();
        // two helper rules + one aggregation rule.
        assert_eq!(rules.len(), 3);

        // The aggregation rule keeps head `cc`, still aggregated, sourced from the helper.
        let agg_rule = rules.iter().find(|r| r.head().name() == "cc").unwrap();
        assert!(agg_rule
            .head()
            .head_arguments()
            .iter()
            .any(|a| matches!(a, HeadArg::Aggregation(_))));
        assert_eq!(agg_rule.head().arity(), 2);
        assert_eq!(body_atom_names(agg_rule), vec!["cc_aggsrc".to_string()]);

        // Helper rules carry no aggregation; the recursive one self-references the
        // helper, never the aggregated `cc`.
        let helpers: Vec<&FLRule> = rules
            .iter()
            .filter(|r| r.head().name() == "cc_aggsrc")
            .collect();
        assert_eq!(helpers.len(), 2);
        for r in &helpers {
            assert!(r
                .head()
                .head_arguments()
                .iter()
                .all(|a| !matches!(a, HeadArg::Aggregation(_))));
        }
        let rec_helper = helpers.iter().find(|r| r.rhs().len() == 2).unwrap();
        let names = body_atom_names(rec_helper);
        assert!(names.contains(&"cc_aggsrc".to_string()));
        assert!(!names.contains(&"cc".to_string()));
    }

    /// Non-recursive aggregation is left untouched (already correct).
    #[test]
    fn non_recursive_aggregation_untouched() {
        let rule = FLRule::new(
            Head::new(
                "mk".to_string(),
                vec![HeadArg::Var("X".to_string()), agg_min("Z")],
            ),
            vec![atom(
                "triple",
                vec![
                    AtomArg::Var("X".to_string()),
                    AtomArg::Placeholder,
                    AtomArg::Var("Z".to_string()),
                ],
            )],
            false,
            false,
        );
        let out = desugar_recursive_aggregation(Program::new(vec![], vec![], vec![rule]));
        assert_eq!(out.rules().len(), 1);
        assert_eq!(out.rules()[0].head().name(), "mk");
        assert!(out.rules()[0]
            .head()
            .head_arguments()
            .iter()
            .any(|a| matches!(a, HeadArg::Aggregation(_))));
    }
}
