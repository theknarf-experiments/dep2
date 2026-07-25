use itertools::Itertools;
use parsing::parser::Program;
use parsing::rule::Predicate;
use std::collections::{HashMap, HashSet};

use std::fmt;
use std::fmt::Write;

#[derive(Debug, Clone)]
pub struct DependencyGraph {
    rule_idb_names: Vec<String>,
    rule_dependency_map: HashMap<usize, HashSet<usize>>,
    negation_dependency_map: HashMap<usize, HashSet<usize>>,
}

impl DependencyGraph {
    pub fn rule_idb_names(&self) -> &[String] {
        &self.rule_idb_names
    }

    pub fn rule_dependency_map(&self) -> &HashMap<usize, HashSet<usize>> {
        &self.rule_dependency_map
    }

    pub fn negation_dependency_map(&self) -> &HashMap<usize, HashSet<usize>> {
        &self.negation_dependency_map
    }

    /* main constructor */
    pub fn from_parser(program: &Program) -> Self {
        let rules = program.rules();
        let rule_idb_names = rules
            .iter()
            .map(|rule| rule.head().name().clone())
            .collect::<Vec<String>>();

        // debug!(".depgraph rule_idb_names = {:?}", rule_idb_names);

        /* head2rule_ids_map maps head_name to rule_ids of that head */
        let mut head2rule_ids_map = HashMap::new();
        for (rule_id, rule) in rules.iter().enumerate() {
            let head_name = rule.head().name();
            let rule_ids = head2rule_ids_map
                .entry(String::from(head_name))
                .or_insert(Vec::new()); // or_insert() returns a mutable reference to the value
            rule_ids.push(rule_id);
        }

        let mut rule_dependency_map: HashMap<usize, HashSet<usize>> =
            (0..rules.len()).map(|i| (i, HashSet::new())).collect();
        let mut negation_dependency_map: HashMap<usize, HashSet<usize>> =
            (0..rules.len()).map(|i| (i, HashSet::new())).collect();

        for (rule_id, rule) in rules.iter().enumerate() {
            for predicate in rule.rhs() {
                let atom_name = match predicate {
                    // S :- ...
                    // T :- ... S ...
                    // T depends on S
                    Predicate::AtomPredicate(atom) => atom.name(),

                    // S :- ...
                    // T :- ... !S ...
                    // T (the next strata) depends on S
                    Predicate::NegatedAtomPredicate(atom) => {
                        if let Some(atom_as_head_rule_ids) = head2rule_ids_map.get(atom.name()) {
                            negation_dependency_map
                                .get_mut(&rule_id)
                                .unwrap()
                                .extend(atom_as_head_rule_ids.iter().copied());
                        }
                        atom.name()
                    }
                    _ => continue, /* skip comparison op */
                };

                if let Some(atom_as_head_rule_ids) = head2rule_ids_map.get(atom_name) {
                    rule_dependency_map
                        .get_mut(&rule_id)
                        .unwrap()
                        .extend(atom_as_head_rule_ids.iter().copied()); // rule_id depends on as_head_rule_id （extends() adds all elements to the HashSet）
                }
            }
        }

        // All rules of a RECURSIVELY-aggregated head are made mutually
        // dependent, so they share an SCC — and therefore a single stratum
        // where the aggregate is computed at exactly one place, inside the
        // recursive fixpoint. Without this, a seed rule (which depends only
        // on its inputs, not on the head) lands in an EARLIER stratum whose
        // partial aggregate is emitted and never retracted when the recursive
        // stratum's aggregation supersedes it (the historical stale-label
        // bug: cc kept `cc(2,2)` for edges {(0,2),(2,0)}).
        // `merge(op)` relations need the same treatment for the same reason:
        // the fold is declared on the relation, so a seed rule and a recursive
        // rule must land in ONE stratum or the seed's unmerged value escapes.
        let agg_heads: HashSet<&str> = rules
            .iter()
            .filter(|r| {
                r.head()
                    .head_arguments()
                    .iter()
                    .any(|a| matches!(a, parsing::head::HeadArg::Aggregation(_)))
            })
            .map(|r| r.head().name().as_str())
            .chain(
                program
                    .idbs()
                    .iter()
                    .filter(|d| d.merge().is_some())
                    .map(|d| d.name()),
            )
            .collect();
        if !agg_heads.is_empty() {
            // Head-level dependency edges (positive and negated atoms).
            let mut head_deps: HashMap<&str, HashSet<&str>> = HashMap::new();
            for rule in rules {
                let entry = head_deps.entry(rule.head().name().as_str()).or_default();
                for pred in rule.rhs() {
                    let name = match pred {
                        Predicate::AtomPredicate(a) | Predicate::NegatedAtomPredicate(a) => {
                            a.name()
                        }
                        Predicate::ComparePredicate(_) => continue,
                    };
                    if head2rule_ids_map.contains_key(name) {
                        entry.insert(name);
                    }
                }
            }
            let reaches_self = |start: &str| -> bool {
                let mut stack: Vec<&str> = head_deps
                    .get(start)
                    .into_iter()
                    .flatten()
                    .copied()
                    .collect();
                let mut seen: HashSet<&str> = stack.iter().copied().collect();
                while let Some(h) = stack.pop() {
                    if h == start {
                        return true;
                    }
                    for next in head_deps.get(h).into_iter().flatten() {
                        if seen.insert(next) {
                            stack.push(next);
                        }
                    }
                }
                false
            };
            for head in agg_heads {
                if !reaches_self(head) {
                    continue;
                }
                if let Some(ids) = head2rule_ids_map.get(head) {
                    for &a in ids {
                        for &b in ids {
                            if a != b {
                                rule_dependency_map.get_mut(&a).unwrap().insert(b);
                            }
                        }
                    }
                }
            }
        }

        Self {
            rule_idb_names,
            rule_dependency_map,
            negation_dependency_map,
        }
    }
}

impl fmt::Display for DependencyGraph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut dependency_graph_str = String::new();

        // formatting the Dependency Graph
        writeln!(
            dependency_graph_str,
            ".dependency graph (rule_id: dependent rule_ids): "
        )
        .unwrap();

        for (rule_id, dependent_rule_ids) in self.rule_dependency_map.iter().sorted_by_key(|x| x.0)
        {
            if !dependent_rule_ids.is_empty() {
                let dependent_rule_ids_str = dependent_rule_ids
                    .iter()
                    .sorted()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    dependency_graph_str,
                    "{}: [{}]",
                    rule_id, dependent_rule_ids_str
                )?; // : here is equivalent to (depends on)
            } else {
                writeln!(dependency_graph_str, "{}: ", rule_id).unwrap();
            }
        }

        // formatting the Negation Dependency Graph
        writeln!(
            dependency_graph_str,
            "\n.negation dependency graph (rule_id: dependent negation rule_ids): "
        )
        .unwrap();
        for (rule_id, dependent_rule_ids) in
            self.negation_dependency_map.iter().sorted_by_key(|x| x.0)
        {
            if !dependent_rule_ids.is_empty() {
                let dependent_rule_ids_str = dependent_rule_ids
                    .iter()
                    .sorted()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    dependency_graph_str,
                    "{}: [{}]",
                    rule_id, dependent_rule_ids_str
                )?;
            }
        }

        write!(f, "{}", dependency_graph_str)
    }
}
