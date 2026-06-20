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
use crate::dataflow::timely::dataflow::Scope;
use crate::map::*;
use crate::transformer::*;
use crate::Iter;
use crate::Time;
use planning::collections::CollectionSignature;
use planning::flow::TransformationFlow;
use planning::strata::GroupStrataQueryPlan;
use planning::transformations::Transformation;
use strata::stratification::Strata;

use catalog::head::AggregationHeadIDB;
use macros::*;
use parsing::decl::DataType;
use reading::inspect::*;
use reading::reader::*;
use reading::rel::DoubleRel::*;
use reading::rel::Rel::*;

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
            let mut session_map = HashMap::new();          // map from each edb name to input session (for data loading)
            let mut row_map = HashMap::new();                 // map from row signature (edbs and idbs) to the physical dataflow data
            let mut kv_map = HashMap::new();                  // map from (k, v) signature to the physical dataflow data
            let mut k_map = HashMap::new();                   // map from (k, ) signature to the physical dataflow data

            /* construct dataflow rels & input session (i.e., file handles) to load the input */
            for edb in strata.program().edbs() {
                let edb_name = edb.name();
                let (session_generic, input_rel) = construct_session_and_table(scope, edb.arity(), fat_mode);

                session_map.insert(
                    edb_name.to_string(), session_generic
                );

                row_map.insert(
                    Arc::new(CollectionSignature::new_atom(edb_name)), Arc::new(input_rel)
                );
            }

            /* inspect edbs (optional) */
            if tracing::level_enabled!(tracing::Level::DEBUG) {
                for (signature, rel) in row_map
                    .iter()
                    .sorted_by_key(|(signature, _)| signature.name()) {
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

                        if next_transformation.is_unary() {
                            let unary = next_transformation.unary();
                            let (ik, iv) = unary.arity();
                            let input_rel = row_map.get(unary.signature()).unwrap_or_else(|| panic!("row absent for unary op: {}", unary.signature()));

                            match next_transformation {
                                Transformation::RowToRow { flow, is_no_op, .. } => { // (1) single op, tc(x, y) :- arc(y, x).
                                    assert!(ik == 0 && ok == 0);
                                    let output_rel = if *is_no_op {
                                        Arc::clone(input_rel)
                                    } else if let TransformationFlow::HeadArith { projections } = flow {
                                        Arc::new(codegen_row_row_head_arith!())
                                    } else {
                                        Arc::new(codegen_row_row!())
                                    };
                                    row_map.insert(Arc::clone(output_signature), output_rel);
                                },

                                Transformation::RowToK { flow, is_no_op, .. } => { // (2) leaf op for semijn or aj
                                    assert!(ik == 0 && ov == 0);
                                    let output_rel = if *is_no_op { Arc::clone(input_rel) } else { Arc::new(codegen_row_row!()) };
                                    k_map.insert(
                                        Arc::clone(output_signature),
                                        (Arc::clone(&output_rel), Arc::new(output_rel.arrange_set()))
                                    );
                                },

                                Transformation::RowToKv { flow, .. } => { // (3) leaf op for jn
                                    assert_eq!(ik, 0);
                                    let output_kv = Arc::new(codegen_row_kv!());
                                    kv_map.insert(
                                        Arc::clone(output_signature),
                                        (Arc::clone(&output_kv), Arc::new(output_kv.arrange_dict()))
                                    );
                                },

                                _ => panic!("abnormal unary transformation"),
                            }
                        } else {
                            let binary = next_transformation.binary();
                            let (ik0, mut iv0) = binary.0.arity();
                            let (ik1, mut iv1) = binary.1.arity();
                            assert_eq!(ik0, ik1);

                            let (large, small, flow) = if iv0 < iv1 {
                                    std::mem::swap(&mut iv0, &mut iv1);
                                    (binary.1.signature(), binary.0.signature(), &next_transformation.flow().jn_flip())
                                } else {
                                    (binary.0.signature(), binary.1.signature(), next_transformation.flow())
                                };

                            let output_rel = match next_transformation {
                                    Transformation::JnKvKv { .. } =>
                                        kv_jn_kv(large, small, &kv_map, ik0, iv0, iv1, target, flow),

                                    Transformation::JnKvK { .. } | Transformation::JnKKv { .. } =>
                                        kv_jn_k(large, small, &kv_map, &k_map, ik0, iv0, iv1, target, flow),

                                    Transformation::JnKK { .. } =>
                                        k_jn_k(large, small, &k_map, ik0, iv0, iv1, target, flow),

                                    Transformation::Cartesian { .. } =>
                                        cartesian(large, small, &row_map, iv0, iv1, target, flow),

                                    Transformation::NjKvK { .. } =>
                                        kv_aj_k(large, small, &kv_map, &mut k_map, ik0, iv0, iv1, target, flow),

                                    Transformation::NjKK { .. } =>
                                        k_aj_k(large, small, &mut k_map, ik0, iv0, iv1, target, flow),

                                    _ => panic!("abnormal binary transformation"),
                                };

                            match (ok, ov) {
                                (0, _) => { // jn → row
                                    row_map.insert(Arc::clone(output_signature), Arc::clone(&output_rel));
                                },
                                (_, 0) => { // jn → k
                                    k_map.insert(
                                        Arc::clone(output_signature),
                                        (Arc::clone(&output_rel), Arc::new(output_rel.arrange_set()))
                                    );
                                }
                                _ => { // jn → kv
                                    let output_kv = Arc::new(output_rel.arrange_double(ok));
                                    kv_map.insert(
                                        Arc::clone(output_signature),
                                        (Arc::clone(&output_kv), Arc::new(output_kv.arrange_dict()))
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
                    );

                    /* inspect idbs of the non-recursive strata (optional) */
                    if tracing::level_enabled!(tracing::Level::DEBUG) || is_last_group_plan {
                        inspector(
                            &group_plan.head_signatures_set(),
                            &mut row_map,
                            false
                        );
                    }

                    /* write non-recursive IDB CSVs (mirrors the recursive case) */
                    if let Some(csv_path) = args.csvs() {
                        for head_sig in group_plan.head_signatures_set().iter() {
                            let rel_name = head_sig.name();
                            if strata.program().idbs().iter().any(|idb| idb.name() == rel_name) {
                                if let Some(rel) = row_map.get(head_sig) {
                                    writesize_generic(rel, rel_name, &format!("{}/csvs/size.txt", csv_path));
                                    let full_path = format!("{}/csvs/{}.csv", csv_path, rel_name);
                                    write_generic(rel, &full_path, id, &idb_types(strata.program(), rel_name));
                                }
                            }
                        }
                    }

                } else {
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
                                construct_var(scope, *head_arity, fat_mode)
                            );
                        }

                        let mut nest_row_map = HashMap::new();
                        let mut nest_kv_map = HashMap::new();
                        let mut nest_k_map = HashMap::new();

                        let dependent_signatures = group_plan.enter_scope_set();
                        for dependent_signature in dependent_signatures.iter().sorted_by_key(|sig| sig.name()) {
                            // (sideways) jump over sip rules
                            // We do not collect sip rules in the collector, we store them in the next row map
                            // TODO: temporarily way to avoid sip rule, need carefully refactor
                            // to avoid this in the future
                            if dependent_signature.name().contains("_sip") {
                                continue;
                            }

                            if let Some(dependent_rel) = row_map.get(dependent_signature) { // rel has been created prior to the strata
                                if head_signatures_set.contains(dependent_signature) {
                                    // (1) rel from prior strata will be part of the eventual idb
                                    variables_next_map.insert(
                                        Arc::clone(dependent_signature),
                                        Arc::new(dependent_rel.enter(scope))
                                    );
                                } else {
                                    // (2) rel from prior strata purely for joins
                                    nest_row_map.insert(
                                        Arc::clone(dependent_signature),
                                        Arc::new(dependent_rel.enter(scope))
                                    );
                                }
                            } else if let Some((dependent_kv, _)) = kv_map.get(dependent_signature) {
                                // (3) dict from prior strata purely for joins
                                let nested_kv = Arc::new(dependent_kv.enter(scope));
                                let nested_dict = Arc::new(nested_kv.arrange_dict());
                                nest_kv_map.insert(
                                    Arc::clone(dependent_signature),
                                    (nested_kv, nested_dict)
                                );
                            } else if let Some((dependent_k, _)) = k_map.get(dependent_signature) {
                                // (4) set from prior strata purely for joins
                                let nested_k = Arc::new(dependent_k.enter(scope));
                                let nested_set = Arc::new(nested_k.arrange_set());
                                nest_k_map.insert(
                                    Arc::clone(dependent_signature),
                                    (nested_k, nested_set)
                                );
                            } else {
                                // (5) rel defined from this recursive strata
                                assert!(
                                    variables_map.contains_key(dependent_signature),
                                    "dependent {:?} must be defined somewhere of the strata", dependent_signature
                                );
                            }
                        }

                        // mostly identical to the non-recursive case
                        for next_transformation in group_plan.strata_plan() {
                            let output = next_transformation.output();
                            let output_signature = output.signature();
                            let (ok, ov) = output.arity();
                            let target = ok + ov;

                            if next_transformation.is_unary() {
                                let unary = next_transformation.unary();
                                let (ik, iv) = unary.arity();
                                let unary_signature = unary.signature();

                                // input must be in the nest_row_map or variables_map
                                let input_rel = nest_row_map
                                    .get(unary_signature)
                                    .map(Arc::as_ref)
                                    .or_else(|| variables_map.get(unary_signature))
                                    .unwrap_or_else(|| panic!("row absent for unary op: {}", unary_signature));

                                match next_transformation {
                                    Transformation::RowToRow { flow, is_no_op, .. } => { // (1) single op, tc(x, y) :- arc(y, x).
                                        assert!(ik == 0 && ok == 0);
                                        let output_rel =
                                            if *is_no_op && nest_row_map.contains_key(unary_signature) {
                                                Arc::clone(nest_row_map.get(unary_signature).unwrap())
                                            } else if let TransformationFlow::HeadArith { projections } = flow {
                                                Arc::new(codegen_row_row_head_arith!())
                                            } else {
                                                Arc::new(codegen_row_row!())
                                            };
                                        nest_row_map.insert(Arc::clone(output_signature), output_rel);
                                    },

                                    Transformation::RowToK { flow, is_no_op, .. } => { // (2) leaf op for semijn or aj
                                        assert!(ik == 0 && ov == 0);
                                        let output_rel =
                                            if *is_no_op && nest_row_map.contains_key(unary_signature) {
                                                Arc::clone(nest_row_map.get(unary_signature).unwrap())
                                            } else {
                                                Arc::new(codegen_row_row!().threshold())
                                            };
                                        nest_k_map.insert(
                                            Arc::clone(output_signature),
                                            (Arc::clone(&output_rel), Arc::new(output_rel.arrange_set()))
                                        );
                                    },

                                    Transformation::RowToKv { flow, .. } => { // (3) leaf op for jn
                                        assert_eq!(ik, 0);
                                        let output_kv = Arc::new(codegen_row_kv!());
                                        nest_kv_map.insert(
                                            Arc::clone(output_signature),
                                            (Arc::clone(&output_kv), Arc::new(output_kv.arrange_dict()))
                                        );
                                    },

                                    _ => panic!("(recursive) abnormal unary transformation"),
                                }
                            } else {
                                let binary = next_transformation.binary();
                                let (ik0, mut iv0) = binary.0.arity();
                                let (ik1, mut iv1) = binary.1.arity();
                                assert_eq!(ik0, ik1);

                                let (large, small, flow) = if iv0 < iv1 {
                                    std::mem::swap(&mut iv0, &mut iv1);
                                    (binary.1.signature(), binary.0.signature(), &next_transformation.flow().jn_flip())
                                } else {
                                    (binary.0.signature(), binary.1.signature(), next_transformation.flow())
                                };

                                let output_rel = match next_transformation {
                                        Transformation::JnKvKv { .. } =>
                                            kv_jn_kv(large, small, &nest_kv_map, ik0, iv0, iv1, target, flow),

                                        Transformation::JnKvK { .. } | Transformation::JnKKv { .. } =>
                                            kv_jn_k(large, small, &nest_kv_map, &nest_k_map, ik0, iv0, iv1, target, flow),

                                        Transformation::JnKK { .. } =>
                                            k_jn_k(large, small, &nest_k_map, ik0, iv0, iv1, target, flow),

                                        Transformation::Cartesian { .. } =>
                                            cartesian(large, small, &nest_row_map, iv0, iv1, target, flow),

                                        Transformation::NjKvK { .. } =>
                                            kv_aj_k(large, small, &nest_kv_map, &mut nest_k_map, ik0, iv0, iv1, target, flow),

                                        Transformation::NjKK { .. } =>
                                            k_aj_k(large, small, &mut nest_k_map, ik0, iv0, iv1, target, flow),

                                        _ => panic!("(recursive) abnormal binary transformation"),
                                    };

                                match (ok, ov) {
                                    (0, _) => { // jn → row
                                        nest_row_map.insert(Arc::clone(output_signature), Arc::clone(&output_rel));
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
                                                    nest_row_map.insert(Arc::clone(head_signature), Arc::clone(&output_rel));
                                                }
                                            }
                                        }
                                    },
                                    (_, 0) => { // jn → k
                                        nest_k_map.insert(
                                            Arc::clone(output_signature),
                                            (Arc::clone(&output_rel), Arc::new(output_rel.arrange_set()))
                                        );
                                    }
                                    _ => { // jn → kv
                                        let output_kv = Arc::new(output_rel.arrange_double(ok));
                                        nest_kv_map.insert(
                                            Arc::clone(output_signature),
                                            (Arc::clone(&output_kv), Arc::new(output_kv.arrange_dict()))
                                        );
                                    }
                                }
                            }
                        }

                        /* concatenate and threshold idbs of the recursive strata into the variables_next_map */
                        // debug!("last_signatures_map: {:?}", group_plan.last_signatures_map());
                        recursive_collector(
                            group_plan.last_signatures_map(),
                            &nest_row_map,
                            &mut variables_next_map,
                            &idb_map
                        );

                        /* inspect idbs of the recursive strata (optional) */
                        if tracing::level_enabled!(tracing::Level:: DEBUG) {
                            inspector(
                                &head_signatures_set,
                                &mut variables_next_map,
                                true
                            );
                        }

                        /* set variables and leave scope */
                        let mut variables_leave_map = HashMap::with_capacity(head_signatures_set.len());
                        for head_signature in head_signatures_set.iter().sorted_by_key(|sig| sig.name()) {
                            let variable_next = variables_next_map
                                .remove(&Arc::clone(head_signature))
                                .unwrap_or_else(|| panic!("head missing when leave: {}", head_signature.name()));

                            if let Some(variable) = variables_map.remove(&Arc::clone(head_signature)) {
                                variable.set(&variable_next); // took ownership of the variable
                            } else {
                                panic!("head missing when set: {}", head_signature.name());
                            }

                            variables_leave_map.insert(
                                Arc::clone(head_signature),
                                variable_next.leave()
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
                        if strata.program().idbs().iter().any(|idb| idb.name() == rel_name) {
                            // printsize the relation
                            printsize_generic(&recursive_rel, &format!("[{}]", rel_name), true);
                            if let Some(csv_path) = args.csvs() {
                                // write IDB to csv
                                writesize_generic(&recursive_rel, rel_name, &format!("{}/csvs/size.txt", csv_path));
                                let full_path = format!("{}/csvs/{}.csv", csv_path, rel_name);
                                write_generic(&recursive_rel, &full_path, id, &idb_types(strata.program(), rel_name));
                            }
                        }


                        // if the rel is in the row_map, it will be overwritten
                        row_map.insert(
                            recursive_signature,
                            Arc::new(recursive_rel)
                        );
                    }
                }
            } // end of a strata (group plan)


            /* exports */
            session_map
        });

        if id == 0 {
            info!("{:?}:\tDataflow assembled", timer.elapsed());
        }

        /* feeding edb data */
        for rel_decl in strata.program().edbs() {
            let rel_name = rel_decl.name();
            let rel_path =
                if let Some(path) = rel_decl.path() {
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
                id,
                peers,
                fat_mode
            );
        }

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
    }).expect("execute_from_args dies");
}

/// Configuration for streaming execution.
pub struct StreamingConfig {
    /// EDB names → channels providing pre-encoded i64 rows with diff (+1 insert, -1 retract).
    pub channels: HashMap<String, crossbeam_channel::Receiver<(Vec<i64>, isize)>>,
    /// EDB names that are streaming (don't close their sessions).
    pub streaming_edbs: HashSet<String>,
    /// Callback invoked with (relation_name, row_values_as_strings, diff) for each output tuple.
    pub output_callback: Arc<dyn Fn(&str, Vec<String>, isize) + Send + Sync>,
    /// Shutdown flag — when true, the streaming loop exits.
    pub shutdown: Arc<std::sync::atomic::AtomicBool>,
}

/// Streaming variant of `program_execution`.
///
/// Reuses the same dataflow assembly as batch mode. The only differences:
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
    timely::execute_from_args(args.timely_args().into_iter(), move |worker| {
        let timer = ::std::time::Instant::now();
        let peers = worker.peers();
        let id = worker.index();

        /* assemble dataflow — identical to batch mode */
        let mut session_map = worker.dataflow::<Time, _, _>(|scope| {
            let mut session_map = HashMap::new();
            let mut row_map = HashMap::new();
            let mut kv_map = HashMap::new();
            let mut k_map = HashMap::new();

            for edb in strata.program().edbs() {
                let edb_name = edb.name();
                let (session_generic, input_rel) =
                    construct_session_and_table(scope, edb.arity(), fat_mode);

                session_map.insert(edb_name.to_string(), session_generic);

                row_map.insert(
                    Arc::new(CollectionSignature::new_atom(edb_name)),
                    Arc::new(input_rel),
                );
            }

            if tracing::level_enabled!(tracing::Level::DEBUG) {
                for (signature, rel) in row_map
                    .iter()
                    .sorted_by_key(|(signature, _)| signature.name())
                {
                    printsize_generic(rel, &format!("[{}]", signature.name()), false);
                }
            }

            for (group_plan_idx, group_plan) in group_plans.iter().enumerate() {
                let _is_last_group_plan = group_plan_idx == group_plans.len() - 1;

                if !group_plan.is_recursive() {
                    for next_transformation in group_plan.strata_plan() {
                        let output = next_transformation.output();
                        let output_signature = output.signature();
                        let (ok, ov) = output.arity();
                        let target = ok + ov;

                        if next_transformation.is_unary() {
                            let unary = next_transformation.unary();
                            let (ik, iv) = unary.arity();
                            let input_rel = row_map.get(unary.signature()).unwrap_or_else(|| {
                                panic!("row absent for unary op: {}", unary.signature())
                            });

                            match next_transformation {
                                Transformation::RowToRow { flow, is_no_op, .. } => {
                                    assert!(ik == 0 && ok == 0);
                                    let output_rel = if *is_no_op {
                                        Arc::clone(input_rel)
                                    } else if let TransformationFlow::HeadArith { projections } =
                                        flow
                                    {
                                        Arc::new(codegen_row_row_head_arith!())
                                    } else {
                                        Arc::new(codegen_row_row!())
                                    };
                                    row_map.insert(Arc::clone(output_signature), output_rel);
                                }
                                Transformation::RowToK { flow, is_no_op, .. } => {
                                    assert!(ik == 0 && ov == 0);
                                    let output_rel = if *is_no_op {
                                        Arc::clone(input_rel)
                                    } else {
                                        Arc::new(codegen_row_row!())
                                    };
                                    k_map.insert(
                                        Arc::clone(output_signature),
                                        (
                                            Arc::clone(&output_rel),
                                            Arc::new(output_rel.arrange_set()),
                                        ),
                                    );
                                }
                                Transformation::RowToKv { flow, .. } => {
                                    assert_eq!(ik, 0);
                                    let output_kv = Arc::new(codegen_row_kv!());
                                    kv_map.insert(
                                        Arc::clone(output_signature),
                                        (
                                            Arc::clone(&output_kv),
                                            Arc::new(output_kv.arrange_dict()),
                                        ),
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
                                    kv_jn_k(
                                        large, small, &kv_map, &k_map, ik0, iv0, iv1, target, flow,
                                    )
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
                                    row_map.insert(
                                        Arc::clone(output_signature),
                                        Arc::clone(&output_rel),
                                    );
                                }
                                (_, 0) => {
                                    k_map.insert(
                                        Arc::clone(output_signature),
                                        (
                                            Arc::clone(&output_rel),
                                            Arc::new(output_rel.arrange_set()),
                                        ),
                                    );
                                }
                                _ => {
                                    let output_kv = Arc::new(output_rel.arrange_double(ok));
                                    kv_map.insert(
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

                    non_recursive_collector(
                        group_plan.last_signatures_map(),
                        &mut row_map,
                        &idb_map,
                    );

                    // Streaming output: attach inspect callbacks BEFORE inspector()
                    // because inspector() applies threshold() which blocks output
                    // until frontier advances (incompatible with streaming).
                    {
                        let callback = Arc::clone(&streaming.output_callback);
                        for head_sig in group_plan.head_signatures_set().iter() {
                            let rel_name = head_sig.name().to_string();
                            if strata
                                .program()
                                .idbs()
                                .iter()
                                .any(|idb| idb.name() == rel_name)
                            {
                                if let Some(rel) = row_map.get(head_sig) {
                                    let cb = Arc::clone(&callback);
                                    let name = rel_name.clone();
                                    let types = idb_types(strata.program(), &rel_name);
                                    inspect_streaming_generic(rel, move |row_str, diff| {
                                        cb(&name, reading::decode_cells(&row_str, &types), diff);
                                    });
                                }
                            }
                        }
                    }
                } else {
                    let recursive_out_map = scope.iterative::<Iter, _, _>(|scope| {
                        let head_signatures_set = group_plan.head_signatures_set().clone();
                        let mut variables_map = HashMap::with_capacity(head_signatures_set.len());
                        let mut variables_next_map =
                            HashMap::with_capacity(head_signatures_set.len());

                        for (head_name, head_arity) in
                            group_plan.heads().iter().sorted_by_key(|x| x.0)
                        {
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

                        let dependent_signatures = group_plan.enter_scope_set();
                        for dependent_signature in
                            dependent_signatures.iter().sorted_by_key(|sig| sig.name())
                        {
                            if dependent_signature.name().contains("_sip") {
                                continue;
                            }
                            if let Some(dependent_rel) = row_map.get(dependent_signature) {
                                if head_signatures_set.contains(dependent_signature) {
                                    variables_next_map.insert(
                                        Arc::clone(dependent_signature),
                                        Arc::new(dependent_rel.enter(scope)),
                                    );
                                } else {
                                    nest_row_map.insert(
                                        Arc::clone(dependent_signature),
                                        Arc::new(dependent_rel.enter(scope)),
                                    );
                                }
                            } else if let Some((dependent_kv, _)) = kv_map.get(dependent_signature)
                            {
                                let nested_kv = Arc::new(dependent_kv.enter(scope));
                                let nested_dict = Arc::new(nested_kv.arrange_dict());
                                nest_kv_map.insert(
                                    Arc::clone(dependent_signature),
                                    (nested_kv, nested_dict),
                                );
                            } else if let Some((dependent_k, _)) = k_map.get(dependent_signature) {
                                let nested_k = Arc::new(dependent_k.enter(scope));
                                let nested_set = Arc::new(nested_k.arrange_set());
                                nest_k_map.insert(
                                    Arc::clone(dependent_signature),
                                    (nested_k, nested_set),
                                );
                            } else {
                                assert!(
                                    variables_map.contains_key(dependent_signature),
                                    "dependent {:?} must be defined somewhere of the strata",
                                    dependent_signature
                                );
                            }
                        }

                        for next_transformation in group_plan.strata_plan() {
                            let output = next_transformation.output();
                            let output_signature = output.signature();
                            let (ok, ov) = output.arity();
                            let target = ok + ov;

                            if next_transformation.is_unary() {
                                let unary = next_transformation.unary();
                                let (ik, iv) = unary.arity();
                                let unary_signature = unary.signature();
                                let input_rel = nest_row_map
                                    .get(unary_signature)
                                    .map(Arc::as_ref)
                                    .or_else(|| variables_map.get(unary_signature))
                                    .unwrap_or_else(|| {
                                        panic!("row absent for unary op: {}", unary_signature)
                                    });

                                match next_transformation {
                                    Transformation::RowToRow { flow, is_no_op, .. } => {
                                        assert!(ik == 0 && ok == 0);
                                        let output_rel = if *is_no_op
                                            && nest_row_map.contains_key(unary_signature)
                                        {
                                            Arc::clone(nest_row_map.get(unary_signature).unwrap())
                                        } else if let TransformationFlow::HeadArith {
                                            projections,
                                        } = flow
                                        {
                                            Arc::new(codegen_row_row_head_arith!())
                                        } else {
                                            Arc::new(codegen_row_row!())
                                        };
                                        nest_row_map
                                            .insert(Arc::clone(output_signature), output_rel);
                                    }
                                    Transformation::RowToK { flow, is_no_op, .. } => {
                                        assert!(ik == 0 && ov == 0);
                                        let output_rel = if *is_no_op
                                            && nest_row_map.contains_key(unary_signature)
                                        {
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
                                    Transformation::JnKvKv { .. } => kv_jn_kv(
                                        large,
                                        small,
                                        &nest_kv_map,
                                        ik0,
                                        iv0,
                                        iv1,
                                        target,
                                        flow,
                                    ),
                                    Transformation::JnKvK { .. } | Transformation::JnKKv { .. } => {
                                        kv_jn_k(
                                            large,
                                            small,
                                            &nest_kv_map,
                                            &nest_k_map,
                                            ik0,
                                            iv0,
                                            iv1,
                                            target,
                                            flow,
                                        )
                                    }
                                    Transformation::JnKK { .. } => k_jn_k(
                                        large,
                                        small,
                                        &nest_k_map,
                                        ik0,
                                        iv0,
                                        iv1,
                                        target,
                                        flow,
                                    ),
                                    Transformation::Cartesian { .. } => cartesian(
                                        large,
                                        small,
                                        &nest_row_map,
                                        iv0,
                                        iv1,
                                        target,
                                        flow,
                                    ),
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
                                    Transformation::NjKK { .. } => k_aj_k(
                                        large,
                                        small,
                                        &mut nest_k_map,
                                        ik0,
                                        iv0,
                                        iv1,
                                        target,
                                        flow,
                                    ),
                                    _ => panic!("(recursive) abnormal binary transformation"),
                                };

                                match (ok, ov) {
                                    (0, _) => {
                                        nest_row_map.insert(
                                            Arc::clone(output_signature),
                                            Arc::clone(&output_rel),
                                        );
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
                                        nest_k_map.insert(
                                            Arc::clone(output_signature),
                                            (
                                                Arc::clone(&output_rel),
                                                Arc::new(output_rel.arrange_set()),
                                            ),
                                        );
                                    }
                                    _ => {
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

                        recursive_collector(
                            group_plan.last_signatures_map(),
                            &nest_row_map,
                            &mut variables_next_map,
                            &idb_map,
                        );

                        if tracing::level_enabled!(tracing::Level::DEBUG) {
                            inspector(&head_signatures_set, &mut variables_next_map, true);
                        }

                        let mut variables_leave_map =
                            HashMap::with_capacity(head_signatures_set.len());
                        for head_signature in
                            head_signatures_set.iter().sorted_by_key(|sig| sig.name())
                        {
                            let variable_next = variables_next_map
                                .remove(&Arc::clone(head_signature))
                                .unwrap_or_else(|| {
                                    panic!("head missing when leave: {}", head_signature.name())
                                });

                            if let Some(variable) =
                                variables_map.remove(&Arc::clone(head_signature))
                            {
                                variable.set(&variable_next);
                            } else {
                                panic!("head missing when set: {}", head_signature.name());
                            }

                            variables_leave_map
                                .insert(Arc::clone(head_signature), variable_next.leave());
                        }

                        variables_leave_map
                    });

                    for (recursive_signature, recursive_rel) in recursive_out_map
                        .into_iter()
                        .sorted_by_key(|(sig, _)| sig.name().to_owned())
                    {
                        let rel_name = recursive_signature.name();

                        if strata
                            .program()
                            .idbs()
                            .iter()
                            .any(|idb| idb.name() == rel_name)
                        {
                            printsize_generic(&recursive_rel, &format!("[{}]", rel_name), true);
                            if let Some(csv_path) = args.csvs() {
                                writesize_generic(
                                    &recursive_rel,
                                    rel_name,
                                    &format!("{}/csvs/size.txt", csv_path),
                                );
                                let full_path = format!("{}/csvs/{}.csv", csv_path, rel_name);
                                write_generic(
                                    &recursive_rel,
                                    &full_path,
                                    id,
                                    &idb_types(strata.program(), rel_name),
                                );
                            }

                            // Streaming output inspect for recursive IDBs
                            {
                                let cb = Arc::clone(&streaming.output_callback);
                                let name = rel_name.to_string();
                                let types = idb_types(strata.program(), rel_name);
                                inspect_streaming_generic(&recursive_rel, move |row_str, diff| {
                                    cb(&name, reading::decode_cells(&row_str, &types), diff);
                                });
                            }
                        }

                        row_map.insert(recursive_signature, Arc::new(recursive_rel));
                    }
                }
            }

            session_map
        });

        if id == 0 {
            info!("{:?}:\tDataflow assembled (streaming)", timer.elapsed());
        }

        /* feeding batch EDB data at epoch 0 */
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
                id,
                peers,
                fat_mode,
            );
        }

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

        loop {
            if streaming
                .shutdown
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                break;
            }

            let mut had_updates = false;
            for (rel_name, rx) in &streaming.channels {
                if let Some(session) = session_map.get_mut(rel_name) {
                    while let Ok((encoded_row, diff)) = rx.try_recv() {
                        update_session_generic(
                            session,
                            &encoded_row,
                            fat_mode,
                            diff as reading::Semiring,
                        );
                        had_updates = true;
                    }
                }
            }

            if had_updates {
                // Advance to next epoch, seal current data, step.
                epoch.0 += 1;
                for (_rel_name, session) in session_map.iter_mut() {
                    session.advance_to(epoch);
                    session.flush();
                }
                worker.step();
            } else {
                worker.step();
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        // Close all remaining sessions
        for (_rel_name, session) in session_map.drain() {
            session.close();
        }

        // Step to drain any remaining work
        while worker.step() {}

        if id == 0 {
            info!("{:?}:\tStreaming execution complete", timer.elapsed());
        }
    })
    .expect("execute_from_args dies (streaming)");
}
