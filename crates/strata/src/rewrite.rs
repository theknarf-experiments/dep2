//! Program-level desugaring that runs before stratification.
//!
//! Two rewrites live here:
//! - [`normalize_aggregation_arguments`]: materialize constant/expression
//!   aggregate arguments so the planner only ever sees bare-variable
//!   aggregates.
//! - [`desugar_recursive_aggregation`]: split recursive SUM/COUNT into a
//!   non-aggregated helper fixpoint plus a downstream aggregation stratum.
//!   Recursive MIN/MAX is deliberately NOT split — it aggregates inside the
//!   fixpoint loop (see the function doc for why both directions matter).

use parsing::aggregation::{Aggregation, AggregationOperator};
use parsing::arithmetic::{Arithmetic, Factor};
use parsing::decl::DataType;
use parsing::head::{Head, HeadArg};
use parsing::parser::Program;
use parsing::rule::{Atom, AtomArg, FLRule, Predicate};
use std::collections::{HashMap, HashSet};

/// Normalize aggregation arguments to plain variables.
///
/// The planner materializes an aggregated head's VALUE column only when the
/// aggregate argument is a bare variable; a constant (`min(0)`) paniced at
/// assembly (arity mismatch) and an expression (`min(C + 1)`) silently
/// aggregated the raw variable instead of the expression. Both were masked
/// while every recursive aggregate went through the helper split (which
/// materialized arguments as head arithmetic); with min/max aggregating
/// inside the loop they surface. This rewrite turns
///
/// ```text
/// s(X, min(C + 1)) :- s(Y, C), edge(Y, X).
/// ```
///
/// into a materializing pre-rule plus a variable-argument aggregation:
///
/// ```text
/// s_aggarg0(X, C + 1) :- s(Y, C), edge(Y, X).
/// s(X, min(V))        :- s_aggarg0(X, V).
/// ```
///
/// Head positions are preserved, so aggregation catalogs stay valid.
pub fn normalize_aggregation_arguments(program: Program) -> Program {
    let needs_work = program.rules().iter().any(|r| {
        r.head()
            .head_arguments()
            .iter()
            .any(|a| matches!(a, HeadArg::Aggregation(agg) if !agg.arithmetic().is_var()))
    });
    if !needs_work {
        return program;
    }

    let taken: HashSet<String> = program
        .rules()
        .iter()
        .map(|r| r.head().name().clone())
        .chain(program.edbs().iter().map(|d| d.name().to_string()))
        .chain(program.idbs().iter().map(|d| d.name().to_string()))
        .collect();

    let mut new_rules: Vec<FLRule> = Vec::with_capacity(program.rules().len() * 2);
    let mut fresh = 0usize;
    for rule in program.rules() {
        let head = rule.head();
        let agg_pos = head
            .head_arguments()
            .iter()
            .position(|a| matches!(a, HeadArg::Aggregation(agg) if !agg.arithmetic().is_var()));
        let Some(pos) = agg_pos else {
            new_rules.push(rule.clone());
            continue;
        };
        let HeadArg::Aggregation(agg) = &head.head_arguments()[pos] else {
            unreachable!()
        };
        let (op, dtype) = (*agg.operator(), *agg.data_type());

        let mut pre_name = format!("{}_aggarg{}", head.name(), fresh);
        while taken.contains(&pre_name) {
            pre_name.push('_');
        }
        fresh += 1;

        // Pre-rule: original body, head args verbatim except the aggregate
        // argument, which materializes as plain head arithmetic.
        let pre_args: Vec<HeadArg> = head
            .head_arguments()
            .iter()
            .enumerate()
            .map(|(i, a)| {
                if i == pos {
                    HeadArg::Arith(agg.arithmetic().clone())
                } else {
                    a.clone()
                }
            })
            .collect();
        new_rules.push(FLRule::new(
            Head::new(pre_name.clone(), pre_args),
            rule.rhs().to_vec(),
            rule.is_planning(),
            rule.is_sip(),
        ));

        // Aggregating rule: same head, aggregate over the materialized column.
        let arity = head.head_arguments().len();
        let mut head_args = Vec::with_capacity(arity);
        let mut body_args = Vec::with_capacity(arity);
        for i in 0..arity {
            if i == pos {
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
        new_rules.push(FLRule::new(
            Head::new(head.name().clone(), head_args),
            vec![Predicate::AtomPredicate(Atom::from_str(
                &pre_name, body_args,
            ))],
            false,
            false,
        ));
    }

    Program::new_unchecked(program.edbs().to_vec(), program.idbs().to_vec(), new_rules)
}

/// Split recursive SUM/COUNT aggregation into a stratum split; leave
/// recursive MIN/MAX in the loop.
///
/// Two sound placements exist for a recursively-computed aggregate, and the
/// operator decides which one is correct:
///
/// **MIN/MAX stay inside the fixpoint loop** (no rewrite). Differential's
/// `reduce` retracts superseded values across iterations, so the aggregate
/// converges to the exact minimum/maximum — including value-GENERATING
/// recursions like shortest paths through a positive cycle, where each pass
/// improves the aggregate until nothing improves. The historical stale-label
/// bug (connected components keeping an old label) was NOT the reduce: it was
/// the seed and recursive rules landing in different strata, each aggregating
/// half the tuples. `DependencyGraph::from_parser` now forces every rule of a
/// recursively-aggregated head into one SCC so there is exactly one
/// aggregation site. Splitting min/max instead would enumerate EVERY derivable
/// value in the helper — divergent for value-generating recursions (the old
/// bug-5 wedge). The executor's iteration bound (`DEP2_MAX_ITER`, default
/// 100k) remains as a safety net for fixpoints that genuinely diverge.
///
/// **SUM/COUNT split into a helper.** In-loop sum/count over a recursion is
/// not a lattice — re-derivations would double-count. Their well-defined
/// semantics is over the derived SET, so
///
/// ```text
/// cnt(N, count(C)) :- edge(N, _).
/// cnt(N, count(C)) :- edge(O, N), cnt(O, C).
/// ```
///
/// becomes a *non-aggregated* recursive helper plus a single *non-recursive*
/// aggregation stratum:
///
/// ```text
/// cnt_aggsrc(N, N) :- edge(N, _).
/// cnt_aggsrc(N, C) :- edge(O, N), cnt_aggsrc(O, C).
/// cnt(K0, count(V)) :- cnt_aggsrc(K0, V).
/// ```
///
/// The aggregated head name, operator and arity are preserved, so an
/// aggregation catalog built from either program stays valid.
///
/// Mixed cycles split as a whole: if any head in a mutual-recursion cycle is
/// SUM/COUNT, every aggregated head in that cycle is lifted into helpers
/// (one semantics per SCC — a half-split cycle would re-introduce the
/// two-aggregation-sites bug). Within a helper rule, references to other
/// aggregated heads in the same cycle are redirected to their helpers;
/// references to aggregated heads in *earlier* strata stay aggregated (so a
/// `sum` over an upstream `min` sums minimised values).
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
    let recursive_all: HashSet<String> = agg_heads
        .iter()
        .filter(|h| reaches_self(h, &deps))
        .cloned()
        .collect();
    if recursive_all.is_empty() {
        return program;
    }

    // Operator per recursive aggregated head (from the first aggregated rule).
    let mut op_of: HashMap<String, AggregationOperator> = HashMap::new();
    for rule in rules {
        let h = rule.head().name();
        if recursive_all.contains(h) && !op_of.contains_key(h) {
            if let Some(HeadArg::Aggregation(agg)) = rule
                .head()
                .head_arguments()
                .iter()
                .find(|a| matches!(a, HeadArg::Aggregation(_)))
            {
                op_of.insert(h.clone(), *agg.operator());
            }
        }
    }

    // Only SUM/COUNT are split: set-then-aggregate is their well-defined
    // semantics over a recursive closure (counting/summing the derived SET).
    // MIN/MAX stay recursive and aggregate INSIDE the fixpoint loop — the
    // lattice iteration converges even when the aggregated value is generated
    // (e.g. shortest-path distances), where the helper split would enumerate
    // unboundedly many values. A mixed cycle (a min head mutually recursive
    // with a sum head) splits entirely, so one SCC gets one semantics.
    let needs_split = |h: &String| {
        matches!(
            op_of.get(h),
            Some(AggregationOperator::Sum) | Some(AggregationOperator::Count)
        )
    };
    let recursive_agg: HashSet<String> = recursive_all
        .iter()
        .filter(|h| {
            needs_split(h)
                || recursive_all
                    .iter()
                    .any(|x| needs_split(x) && reaches(h, x, &deps) && reaches(x, h, &deps))
        })
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

    // For each recursive aggregated head, the set of aggregated heads in its
    // recursion cycle (its SCC, including itself). Within a helper rule we
    // redirect references to *cycle mates* to their helpers — that pulls every
    // aggregated head in the cycle (self- or mutually-recursive) out of the
    // recursive SCC. References to aggregated heads in *earlier* strata stay
    // aggregated, so e.g. `sum` over an upstream `min` sums the minimised values.
    let mut cycle_mates: HashMap<String, HashSet<String>> = HashMap::new();
    for h in &recursive_agg {
        let mates: HashSet<String> = recursive_agg
            .iter()
            .filter(|x| reaches(h, x, &deps) && reaches(x, h, &deps))
            .cloned()
            .chain(std::iter::once(h.clone()))
            .collect();
        cycle_mates.insert(h.clone(), mates);
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
    // with cycle-mate references in their bodies pointed at the matching helpers.
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
                let mates = &cycle_mates[h];
                let new_rhs: Vec<Predicate> = rule
                    .rhs()
                    .iter()
                    .map(|p| rename_atom(p, mates, &helper_of))
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

    // Unchecked: the rules are already typed, and the generated *_aggsrc
    // helper heads are deliberately undeclared (they are engine-internal).
    Program::new_unchecked(program.edbs().to_vec(), program.idbs().to_vec(), new_rules)
}

/// Is `to` reachable from `from` over ≥1 edges of the head-name dependency graph?
/// (`reaches(x, x, _)` is true iff `x` lies on a cycle.)
fn reaches(from: &str, to: &str, deps: &HashMap<String, HashSet<String>>) -> bool {
    let mut stack: Vec<&str> = deps
        .get(from)
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let mut visited: HashSet<&str> = HashSet::new();
    while let Some(n) = stack.pop() {
        if n == to {
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

/// Does `start` lie on a cycle in the head-name dependency graph?
fn reaches_self(start: &str, deps: &HashMap<String, HashSet<String>>) -> bool {
    reaches(start, start, deps)
}

/// Clone `pred`, redirecting a positive/negated atom whose name is in `mates`
/// to that name's helper relation in `helper_of`.
fn rename_atom(
    pred: &Predicate,
    mates: &HashSet<String>,
    helper_of: &HashMap<String, String>,
) -> Predicate {
    let redirect = |a: &Atom| -> Option<Atom> {
        if mates.contains(a.name()) {
            Some(Atom::from_str(&helper_of[a.name()], a.arguments().clone()))
        } else {
            None
        }
    };
    match pred {
        Predicate::AtomPredicate(a) => match redirect(a) {
            Some(r) => Predicate::AtomPredicate(r),
            None => pred.clone(),
        },
        Predicate::NegatedAtomPredicate(a) => match redirect(a) {
            Some(r) => Predicate::NegatedAtomPredicate(r),
            None => pred.clone(),
        },
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

    fn agg_count(var: &str) -> HeadArg {
        HeadArg::Aggregation(Aggregation::with_type(
            AggregationOperator::Count,
            Arithmetic::with_type(Factor::Var(var.to_string()), vec![], DataType::Integer),
            DataType::Integer,
        ))
    }

    fn atom(name: &str, args: Vec<AtomArg>) -> Predicate {
        Predicate::AtomPredicate(Atom::from_str(name, args))
    }

    #[test]
    fn min_cc_stays_recursive() {
        // Recursive MIN aggregates INSIDE the fixpoint loop: the desugar must
        // leave it untouched (the split would enumerate every reachable label
        // — and unboundedly many values when the aggregate is generated).
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
        assert_eq!(rules.len(), 2, "min must not split");
        assert!(rules.iter().all(|r| r.head().name() == "cc"));
        assert!(rules.iter().all(|r| r
            .head()
            .head_arguments()
            .iter()
            .any(|a| matches!(a, HeadArg::Aggregation(_)))));
    }

    #[test]
    fn recursive_count_is_split() {
        // COUNT (like SUM) keeps the helper split: counting the derived SET
        // is its well-defined semantics over a recursive closure.
        let base = FLRule::new(
            Head::new(
                "cnt".to_string(),
                vec![HeadArg::Var("N".to_string()), agg_count("N")],
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
                "cnt".to_string(),
                vec![HeadArg::Var("N".to_string()), agg_count("C")],
            ),
            vec![
                atom(
                    "edge",
                    vec![AtomArg::Var("O".to_string()), AtomArg::Var("N".to_string())],
                ),
                atom(
                    "cnt",
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
        let names: Vec<String> = rules.iter().map(|r| r.head().name().clone()).collect();
        assert_eq!(
            names.iter().filter(|n| n.as_str() == "cnt_aggsrc").count(),
            2
        );
        assert_eq!(names.iter().filter(|n| n.as_str() == "cnt").count(), 1);
    }

    #[test]
    fn normalization_materializes_const_and_expr_args() {
        // `s(X, min(0))` and `s(X, min(C + 1))` must become a materializing
        // pre-rule plus a variable-argument aggregation — the planner only
        // handles bare-variable aggregate arguments.
        let konst = FLRule::new(
            Head::new(
                "s".to_string(),
                vec![
                    HeadArg::Var("X".to_string()),
                    HeadArg::Aggregation(Aggregation::with_type(
                        AggregationOperator::Min,
                        Arithmetic::with_type(
                            Factor::Const(parsing::rule::Const::Integer(0)),
                            vec![],
                            DataType::Integer,
                        ),
                        DataType::Integer,
                    )),
                ],
            ),
            vec![atom("id", vec![AtomArg::Var("X".to_string())])],
            false,
            false,
        );
        let out =
            normalize_aggregation_arguments(Program::new_unchecked(vec![], vec![], vec![konst]));
        let rules = out.rules();
        assert_eq!(rules.len(), 2);
        // Pre-rule materializes the constant as head arithmetic...
        assert!(rules[0].head().name().starts_with("s_aggarg"));
        assert!(matches!(
            rules[0].head().head_arguments()[1],
            HeadArg::Arith(_)
        ));
        // ...and the aggregating rule aggregates a plain variable.
        assert_eq!(rules[1].head().name(), "s");
        match &rules[1].head().head_arguments()[1] {
            HeadArg::Aggregation(agg) => assert!(agg.arithmetic().is_var()),
            other => panic!("expected aggregation, got {:?}", other),
        }
    }

    #[test]
    fn mutual_min_stays_recursive_and_mixed_cycle_splits() {
        // a(N, min(C)) :- seed(N, C).
        // a(N, min(C)) :- edge(N, M), b(M, C).
        // b(N, min(C)) :- edge(N, M), a(M, C).
        let mk = |head: &str, body_atom: Option<&str>, agg: HeadArg| {
            let rhs = match body_atom {
                Some(b) => vec![
                    atom(
                        "edge",
                        vec![AtomArg::Var("N".to_string()), AtomArg::Var("M".to_string())],
                    ),
                    atom(
                        b,
                        vec![AtomArg::Var("M".to_string()), AtomArg::Var("C".to_string())],
                    ),
                ],
                None => vec![atom(
                    "seed",
                    vec![AtomArg::Var("N".to_string()), AtomArg::Var("C".to_string())],
                )],
            };
            FLRule::new(
                Head::new(head.to_string(), vec![HeadArg::Var("N".to_string()), agg]),
                rhs,
                false,
                false,
            )
        };

        // Pure-min mutual recursion: both heads stay aggregated and recursive.
        let out = desugar_recursive_aggregation(Program::new_unchecked(
            vec![],
            vec![],
            vec![
                mk("a", None, agg_min("C")),
                mk("a", Some("b"), agg_min("C")),
                mk("b", Some("a"), agg_min("C")),
            ],
        ));
        assert_eq!(out.rules().len(), 3, "pure-min cycle must not split");

        // A COUNT head in the cycle splits the WHOLE cycle (one semantics per SCC).
        let out = desugar_recursive_aggregation(Program::new_unchecked(
            vec![],
            vec![],
            vec![
                mk("a", None, agg_min("C")),
                mk("a", Some("b"), agg_min("C")),
                mk("b", Some("a"), agg_count("C")),
            ],
        ));
        let names: Vec<String> = out
            .rules()
            .iter()
            .map(|r| r.head().name().clone())
            .collect();
        assert!(names.contains(&"a_aggsrc".to_string()), "got: {names:?}");
        assert!(names.contains(&"b_aggsrc".to_string()), "got: {names:?}");
    }

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
