use itertools::Itertools;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info};

extern crate differential_dataflow;
extern crate timely;

// local modules
use crate::arg::Args;
use crate::collector::inspector;
use crate::collector::non_recursive_collector;
use crate::collector::recursive_collector;
use crate::map::*;
use crate::transformer::*;
use crate::Iter;
use crate::Time;
use planning::collections::CollectionSignature;
use planning::flow::TransformationFlow;
use planning::strata::GroupStrataQueryPlan;
use planning::transformations::Transformation;
use strata::stratification::Strata;
use timely::dataflow::operators::probe::Handle as ProbeHandle;
use timely::dataflow::Scope;

use catalog::head::AggregationHeadIDB;
use macros::*;
use parsing::decl::DataType;
use reading::arrangements::SetTraceGeneric;
use reading::inspect::*;
use reading::reader::*;
use reading::rel::DoubleRel::*;
use reading::rel::Rel::*;
use reading::session::InputSessionGeneric;

use differential_dataflow::operators::arrange::ShutdownButton;
use timely::dataflow::operators::CapabilitySet;

/// Column types of an IDB relation by name (empty if unknown), used to decode
/// engine output (`string`/`float` columns) back to their textual form.
fn idb_types(program: &parsing::parser::Program, name: &str) -> Vec<DataType> {
    program
        .idbs()
        .iter()
        .find(|d| d.name() == name)
        .map(|d| d.attributes().iter().map(|a| *a.data_type()).collect())
        .unwrap_or_default()
}

/// Live trace handles for published relations, keyed by relation name. Filled
/// while assembling the base streaming dataflow; read when a late-added query
/// imports its base relations.
pub type TraceRegistry = HashMap<String, SetTraceGeneric<Time>>;

/// A query compiled on the control side (parse/plan/validate happen there, so
/// a bad program never reaches the workers), ready for every worker to
/// instantiate as its own dataflow over imported traces.
pub struct CompiledQuery {
    pub id: String,
    pub strata: Strata,
    pub plans: Vec<GroupStrataQueryPlan>,
    pub idb_map: HashMap<String, AggregationHeadIDB>,
    /// Must equal the base program's fat mode: imported traces carry the
    /// base's row representation.
    pub fat_mode: bool,
    /// Per-query output callback (relation names are the query's own IDBs).
    pub output_callback: Arc<dyn Fn(&str, smallvec::SmallVec<[i64; 8]>, isize) + Send + Sync>,
}

/// A runtime command for a live streaming engine.
#[derive(Clone)]
pub enum QueryCommand {
    Add(Arc<CompiledQuery>),
    Drop { id: String },
}

/// Append-only, totally-ordered command log. Every worker applies every entry
/// exactly once, in log order (each keeps a private cursor) — timely requires
/// all workers to construct the same dataflows in the same sequence, and a
/// shared ordered log is the simplest way to guarantee that.
#[derive(Clone, Default)]
pub struct CommandLog {
    entries: Arc<std::sync::RwLock<Vec<QueryCommand>>>,
}

impl CommandLog {
    pub fn push(&self, cmd: QueryCommand) {
        self.entries.write().unwrap().push(cmd);
    }

    /// Entries appended since `cursor` (clones; commands are cheap handles).
    fn after(&self, cursor: usize) -> Vec<QueryCommand> {
        self.entries.read().unwrap()[cursor..].to_vec()
    }
}

/// Where the assembled dataflow's base relations come from: fresh input
/// sessions (the base program) or imports of published traces (a late-added
/// query, which also collects each import's shutdown button for teardown).
enum EdbSource<'r> {
    Sessions,
    Imports {
        registry: &'r mut TraceRegistry,
        tokens: &'r mut Vec<ShutdownButton<CapabilitySet<Time>>>,
    },
}

/// Where the assembled dataflow's IDB outputs go.
///
/// Batch attaches count inspectors and optional CSV writers and lets the
/// dataflow run to fixpoint; streaming attaches per-tuple inspect callbacks
/// plus a shared probe so the epoch loop can tell when an epoch's output has
/// fully drained. The plan-walking core is identical either way — this enum is
/// the only seam between the two execution modes (and the seam where a future
/// mode, e.g. building over imported traces, would slot in).
enum OutputMode<'a> {
    Batch,
    Streaming {
        /// Invoked with (relation_name, raw i64 row, diff) for each output tuple.
        callback: Arc<dyn Fn(&str, smallvec::SmallVec<[i64; 8]>, isize) + Send + Sync>,
        /// Attached to every streaming output so the caller's loop can drive the
        /// worker until each epoch's output is fully produced.
        probe: &'a mut ProbeHandle<Time>,
    },
}

/// Assemble the complete dataflow for `group_plans` inside `scope`: EDB input
/// sessions, every stratum's transformation chain (recursive strata inside a
/// nested iterative scope), and the mode's output attachments. Returns the EDB
/// input sessions for the caller to feed. This is the single shared core of
/// batch and streaming execution — the two differ only via `mode`.
#[allow(clippy::too_many_arguments)]
fn assemble_dataflow<'scope>(
    scope: Scope<'scope, Time>,
    args: &Args,
    strata: &Strata,
    group_plans: &[GroupStrataQueryPlan],
    fat_mode: bool,
    idb_map: &HashMap<String, AggregationHeadIDB>,
    worker_id: usize,
    mode: &mut OutputMode<'_>,
    mut publish: Option<(&HashSet<String>, &mut TraceRegistry)>,
    mut edb_source: EdbSource<'_>,
) -> HashMap<String, InputSessionGeneric<Time>> {
    let mut session_map = HashMap::new(); // map from each edb name to input session (for data loading)
    let mut row_map = HashMap::new(); // map from row signature (edbs and idbs) to the physical dataflow data

    // Relations consumed by an aggregation rule's body must be true SETS:
    // `expand_values` turns reduce-input multiplicities back into body matches,
    // so a bag input (streaming mode never runs the thresholding `inspector`,
    // and sources may push duplicate rows) inflates every aggregate. Threshold
    // exactly the relations that feed an aggregation, at their producer.
    let agg_body_rels: HashSet<&str> = strata
        .program()
        .rules()
        .iter()
        .filter(|rule| {
            rule.head()
                .head_arguments()
                .iter()
                .any(|arg| matches!(arg, parsing::head::HeadArg::Aggregation(_)))
        })
        .flat_map(|rule| {
            rule.rhs().iter().filter_map(|pred| match pred {
                parsing::rule::Predicate::AtomPredicate(atom)
                | parsing::rule::Predicate::NegatedAtomPredicate(atom) => Some(atom.name()),
                parsing::rule::Predicate::ComparePredicate(_) => None,
            })
        })
        .collect();
    let mut kv_map = HashMap::new(); // map from (k, v) signature to the physical dataflow data
    let mut k_map = HashMap::new(); // map from (k, ) signature to the physical dataflow data

    /* construct dataflow rels: input sessions for the base program, or imports
     * of published traces for a late-added query */
    for edb in strata.program().edbs() {
        let edb_name = edb.name();
        let input_rel = match &mut edb_source {
            EdbSource::Sessions => {
                let (session_generic, input_rel) =
                    construct_session_and_table(scope, edb.arity(), fat_mode);
                session_map.insert(edb_name.to_string(), session_generic);
                input_rel
            }
            EdbSource::Imports { registry, tokens } => {
                let trace = registry.get_mut(edb_name).unwrap_or_else(|| {
                    panic!("query base relation '{}' is not published", edb_name)
                });
                assert_eq!(
                    trace.arity(),
                    edb.arity(),
                    "published '{}' arity differs from the query's decl",
                    edb_name
                );
                if std::env::var("DEP2_DEBUG_IMPORT").is_ok() {
                    let (since, upper) = trace.frontiers();
                    eprintln!(
                        "[import w{}] {} since={:?} upper={:?}",
                        worker_id, edb_name, since, upper
                    );
                }
                let (input_rel, button) = trace.import_core(scope, edb_name);
                tokens.push(button);
                input_rel
            }
        };

        // Publish the raw EDB itself when asked (query programs can then use
        // base inputs directly, not only derived IDBs).
        if let Some((publish_set, registry)) = publish.as_mut() {
            if publish_set.contains(edb_name) {
                registry.insert(
                    edb_name.to_string(),
                    input_rel.arrange_set().trace_generic(),
                );
            }
        }

        let input_rel = if agg_body_rels.contains(edb_name) {
            input_rel.threshold()
        } else {
            input_rel
        };
        row_map.insert(
            Arc::new(CollectionSignature::new_atom(edb_name)),
            Arc::new(input_rel),
        );
    }

    /* inspect edbs (optional) */
    if tracing::level_enabled!(tracing::Level::DEBUG) {
        for (signature, rel) in row_map
            .iter()
            .sorted_by_key(|(signature, _)| signature.name())
        {
            printsize_generic(rel, &format!("[{}]", signature.name()), false);
        }
    }

    for (group_plan_idx, group_plan) in group_plans.iter().enumerate() {
        let is_last_group_plan = group_plan_idx == group_plans.len() - 1; // last group plan is the final strata (must print size)

        if !group_plan.is_recursive() {
            /* construct dataflow for a non-recursive strata */
            for next_transformation in group_plan.strata_plan() {
                let output = next_transformation.output();
                let output_signature = output.signature();
                let (ok, ov) = output.arity();
                let target = ok + ov;

                // Arrangement sharing: signature names are content-canonical
                // (input chain + positional flow + filters — no rule-local
                // identity), so an output already present in its destination
                // map IS this collection/arrangement, built by an earlier rule
                // or stratum. Reuse it instead of building a duplicate
                // operator + arrangement (borrow-class programs rebuild the
                // same join leaf up to 5x otherwise). Deterministic across
                // workers: every worker walks the same plan order.
                let already_built = match (ok, ov) {
                    (0, _) => row_map.contains_key(output_signature),
                    (_, 0) => k_map.contains_key(output_signature),
                    _ => kv_map.contains_key(output_signature),
                };
                if already_built {
                    continue;
                }

                if next_transformation.is_unary() {
                    let unary = next_transformation.unary();
                    let (ik, iv) = unary.arity();
                    let input_rel = row_map.get(unary.signature()).unwrap_or_else(|| {
                        panic!("row absent for unary op: {}", unary.signature())
                    });

                    match next_transformation {
                        Transformation::RowToRow { flow, is_no_op, .. } => {
                            // (1) single op, tc(x, y) :- arc(y, x).
                            assert!(ik == 0 && ok == 0);
                            let output_rel = if *is_no_op {
                                Arc::clone(input_rel)
                            } else if let TransformationFlow::HeadArith { projections } = flow {
                                Arc::new(codegen_row_row_head_arith!())
                            } else {
                                Arc::new(codegen_row_row!())
                            };
                            row_map.insert(Arc::clone(output_signature), output_rel);
                        }
                        Transformation::RowToK { flow, is_no_op, .. } => {
                            // (2) leaf op for semijn or aj
                            assert!(ik == 0 && ov == 0);
                            let output_rel = if *is_no_op {
                                Arc::clone(input_rel)
                            } else {
                                Arc::new(codegen_row_row!())
                            };
                            k_map.insert(
                                Arc::clone(output_signature),
                                (Arc::clone(&output_rel), Arc::new(output_rel.arrange_set())),
                            );
                        }
                        Transformation::RowToKv { flow, .. } => {
                            // (3) leaf op for jn
                            assert_eq!(ik, 0);
                            let output_kv = Arc::new(codegen_row_kv!());
                            kv_map.insert(
                                Arc::clone(output_signature),
                                (Arc::clone(&output_kv), Arc::new(output_kv.arrange_dict())),
                            );
                        }
                        _ => panic!("abnormal unary transformation"),
                    }
                } else {
                    let binary = next_transformation.binary();
                    let (ik0, mut iv0) = binary.0.arity();
                    let (ik1, mut iv1) = binary.1.arity();
                    assert_eq!(ik0, ik1);

                    let (large, small, flow) = if iv0 < iv1 {
                        std::mem::swap(&mut iv0, &mut iv1);
                        (
                            binary.1.signature(),
                            binary.0.signature(),
                            &next_transformation.flow().jn_flip(),
                        )
                    } else {
                        (
                            binary.0.signature(),
                            binary.1.signature(),
                            next_transformation.flow(),
                        )
                    };

                    let output_rel = match next_transformation {
                        Transformation::JnKvKv { .. } => {
                            kv_jn_kv(large, small, &kv_map, ik0, iv0, iv1, target, flow)
                        }
                        Transformation::JnKvK { .. } | Transformation::JnKKv { .. } => {
                            kv_jn_k(large, small, &kv_map, &k_map, ik0, iv0, iv1, target, flow)
                        }
                        Transformation::JnKK { .. } => {
                            k_jn_k(large, small, &k_map, ik0, iv0, iv1, target, flow)
                        }
                        Transformation::Cartesian { .. } => {
                            cartesian(large, small, &row_map, iv0, iv1, target, flow)
                        }
                        Transformation::NjKvK { .. } => kv_aj_k(
                            large, small, &kv_map, &mut k_map, ik0, iv0, iv1, target, flow,
                        ),
                        Transformation::NjKK { .. } => {
                            k_aj_k(large, small, &mut k_map, ik0, iv0, iv1, target, flow)
                        }
                        _ => panic!("abnormal binary transformation"),
                    };

                    match (ok, ov) {
                        (0, _) => {
                            // jn → row
                            row_map.insert(Arc::clone(output_signature), Arc::clone(&output_rel));
                        }
                        (_, 0) => {
                            // jn → k
                            k_map.insert(
                                Arc::clone(output_signature),
                                (Arc::clone(&output_rel), Arc::new(output_rel.arrange_set())),
                            );
                        }
                        _ => {
                            // jn → kv
                            let output_kv = Arc::new(output_rel.arrange_double(ok));
                            kv_map.insert(
                                Arc::clone(output_signature),
                                (Arc::clone(&output_kv), Arc::new(output_kv.arrange_dict())),
                            );
                        }
                    }
                }
            }

            /* concat idbs of the non-recursive strata into row_map */
            non_recursive_collector(
                group_plan.last_signatures_map(),
                &mut row_map,
                &idb_map,
                &agg_body_rels,
            );

            /* per-mode outputs of the non-recursive strata */
            match mode {
                OutputMode::Batch => {
                    /* inspect idbs of the non-recursive strata (optional) */
                    if tracing::level_enabled!(tracing::Level::DEBUG) || is_last_group_plan {
                        inspector(&group_plan.head_signatures_set(), &mut row_map, false);
                    }

                    /* write non-recursive IDB CSVs (mirrors the recursive case) */
                    if let Some(csv_path) = args.csvs() {
                        for head_sig in group_plan.head_signatures_set().iter() {
                            let rel_name = head_sig.name();
                            if strata
                                .program()
                                .idbs()
                                .iter()
                                .any(|idb| idb.name() == rel_name)
                            {
                                if let Some(rel) = row_map.get(head_sig) {
                                    writesize_generic(
                                        rel,
                                        rel_name,
                                        &format!("{}/csvs/size.txt", csv_path),
                                    );
                                    let full_path = format!("{}/csvs/{}.csv", csv_path, rel_name);
                                    write_generic(
                                        rel,
                                        &full_path,
                                        worker_id,
                                        &idb_types(strata.program(), rel_name),
                                    );
                                }
                            }
                        }
                    }
                }
                OutputMode::Streaming { callback, probe } => {
                    // Attach inspect callbacks INSTEAD of inspector(): inspector()
                    // applies threshold() which blocks output until the frontier
                    // advances (incompatible with streaming). Sorted like every
                    // operator-constructing loop: workers must build in one order.
                    for head_sig in group_plan
                        .head_signatures_set()
                        .iter()
                        .sorted_by_key(|sig| sig.name())
                    {
                        let rel_name = head_sig.name().to_string();
                        if strata
                            .program()
                            .idbs()
                            .iter()
                            .any(|idb| idb.name() == rel_name)
                        {
                            if let Some(rel) = row_map.get(head_sig) {
                                let cb = Arc::clone(callback);
                                let name = rel_name.clone();
                                inspect_streaming_generic(rel, move |row, diff| {
                                    cb(&name, row, diff);
                                });
                                probe_streaming_generic(rel, probe);
                            }
                        }
                    }
                }
            }

            /* register published IDB traces for late-added queries.
             * SORTED: registration builds an arrangement (with exchange
             * channels), and every worker must construct operators in the
             * same order or timely's channel identities misalign — HashSet
             * iteration order differs per worker. */
            if let Some((publish_set, registry)) = publish.as_mut() {
                for head_sig in group_plan
                    .head_signatures_set()
                    .iter()
                    .sorted_by_key(|sig| sig.name())
                {
                    let rel_name = head_sig.name();
                    if publish_set.contains(rel_name) {
                        if let Some(rel) = row_map.get(head_sig) {
                            registry
                                .insert(rel_name.to_string(), rel.arrange_set().trace_generic());
                        }
                    }
                }
            }
        } else {
            let outer_scope = scope;
            let recursive_out_map = scope.iterative::<Iter, _, _>(|scope| {
                /* (1) construct iterative variables for strata idbs */
                let head_signatures_set = group_plan.head_signatures_set().clone();
                let mut variables_map = HashMap::with_capacity(head_signatures_set.len());
                let mut variables_next_map = HashMap::with_capacity(head_signatures_set.len());

                for (head_name, head_arity) in group_plan.heads().iter().sorted_by_key(|x| x.0) {
                    // (sideways) jump over sip rules
                    // We do not collect sip rules in the collector, we store them in the next row map
                    // TODO: temporarily way to avoid sip rule, need carefully refactor
                    // to avoid this in the future
                    if head_name.contains("_sip") {
                        continue;
                    }

                    variables_map.insert(
                        Arc::new(CollectionSignature::new_atom(head_name)),
                        construct_var(scope, *head_arity, fat_mode),
                    );
                }

                let mut nest_row_map = HashMap::new();
                let mut nest_kv_map = HashMap::new();
                let mut nest_k_map = HashMap::new();

                // Signatures seeded from the OUTER scope: their nested copies
                // hold pre-recursion content, and the plan may deliberately
                // rebuild them in-scope over live variables — those rebuilds
                // must not be shared away (see the sharing guard below).
                let mut entered_sigs: HashSet<Arc<CollectionSignature>> = HashSet::new();

                let dependent_signatures = group_plan.enter_scope_set();
                for dependent_signature in
                    dependent_signatures.iter().sorted_by_key(|sig| sig.name())
                {
                    // (sideways) jump over sip rules
                    // We do not collect sip rules in the collector, we store them in the next row map
                    // TODO: temporarily way to avoid sip rule, need carefully refactor
                    // to avoid this in the future
                    if dependent_signature.name().contains("_sip") {
                        continue;
                    }

                    if let Some(dependent_rel) = row_map.get(dependent_signature) {
                        // rel has been created prior to the strata
                        if head_signatures_set.contains(dependent_signature) {
                            // (1) rel from prior strata will be part of the eventual idb
                            variables_next_map.insert(
                                Arc::clone(dependent_signature),
                                Arc::new(dependent_rel.enter(scope)),
                            );
                        } else {
                            // (2) rel from prior strata purely for joins
                            nest_row_map.insert(
                                Arc::clone(dependent_signature),
                                Arc::new(dependent_rel.enter(scope)),
                            );
                            entered_sigs.insert(Arc::clone(dependent_signature));
                        }
                    } else if let Some((dependent_kv, _)) = kv_map.get(dependent_signature) {
                        // (3) dict from prior strata purely for joins
                        let nested_kv = Arc::new(dependent_kv.enter(scope));
                        let nested_dict = Arc::new(nested_kv.arrange_dict());
                        nest_kv_map.insert(
                            Arc::clone(dependent_signature),
                            (nested_kv, nested_dict),
                        );
                        entered_sigs.insert(Arc::clone(dependent_signature));
                    } else if let Some((dependent_k, _)) = k_map.get(dependent_signature) {
                        // (4) set from prior strata purely for joins
                        let nested_k = Arc::new(dependent_k.enter(scope));
                        let nested_set = Arc::new(nested_k.arrange_set());
                        nest_k_map.insert(Arc::clone(dependent_signature), (nested_k, nested_set));
                        entered_sigs.insert(Arc::clone(dependent_signature));
                    } else {
                        // (5) rel defined from this recursive strata
                        assert!(
                            variables_map.contains_key(dependent_signature),
                            "dependent {:?} must be defined somewhere of the strata",
                            dependent_signature
                        );
                    }
                }

                // mostly identical to the non-recursive case
                for next_transformation in group_plan.strata_plan() {
                    let output = next_transformation.output();
                    let output_signature = output.signature();
                    let (ok, ov) = output.arity();
                    let target = ok + ov;

                    // Arrangement sharing, scope-local (see the non-recursive
                    // twin). Entered-from-outer signatures are exempt: the
                    // plan may rebuild them over live in-scope variables, and
                    // that rebuild must replace the pre-recursion snapshot
                    // exactly as it does today.
                    if !entered_sigs.contains(output_signature) {
                        let already_built = match (ok, ov) {
                            (0, _) => nest_row_map.contains_key(output_signature),
                            (_, 0) => nest_k_map.contains_key(output_signature),
                            _ => nest_kv_map.contains_key(output_signature),
                        };
                        if already_built {
                            continue;
                        }
                    }

                    if next_transformation.is_unary() {
                        let unary = next_transformation.unary();
                        let (ik, iv) = unary.arity();
                        let unary_signature = unary.signature();

                        // input must be in the nest_row_map or variables_map
                        let input_rel = nest_row_map
                            .get(unary_signature)
                            .map(Arc::as_ref)
                            .or_else(|| variables_map.get(unary_signature))
                            .unwrap_or_else(|| {
                                panic!("row absent for unary op: {}", unary_signature)
                            });

                        match next_transformation {
                            Transformation::RowToRow { flow, is_no_op, .. } => {
                                // (1) single op, tc(x, y) :- arc(y, x).
                                assert!(ik == 0 && ok == 0);
                                let output_rel =
                                    if *is_no_op && nest_row_map.contains_key(unary_signature) {
                                        Arc::clone(nest_row_map.get(unary_signature).unwrap())
                                    } else if let TransformationFlow::HeadArith { projections } =
                                        flow
                                    {
                                        Arc::new(codegen_row_row_head_arith!())
                                    } else {
                                        Arc::new(codegen_row_row!())
                                    };
                                nest_row_map.insert(Arc::clone(output_signature), output_rel);
                            }
                            Transformation::RowToK { flow, is_no_op, .. } => {
                                // (2) leaf op for semijn or aj
                                assert!(ik == 0 && ov == 0);
                                let output_rel =
                                    if *is_no_op && nest_row_map.contains_key(unary_signature) {
                                        Arc::clone(nest_row_map.get(unary_signature).unwrap())
                                    } else {
                                        Arc::new(codegen_row_row!().threshold())
                                    };
                                nest_k_map.insert(
                                    Arc::clone(output_signature),
                                    (
                                        Arc::clone(&output_rel),
                                        Arc::new(output_rel.arrange_set()),
                                    ),
                                );
                            }
                            Transformation::RowToKv { flow, .. } => {
                                // (3) leaf op for jn
                                assert_eq!(ik, 0);
                                let output_kv = Arc::new(codegen_row_kv!());
                                nest_kv_map.insert(
                                    Arc::clone(output_signature),
                                    (
                                        Arc::clone(&output_kv),
                                        Arc::new(output_kv.arrange_dict()),
                                    ),
                                );
                            }
                            _ => panic!("(recursive) abnormal unary transformation"),
                        }
                    } else {
                        let binary = next_transformation.binary();
                        let (ik0, mut iv0) = binary.0.arity();
                        let (ik1, mut iv1) = binary.1.arity();
                        assert_eq!(ik0, ik1);

                        let (large, small, flow) = if iv0 < iv1 {
                            std::mem::swap(&mut iv0, &mut iv1);
                            (
                                binary.1.signature(),
                                binary.0.signature(),
                                &next_transformation.flow().jn_flip(),
                            )
                        } else {
                            (
                                binary.0.signature(),
                                binary.1.signature(),
                                next_transformation.flow(),
                            )
                        };

                        let output_rel = match next_transformation {
                            Transformation::JnKvKv { .. } => {
                                kv_jn_kv(large, small, &nest_kv_map, ik0, iv0, iv1, target, flow)
                            }
                            Transformation::JnKvK { .. } | Transformation::JnKKv { .. } => kv_jn_k(
                                large,
                                small,
                                &nest_kv_map,
                                &nest_k_map,
                                ik0,
                                iv0,
                                iv1,
                                target,
                                flow,
                            ),
                            Transformation::JnKK { .. } => {
                                k_jn_k(large, small, &nest_k_map, ik0, iv0, iv1, target, flow)
                            }
                            Transformation::Cartesian { .. } => {
                                cartesian(large, small, &nest_row_map, iv0, iv1, target, flow)
                            }
                            Transformation::NjKvK { .. } => kv_aj_k(
                                large,
                                small,
                                &nest_kv_map,
                                &mut nest_k_map,
                                ik0,
                                iv0,
                                iv1,
                                target,
                                flow,
                            ),
                            Transformation::NjKK { .. } => {
                                k_aj_k(large, small, &mut nest_k_map, ik0, iv0, iv1, target, flow)
                            }
                            _ => panic!("(recursive) abnormal binary transformation"),
                        };

                        match (ok, ov) {
                            (0, _) => {
                                // jn → row
                                nest_row_map
                                    .insert(Arc::clone(output_signature), Arc::clone(&output_rel));
                                // (sideways) compensate sip rules
                                // We do not collect sip rules in the collector, so we need to store them in the next row map
                                // NOTE: intermediate join outputs in multi-way join trees won't
                                // be in reverse_last_signatures_map — only the final output is.
                                // This is expected and safe to skip.
                                if let Some(head_signatures) = group_plan
                                    .reverse_last_signatures_map()
                                    .get(output_signature)
                                {
                                    for head_signature in head_signatures {
                                        if head_signature.name().contains("_sip") {
                                            nest_row_map.insert(
                                                Arc::clone(head_signature),
                                                Arc::clone(&output_rel),
                                            );
                                        }
                                    }
                                }
                            }
                            (_, 0) => {
                                // jn → k
                                nest_k_map.insert(
                                    Arc::clone(output_signature),
                                    (
                                        Arc::clone(&output_rel),
                                        Arc::new(output_rel.arrange_set()),
                                    ),
                                );
                            }
                            _ => {
                                // jn → kv
                                let output_kv = Arc::new(output_rel.arrange_double(ok));
                                nest_kv_map.insert(
                                    Arc::clone(output_signature),
                                    (
                                        Arc::clone(&output_kv),
                                        Arc::new(output_kv.arrange_dict()),
                                    ),
                                );
                            }
                        }
                    }
                }

                /* concatenate and threshold idbs of the recursive strata into the variables_next_map */
                recursive_collector(
                    group_plan.last_signatures_map(),
                    &nest_row_map,
                    &mut variables_next_map,
                    &idb_map,
                );

                /* inspect idbs of the recursive strata (optional) */
                if tracing::level_enabled!(tracing::Level::DEBUG) {
                    inspector(&head_signatures_set, &mut variables_next_map, true);
                }

                /* set variables and leave scope */
                // Feedback is iteration-BOUNDED: a divergent fixpoint (e.g. a
                // recursive min whose value grows around a cycle — a documented
                // desugar limitation) completes at the cap with an error logged
                // instead of wedging the worker forever. Healthy fixpoints run
                // orders of magnitude below the bound.
                let max_iter: Iter = std::env::var("DEP2_MAX_ITER")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(100_000);
                let mut variables_leave_map = HashMap::with_capacity(head_signatures_set.len());
                for head_signature in head_signatures_set.iter().sorted_by_key(|sig| sig.name()) {
                    let variable_next = variables_next_map
                        .remove(&Arc::clone(head_signature))
                        .unwrap_or_else(|| {
                            panic!("head missing when leave: {}", head_signature.name())
                        });

                    if let Some(variable) = variables_map.remove(&Arc::clone(head_signature)) {
                        let bounded =
                            variable_next.bound_iterations(max_iter, head_signature.name());
                        variable.set(&bounded); // took ownership of the variable
                    } else {
                        panic!("head missing when set: {}", head_signature.name());
                    }

                    variables_leave_map.insert(
                        Arc::clone(head_signature),
                        variable_next.leave(outer_scope),
                    );
                }

                /* exports */
                variables_leave_map
            });

            // final contribution of the recursive strata
            for (recursive_signature, recursive_rel) in recursive_out_map
                .into_iter()
                .sorted_by_key(|(sig, _)| sig.name().to_owned())
            {
                let rel_name = recursive_signature.name();

                // only output if rel is IDBs
                if strata
                    .program()
                    .idbs()
                    .iter()
                    .any(|idb| idb.name() == rel_name)
                {
                    // printsize the relation
                    printsize_generic(&recursive_rel, &format!("[{}]", rel_name), true);
                    if let Some(csv_path) = args.csvs() {
                        // write IDB to csv
                        writesize_generic(
                            &recursive_rel,
                            rel_name,
                            &format!("{}/csvs/size.txt", csv_path),
                        );
                        let full_path = format!("{}/csvs/{}.csv", csv_path, rel_name);
                        write_generic(
                            &recursive_rel,
                            &full_path,
                            worker_id,
                            &idb_types(strata.program(), rel_name),
                        );
                    }

                    // Streaming output inspect for recursive IDBs
                    if let OutputMode::Streaming { callback, probe } = mode {
                        let cb = Arc::clone(callback);
                        let name = rel_name.to_string();
                        inspect_streaming_generic(&recursive_rel, move |row, diff| {
                            cb(&name, row, diff);
                        });
                        probe_streaming_generic(&recursive_rel, probe);
                    }
                }

                /* register published recursive IDB traces for late-added queries */
                if let Some((publish_set, registry)) = publish.as_mut() {
                    if publish_set.contains(rel_name) {
                        registry.insert(
                            rel_name.to_string(),
                            recursive_rel.arrange_set().trace_generic(),
                        );
                    }
                }

                // if the rel is in the row_map, it will be overwritten
                row_map.insert(recursive_signature, Arc::new(recursive_rel));
            }
        }
    } // end of a strata (group plan)

    /* Any published relation nothing derives still needs a trace: a declared
     * IDB with no rules is a legal (empty) relation, and a late query
     * importing it must see empty — not panic the worker. The session is
     * closed immediately, sealing an empty trace whose frontier drains, so
     * imports of it complete like any other. */
    if let Some((publish_set, registry)) = publish.as_mut() {
        for decl in strata.program().idbs() {
            if publish_set.contains(decl.name()) && !registry.contains_key(decl.name()) {
                let (session, empty_rel) =
                    construct_session_and_table(scope, decl.arity(), fat_mode);
                session.close();
                registry.insert(
                    decl.name().to_string(),
                    empty_rel.arrange_set().trace_generic(),
                );
            }
        }
    }

    /* exports */
    session_map
}

/// Load each EDB's staged `.facts` file into its input session (rows sharded
/// across workers). Shared by batch and streaming execution — streaming stages
/// (usually empty) files and feeds live rows through the sessions afterwards.
fn load_edb_facts(
    session_map: &mut HashMap<String, InputSessionGeneric<Time>>,
    strata: &Strata,
    args: &Args,
    worker_id: usize,
    peers: usize,
    fat_mode: bool,
) {
    for rel_decl in strata.program().edbs() {
        let rel_name = rel_decl.name();
        let rel_path = if let Some(path) = rel_decl.path() {
            format!("{}/{}", args.facts(), path)
        } else {
            format!("{}/{}.facts", args.facts(), rel_name)
        };

        let session_generic = session_map
            .get_mut(rel_name)
            .unwrap_or_else(|| panic!("entry from session_map: {}", rel_name));

        read_row_generic(
            rel_decl,
            &rel_path,
            &args.delimiter().as_bytes()[0],
            session_generic,
            worker_id,
            peers,
            fat_mode,
        );
    }
}

pub fn program_execution(
    args: Args,
    strata: Strata,
    group_plans: Vec<GroupStrataQueryPlan>,
    fat_mode: bool,
    idb_map: HashMap<String, AggregationHeadIDB>,
) {
    timely::execute_from_args(args.timely_args().into_iter(), move |worker| {
        let timer = ::std::time::Instant::now();
        let peers = worker.peers();
        let id = worker.index();

        /* assemble dataflow */
        let mut session_map = worker.dataflow::<Time, _, _>(|scope| {
            assemble_dataflow(
                scope,
                &args,
                &strata,
                &group_plans,
                fat_mode,
                &idb_map,
                id,
                &mut OutputMode::Batch,
                None,
                EdbSource::Sessions,
            )
        });

        if id == 0 {
            info!("{:?}:\tDataflow assembled", timer.elapsed());
        }

        /* feeding edb data */
        load_edb_facts(&mut session_map, &strata, &args, id, peers, fat_mode);

        for rel_decl in strata.program().edbs() {
            let rel_name = rel_decl.name();
            session_map
                .remove(rel_name)
                .unwrap_or_else(|| panic!("entry from session_map: {}", rel_name))
                .close();

            if id == 0 {
                info!("{:?}:\tData loaded for {}", timer.elapsed(), rel_name);
            }
        }

        /* executing the dataflow */
        while worker.step() {
            // spinning
        }

        if id == 0 {
            let time_elapsed = timer.elapsed(); // <--- end of clock excluding output
            info!("{:?}:\tDataflow executed", time_elapsed);

            if let Some(csv_path) = args.csvs() {
                for relation in strata.program().idbs() {
                    let full_path = format!("{}/csvs/{}.csv", csv_path, relation.name());
                    debug!("flusing {} to {}.csv", relation.name(), full_path); // actually merging flushed partitions
                    merge_relation_partitions(&full_path, peers);
                }
            }
        }
    })
    .expect("execute_from_args dies");
}

/// Configuration for streaming execution.
pub struct StreamingConfig {
    /// Pre-encoded input rows `(relation, encoded i64 row, diff)`, produced by the
    /// engine's parallel parse pool and drained here into the worker(s)' input
    /// sessions. Bounded, so a slow dataflow backpressures the parsers. With >1
    /// worker the channel is MPMC (each worker drains a share; differential
    /// exchanges downstream); with 1 worker it drains the whole stream locally.
    /// The relation is an `Arc<str>` and the row a `SmallVec` (inline up to the max
    /// non-fat arity) so the engine's hot path adds no per-row heap allocation.
    pub input: crossbeam_channel::Receiver<(Arc<str>, smallvec::SmallVec<[i64; 8]>, isize)>,
    /// Callback invoked with (relation_name, raw i64 row, diff) for each output
    /// tuple. The row is the engine's encoded form; decoding to display text is the
    /// consumer's job (done lazily, e.g. only when a relation is queried), so the
    /// output hot path does no stringify/decode per tuple.
    pub output_callback: Arc<dyn Fn(&str, smallvec::SmallVec<[i64; 8]>, isize) + Send + Sync>,
    /// Shutdown flag — when true, the streaming loop exits.
    pub shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// Monotonic counter bumped on every output tuple, so the streaming loop can
    /// detect quiescence (used by DEP2_BENCH to report ingestion time).
    pub output_seq: Arc<std::sync::atomic::AtomicU64>,
    /// Relations (EDBs and/or IDB heads) to arrange and register so queries
    /// added at runtime can import them. Publishing costs one extra whole-row
    /// arrangement per relation; leave empty when runtime queries are unused.
    pub publish: HashSet<String>,
    /// Runtime add/drop-query command log (see [`CommandLog`]).
    pub commands: CommandLog,
}

/// Streaming variant of `program_execution`.
///
/// Reuses the same dataflow assembly as batch mode (see [`assemble_dataflow`]).
/// The only differences:
/// - Streaming EDB sessions are kept open (not closed).
/// - After loading batch EDB facts, enters a continuous loop: receive from channels,
///   feed sessions, step the worker.
/// - Output IDB relations use inspect callbacks to report new tuples.
pub fn streaming_program_execution(
    args: Args,
    strata: Strata,
    group_plans: Vec<GroupStrataQueryPlan>,
    fat_mode: bool,
    idb_map: HashMap<String, AggregationHeadIDB>,
    streaming: StreamingConfig,
) {
    let streaming = Arc::new(streaming);
    // Cross-worker streaming coordination. All workers drain the shared input
    // channels in parallel (so ingestion scales), but epochs must be sealed in
    // lockstep: the global frontier is the min over workers, so a worker that
    // happened to receive no input in a window must still advance or it stalls
    // everyone (and output comes out incomplete). Worker 0 owns the seal decision;
    // a shared target epoch + dirty flag + last-input clock keep all workers
    // aligned. `base` is a single shared instant so the timings agree.
    use std::sync::atomic::AtomicU64;
    let shared_epoch = Arc::new(AtomicU64::new(1));
    // Wall time (ms since `base`) of the most recent input on ANY worker. Single
    // shared clock; worker 0 compares it against its own last-seal time to decide
    // when to advance, avoiding any raced flag.
    let last_input_ms = Arc::new(AtomicU64::new(0));
    let base = std::time::Instant::now();
    timely::execute_from_args(args.timely_args().into_iter(), move |worker| {
        let timer = ::std::time::Instant::now();
        let peers = worker.peers();
        let id = worker.index();

        /* assemble dataflow — identical to batch mode */
        // Probe attached to every streaming output, so the loop can drive the
        // worker until each epoch's output is fully produced (canonical timely).
        let mut probe = ProbeHandle::<Time>::new();
        // Published traces (for late-added queries) and the live queries'
        // shutdown buttons, both worker-local.
        let mut registry: TraceRegistry = HashMap::new();
        let mut live_queries: HashMap<String, Vec<ShutdownButton<CapabilitySet<Time>>>> =
            HashMap::new();
        let mut cmd_cursor: usize = 0;
        let mut session_map = {
            let mut mode = OutputMode::Streaming {
                callback: Arc::clone(&streaming.output_callback),
                probe: &mut probe,
            };
            worker.dataflow::<Time, _, _>(|scope| {
                assemble_dataflow(
                    scope,
                    &args,
                    &strata,
                    &group_plans,
                    fat_mode,
                    &idb_map,
                    id,
                    &mut mode,
                    Some((&streaming.publish, &mut registry)),
                    EdbSource::Sessions,
                )
            })
        };

        if id == 0 {
            info!("{:?}:\tDataflow assembled (streaming)", timer.elapsed());
        }

        /* feeding batch EDB data at epoch 0 */
        load_edb_facts(&mut session_map, &strata, &args, id, peers, fat_mode);

        // Advance all sessions to epoch 1, flush, and step.
        // This seals epoch 0 data in arrangements so joins can access it.
        let mut epoch = reading::Epoch(1);
        for (_rel_name, session) in session_map.iter_mut() {
            session.advance_to(epoch);
            session.flush();
        }
        worker.step();

        if id == 0 {
            info!("{:?}:\tBatch EDB data loaded at epoch 0", timer.elapsed());
        }

        /* streaming execution loop */
        if id == 0 {
            info!("{:?}:\tEntering streaming loop", timer.elapsed());
        }

        // Seal an epoch at a fixed cadence (every `epoch_period_ms`) as long as new
        // input has arrived since the last seal. This is what makes the engine
        // *incremental*: each sealed epoch flushes a batch of newly-parsed rows
        // through the dataflow and out to the query API, so a client (the web UI)
        // sees the graph fill in live during a long seed — the whole point of the
        // engine. The cadence is the tradeoff knob: coarser means fewer arrangement
        // batches (a little faster overall) but the results appear in fewer, larger
        // jumps; do NOT make it so coarse that a multi-minute seed produces no
        // output until it finishes. 64ms (~15 updates/sec) reads as smooth and
        // streaming while still coalescing each burst of per-file sends into one
        // epoch. Tunable via DEP2_EPOCH_MS.
        let epoch_period_ms: u64 = std::env::var("DEP2_EPOCH_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64);
        use std::sync::atomic::Ordering::Relaxed;
        let mut last_seal_ms: u64 = 0;

        // How many input rows to drain per loop iteration before stepping. Bounded
        // so the worker interleaves feeding with dataflow stepping (output streams)
        // rather than draining the whole queue before any compute.
        let drain_batch: usize = std::env::var("DEP2_DRAIN_BATCH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4096);
        // Benchmark quiescence: print once the seed has been fed and the dataflow
        // has gone idle (no work and input quiet), the authoritative "ingested in".
        let bench = std::env::var("DEP2_BENCH").is_ok();
        let mut announced = false;
        let mut idle_steps: u32 = 0;
        let mut last_output_seq: u64 = 0;
        let mut last_output_ms: u64 = 0;

        // Multi-worker stall diagnostics (DEP2_DEBUG_STALL=1): a per-second line
        // per worker with everything the pacing decision depends on.
        let debug_stall = std::env::var("DEP2_DEBUG_STALL").is_ok();
        let mut stall_drained: u64 = 0;
        let mut stall_sleeps: u64 = 0;
        let mut stall_steps: u64 = 0;
        let mut stall_last_report_ms: u64 = 0;

        loop {
            if streaming.shutdown.load(Relaxed) {
                break;
            }

            // Apply newly appended runtime commands. Every worker applies every
            // entry once, in log order, so all workers construct the same
            // dataflows in the same sequence (which timely requires). A new
            // query dataflow imports its base relations from the published
            // traces: it replays their history, then follows live updates —
            // its outputs land on the shared probe, so the epoch loop below
            // drives the replay to completion like any other epoch's work.
            for cmd in streaming.commands.after(cmd_cursor) {
                cmd_cursor += 1;
                match cmd {
                    QueryCommand::Add(q) => {
                        if std::env::var("DEP2_DEBUG_IMPORT").is_ok() {
                            eprintln!("[cmd w{}] add '{}' at epoch {}", id, q.id, epoch.0);
                        }
                        assert_eq!(
                            q.fat_mode, fat_mode,
                            "query fat mode must match the base program"
                        );
                        let mut tokens = Vec::new();
                        worker.dataflow::<Time, _, _>(|scope| {
                            let mut mode = OutputMode::Streaming {
                                callback: Arc::clone(&q.output_callback),
                                probe: &mut probe,
                            };
                            assemble_dataflow(
                                scope,
                                &args,
                                &q.strata,
                                &q.plans,
                                fat_mode,
                                &q.idb_map,
                                id,
                                &mut mode,
                                None,
                                EdbSource::Imports {
                                    registry: &mut registry,
                                    tokens: &mut tokens,
                                },
                            );
                        });
                        // Replacing an id must tear down its predecessor:
                        // dropping the old buttons unpressed would leave the
                        // old dataflow running forever, with both callbacks
                        // writing. Control layers guard duplicate ids, but the
                        // engine contract must not leak on one either.
                        if let Some(mut old) = live_queries.insert(q.id.clone(), tokens) {
                            for token in old.iter_mut() {
                                token.press();
                            }
                        }
                    }
                    QueryCommand::Drop { id: query_id } => {
                        if let Some(mut tokens) = live_queries.remove(&query_id) {
                            for token in tokens.iter_mut() {
                                token.press();
                            }
                        }
                    }
                }
            }

            // Catch this worker up to the shared target epoch before feeding, so we
            // never feed at a time the global frontier has already passed. Every
            // worker follows the same shared epoch, so the global frontier (the min
            // over workers) always advances in lockstep — no worker stalls it.
            let target = shared_epoch.load(Relaxed);
            if epoch.0 < target {
                epoch.0 = target;
                for (_rel_name, session) in session_map.iter_mut() {
                    session.advance_to(epoch);
                    session.flush();
                }
                // Let published traces consolidate history older than the
                // previous epoch. Without this, every registry handle pins the
                // full update history forever and trace memory grows without
                // bound; with it, a query added later imports merged state at
                // the compaction frontier plus subsequent updates — same
                // contents (the property tests pin this), bounded memory. One
                // epoch of slack keeps the frontier strictly behind the seal.
                let compact_to = [reading::Epoch(target.saturating_sub(1))];
                for trace in registry.values_mut() {
                    trace.set_logical_compaction(&compact_to);
                    trace.set_physical_compaction(&compact_to);
                }
            }

            // Drain a bounded chunk of pre-encoded rows from the parse pool into
            // this worker's input sessions. The channel is MPMC, so with >1 worker
            // each takes a share and differential exchanges downstream.
            let mut had_updates = false;
            for _ in 0..drain_batch {
                match streaming.input.try_recv() {
                    Ok((rel, row, diff)) => {
                        if let Some(session) = session_map.get_mut(&*rel) {
                            update_session_generic(
                                session,
                                &row,
                                fat_mode,
                                diff as reading::Semiring,
                            );
                            had_updates = true;
                            stall_drained += 1;
                        }
                    }
                    Err(_) => break, // empty (or disconnected); stop draining this round
                }
            }
            if had_updates {
                last_input_ms.store(base.elapsed().as_millis() as u64, Relaxed);
            }

            // Worker 0 alone advances the shared epoch (all workers follow it) on a
            // fixed cadence, but only when input has arrived since the last seal (so
            // a quiescent daemon doesn't churn empty epochs). Deterministic.
            if id == 0 {
                let now_ms = base.elapsed().as_millis() as u64;
                if now_ms.saturating_sub(last_seal_ms) >= epoch_period_ms
                    && last_input_ms.load(Relaxed) >= last_seal_ms
                {
                    shared_epoch.fetch_add(1, Relaxed);
                    last_seal_ms = now_ms;
                }
            }

            // Drive the dataflow until this epoch's output is fully produced — the
            // canonical timely pattern: step the worker while the output probe is
            // behind the input frontier. This is what makes every rule stream its
            // output per epoch, including recursive/negated rules under MULTIPLE
            // workers (e.g. import_graph's file_node, via recursive file_anc_dir +
            // `!has_module`), whose fixpoint needs several exchange iterations to
            // converge each epoch. Draining each epoch before feeding the next also
            // bounds how much data piles into one epoch, so output never freezes
            // mid-seed. When quiescent the probe is already caught up and this
            // returns immediately, so we then sleep.
            {
                let debug_stuck = std::env::var("DEP2_DEBUG_STUCK").is_ok();
                let mut steps: u64 = 0;
                let started = std::time::Instant::now();
                let mut last_report = started;
                let mut warned = false;
                while probe.less_than(&epoch) {
                    // A divergent fixpoint (e.g. a recursive aggregation over a
                    // cycle with growing values — a documented limitation) must
                    // not make shutdown hang forever.
                    if streaming.shutdown.load(Relaxed) {
                        break;
                    }
                    // step_or_park, not a bare step: when this worker is only
                    // waiting on peers' exchanged data, a bare step() spins
                    // millions of empty iterations per second — a yield-storm
                    // the OS scheduler punishes with priority decay, which can
                    // wedge a whole run into a slow mode. Parking (bounded,
                    // woken early by incoming channel events) removes the spin.
                    worker.step_or_park(Some(Duration::from_millis(1)));
                    steps += 1;
                    stall_steps += 1;
                    if !warned && steps % 1024 == 0 && started.elapsed() > Duration::from_secs(10) {
                        warned = true;
                        tracing::error!(
                            "worker {}: epoch {} has not completed after {} steps / {:?} — \
                             the program may be divergent (e.g. recursive min/max whose \
                             value keeps growing around a cycle)",
                            id,
                            epoch.0,
                            steps,
                            started.elapsed()
                        );
                    }
                    if debug_stuck && last_report.elapsed() > Duration::from_secs(2) {
                        last_report = std::time::Instant::now();
                        probe.with_frontier(|f| {
                            eprintln!(
                                "[stuck w{}] epoch={} steps={} frontier={:?}",
                                id,
                                epoch.0,
                                steps,
                                f.to_vec()
                            );
                        });
                    }
                }
            }

            if bench && id == 0 && !announced {
                let now_ms = base.elapsed().as_millis() as u64;
                let seq = streaming.output_seq.load(Relaxed);
                if seq != last_output_seq {
                    last_output_seq = seq;
                    last_output_ms = now_ms;
                }
                let li = last_input_ms.load(Relaxed);
                // Quiescent once we've seen input and both input and output have been
                // silent for a window (the dataflow has caught up to the seed).
                let quiet = li > 0
                    && now_ms.saturating_sub(li) >= 400
                    && now_ms.saturating_sub(last_output_ms) >= 400;
                if quiet {
                    idle_steps += 1;
                } else {
                    idle_steps = 0;
                }
                if idle_steps >= 25 {
                    announced = true;
                    eprintln!("[bench] ingested in {:.2}s", base.elapsed().as_secs_f64());
                }
            }

            // When no input arrived this round, sleep briefly so a quiescent daemon
            // stays near 0% CPU (timely can't park on a channel it doesn't track).
            if !had_updates {
                stall_sleeps += 1;
                std::thread::sleep(Duration::from_millis(2));
            }

            if debug_stall {
                let now_ms = base.elapsed().as_millis() as u64;
                if now_ms.saturating_sub(stall_last_report_ms) >= 1000 {
                    stall_last_report_ms = now_ms;
                    let frontier = probe.with_frontier(|f| f.to_vec());
                    eprintln!(
                        "[stall w{id}] t={}s epoch={} target={} qlen={} drained={} \
                         sleeps={} steps={} probe={:?} last_input={} last_seal={}",
                        now_ms / 1000,
                        epoch.0,
                        shared_epoch.load(Relaxed),
                        streaming.input.len(),
                        stall_drained,
                        stall_sleeps,
                        stall_steps,
                        frontier,
                        last_input_ms.load(Relaxed),
                        last_seal_ms,
                    );
                }
            }
        }

        // Close all remaining sessions
        for (_rel_name, session) in session_map.drain() {
            session.close();
        }

        // Step to drain remaining work — bounded, because a divergent fixpoint
        // (see the watchdog above) would otherwise spin here forever and make
        // shutdown hang. Healthy dataflows drain in milliseconds.
        let drain_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while worker.step() {
            if std::time::Instant::now() > drain_deadline {
                tracing::warn!(
                    "worker {}: dataflow still busy 5s after shutdown; abandoning drain \
                     (the program may be divergent)",
                    id
                );
                break;
            }
        }

        if id == 0 {
            info!("{:?}:\tStreaming execution complete", timer.elapsed());
        }
    })
    .expect("execute_from_args dies (streaming)");
}
