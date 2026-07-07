use differential_dataflow::lattice::Lattice;
use differential_dataflow::operators::arrange::Arranged;
use differential_dataflow::operators::arrange::TraceAgent;
use differential_dataflow::operators::ThresholdTotal;
use differential_dataflow::Data;
use paste::paste;
use std::rc::Rc; // reference counted pointer // differential dataflow trace implementation
use timely::order::TotalOrder;
use timely::progress::timestamp::Timestamp;

use crate::rel::Rel;
use crate::row::FatRow;
use crate::row::Row;
#[cfg(all(feature = "present-type", not(feature = "isize-type")))]
use crate::semiring_one;
use crate::Semiring;

use differential_dataflow::trace::implementations::ord_neu::OrdValBatch;
use differential_dataflow::trace::implementations::spine_fueled::Spine;
use differential_dataflow::trace::implementations::Vector;

/* ------------------------------------------------------------------------------------ */
/* Dict */
/* ------------------------------------------------------------------------------------ */
// Arranged<'scope, TraceAgent<Spine<Rc<OrdValBatch< Vector<((u32, u32), Product<(), u64>, Present)>> >>>>

pub type BatchDict<const K: usize, const V: usize, T, R> = ((Row<K>, Row<V>), T, R);
pub type VectorBatchDict<const K: usize, const V: usize, T, R> = Vector<BatchDict<K, V, T, R>>;
pub type DictTrace<const K: usize, const V: usize, T, R> =
    TraceAgent<Spine<Rc<OrdValBatch<VectorBatchDict<K, V, T, R>>>>>;

pub type ArrangedDictType<'scope, const K: usize, const V: usize, T, R> =
    Arranged<'scope, DictTrace<K, V, T, R>>;

// Fat row arrangements for fallback
pub type BatchDictFat<T, R> = ((FatRow, FatRow), T, R);
pub type VectorBatchDictFat<T, R> = Vector<BatchDictFat<T, R>>;
pub type DictTraceFat<T, R> = TraceAgent<Spine<Rc<OrdValBatch<VectorBatchDictFat<T, R>>>>>;
pub type ArrangedDictTypeFat<'scope, T, R> = Arranged<'scope, DictTraceFat<T, R>>;

macro_rules! impl_dicts {
    ($(($K:literal, $V:literal)),*) => {
        paste! {
            pub enum ArrangedDict<'scope, T: Timestamp>
            where
                T: Data+Lattice+TotalOrder,
            {
                $(
                    [<ArrangedDict $K _ $V>](ArrangedDictType<'scope, $K, $V, T, Semiring>),
                )*
                // Fallback for large arities using FatRow
                ArrangedDictFat(ArrangedDictTypeFat<'scope, T, Semiring>, usize, usize), // Store K and V arities
            }

            impl<'scope, T: Timestamp> ArrangedDict<'scope, T>
            where
                T: Data+Lattice+TotalOrder,
            {
                pub fn arity(&self) -> (usize, usize) {
                    match self {
                        $( ArrangedDict::[<ArrangedDict $K _ $V>](_) => ($K, $V), )*
                        ArrangedDict::ArrangedDictFat(_, k, v) => (*k, *v),
                    }
                }

                /// Check if this ArrangedDict uses FatRow (heap-allocated)
                pub fn is_fat(&self) -> bool {
                    matches!(self, ArrangedDict::ArrangedDictFat(_, _, _))
                }

                /// Check if this ArrangedDict uses fixed-size Row<N> (stack-allocated)
                pub fn is_thin(&self) -> bool {
                    !self.is_fat()
                }
            }

            impl<'scope, T: Timestamp> ArrangedDict<'scope, T>
            where
                T: Data+Lattice+TotalOrder,
            {
                $(
                    pub fn [<dict_ $K _ $V>](&self) -> &ArrangedDictType<'scope, $K, $V, T, Semiring> {
                        match self {
                            ArrangedDict::[<ArrangedDict $K _ $V>](dict) => dict,
                            _ => panic!("panic access to dict of arity ({}, {})", $K, $V),
                        }
                    }
                )*

                pub fn dict_fat(&self) -> &ArrangedDictTypeFat<'scope, T, Semiring> {
                    match self {
                        ArrangedDict::ArrangedDictFat(dict, _, _) => dict,
                        _ => panic!("Cannot access fat dict on fixed-arity arrangement"),
                    }
                }
            }
        }
    };
}

impl_dicts!(
    (1, 1),
    (1, 2),
    (1, 3),
    (1, 4),
    (1, 5),
    (1, 6),
    (1, 7),
    (1, 8),
    (2, 1),
    (2, 2),
    (2, 3),
    (2, 4),
    (2, 5),
    (2, 6),
    (2, 7),
    (2, 8),
    (3, 1),
    (3, 2),
    (3, 3),
    (3, 4),
    (3, 5),
    (3, 6),
    (3, 7),
    (3, 8),
    (4, 1),
    (4, 2),
    (4, 3),
    (4, 4),
    (4, 5),
    (4, 6),
    (4, 7),
    (4, 8),
    (5, 1),
    (5, 2),
    (5, 3),
    (5, 4),
    (5, 5),
    (5, 6),
    (5, 7),
    (5, 8),
    (6, 1),
    (6, 2),
    (6, 3),
    (6, 4),
    (6, 5),
    (6, 6),
    (6, 7),
    (6, 8),
    (7, 1),
    (7, 2),
    (7, 3),
    (7, 4),
    (7, 5),
    (7, 6),
    (7, 7),
    (7, 8),
    (8, 1),
    (8, 2),
    (8, 3),
    (8, 4),
    (8, 5),
    (8, 6),
    (8, 7),
    (8, 8)
);

// impl for 4 by 4
// impl_dicts!(
//     (1, 1), (1, 2), (1, 3), (1, 4),
//     (2, 1), (2, 2), (2, 3), (2, 4),
//     (3, 1), (3, 2), (3, 3), (3, 4),
//     (4, 1), (4, 2), (4, 3), (4, 4)
// );

/* ------------------------------------------------------------------------------------ */
/* Set */
/* ------------------------------------------------------------------------------------ */
// Arranged<'scope, TraceAgent<Spine<Rc<OrdKeyBatch< Vector<((Row<K>, ()), Product<(), u64>, Present)>> >>>>
use differential_dataflow::trace::implementations::ord_neu::OrdKeyBatch;
pub type BatchSet<const K: usize, T, R> = ((Row<K>, ()), T, R);
pub type VectorBatchSet<const K: usize, T, R> = Vector<BatchSet<K, T, R>>;
pub type SetTrace<const K: usize, T, R> =
    TraceAgent<Spine<Rc<OrdKeyBatch<VectorBatchSet<K, T, R>>>>>;
pub type ArrangedSetType<'scope, const K: usize, T, R> = Arranged<'scope, SetTrace<K, T, R>>;

// Fat row set arrangements for fallback
pub type BatchSetFat<T, R> = ((FatRow, ()), T, R);
pub type VectorBatchSetFat<T, R> = Vector<BatchSetFat<T, R>>;
pub type SetTraceFat<T, R> = TraceAgent<Spine<Rc<OrdKeyBatch<VectorBatchSetFat<T, R>>>>>;
pub type ArrangedSetTypeFat<'scope, T, R> = Arranged<'scope, SetTraceFat<T, R>>;

macro_rules! impl_sets {
    ($($K:literal),*) => {
        paste! {
            pub enum ArrangedSet<'scope, T: Timestamp>
            where
                T: Data+Lattice+TotalOrder,
            {
                $( [<ArrangedSet $K>](ArrangedSetType<'scope, $K, T, Semiring>), )*
                // Fallback for large arities using FatRow
                ArrangedSetFat(ArrangedSetTypeFat<'scope, T, Semiring>, usize), // Store K arity
            }

            impl<'scope, T: Timestamp> ArrangedSet<'scope, T>
            where
                T: Data+Lattice+TotalOrder,
            {
                pub fn arity(&self) -> usize {
                    match self {
                        $( ArrangedSet::[<ArrangedSet $K>](_) => $K, )*
                        ArrangedSet::ArrangedSetFat(_, k) => *k,
                    }
                }

                /// Check if this ArrangedSet uses FatRow (heap-allocated)
                pub fn is_fat(&self) -> bool {
                    matches!(self, ArrangedSet::ArrangedSetFat(_, _))
                }

                /// Check if this ArrangedSet uses fixed-size Row<N> (stack-allocated)
                pub fn is_thin(&self) -> bool {
                    !self.is_fat()
                }

                pub fn threshold(&self) -> Rel<'scope, T> {
                    // Deduplicate to a set: present iff accumulated multiplicity > 0.
                    // `threshold_total` (isize) emits f(new)-f(old) so retractions
                    // propagate — essential for the negated side of an antijoin to
                    // re-derive when the negated relation loses a row. `Present`
                    // (batch only) keeps the first-seen toggle.
                    if self.is_fat() {
                        #[cfg(all(feature = "isize-type", not(feature = "present-type")))]
                        let out = self.set_fat().clone().threshold_total(|_, c| if *c > 0 { 1isize } else { 0isize });
                        #[cfg(all(feature = "present-type", not(feature = "isize-type")))]
                        let out = self
                            .set_fat()
                            .clone()
                            .threshold_semigroup(move |_, _, old| old.is_none().then_some(semiring_one()));
                        Rel::CollectionFat(out, self.arity())
                    } else {
                        match self {
                            $( ArrangedSet::[<ArrangedSet $K>](set) => {
                                #[cfg(all(feature = "isize-type", not(feature = "present-type")))]
                                let out = set.clone().threshold_total(|_, c| if *c > 0 { 1isize } else { 0isize });
                                #[cfg(all(feature = "present-type", not(feature = "isize-type")))]
                                let out = set
                                    .clone()
                                    .threshold_semigroup(move |_, _, old| old.is_none().then_some(semiring_one()));
                                Rel::[<Collection $K>](out)
                            },
                            )*
                            ArrangedSet::ArrangedSetFat(_, _) => unreachable!("Fat case should be handled elsewhere"),
                        }
                    }
                }
            }

            impl<'scope, T: Timestamp> ArrangedSet<'scope, T>
            where
                T: Data+Lattice+TotalOrder,
            {
                $(
                    pub fn [<set_ $K>](&self) -> &ArrangedSetType<'scope, $K, T, Semiring> {
                        match self {
                            ArrangedSet::[<ArrangedSet $K>](set) => set,
                            _ => panic!("panic access to set_{} of arity {}", $K, $K),
                        }
                    }
                )*

                pub fn set_fat(&self) -> &ArrangedSetTypeFat<'scope, T, Semiring> {
                    match self {
                        ArrangedSet::ArrangedSetFat(set, _) => set,
                        _ => panic!("Cannot access fat set on fixed-arity arrangement"),
                    }
                }
            }
        }
    };
}

impl_sets!(0, 1, 2, 3, 4, 5, 6, 7, 8);

/* ------------------------------------------------------------------------------------ */
/* Exportable set traces (live handles for late-added dataflows) */
/* ------------------------------------------------------------------------------------ */

use differential_dataflow::operators::arrange::ShutdownButton;
use differential_dataflow::trace::TraceReader;
use timely::dataflow::operators::CapabilitySet;
use timely::dataflow::Scope;
use timely::progress::frontier::AntichainRef;

macro_rules! impl_set_traces {
    ($($K:literal),*) => {
        paste! {
            /// A cloneable handle to a whole-row set arrangement's shared trace,
            /// detached from the dataflow scope that built it. Importing it into
            /// a dataflow constructed LATER yields the relation's consolidated
            /// state (at the trace's compaction frontier) plus every subsequent
            /// update, timestamps intact — the primitive behind adding queries
            /// to a running engine.
            pub enum SetTraceGeneric<T: Timestamp>
            where
                T: Data + Lattice + TotalOrder,
            {
                $( [<TraceSet $K>](SetTrace<$K, T, Semiring>), )*
                TraceSetFat(SetTraceFat<T, Semiring>, usize),
            }

            impl<T: Timestamp> SetTraceGeneric<T>
            where
                T: Data + Lattice + TotalOrder,
            {
                pub fn arity(&self) -> usize {
                    match self {
                        $( SetTraceGeneric::[<TraceSet $K>](_) => $K, )*
                        SetTraceGeneric::TraceSetFat(_, k) => *k,
                    }
                }

                /// Import into `scope` as a collection, plus the shutdown button
                /// that tears the import down (pressing it releases the source so
                /// the enclosing dataflow can retire — the drop-a-query path).
                pub fn import_core<'scope>(
                    &mut self,
                    scope: Scope<'scope, T>,
                    name: &str,
                ) -> (Rel<'scope, T>, ShutdownButton<CapabilitySet<T>>) {
                    match self {
                        $(
                            SetTraceGeneric::[<TraceSet $K>](trace) => {
                                let (arranged, button) = trace.import_core(scope, name);
                                (
                                    Rel::[<Collection $K>](arranged.as_collection(|k, _| k.clone())),
                                    button,
                                )
                            }
                        )*
                        SetTraceGeneric::TraceSetFat(trace, k) => {
                            let (arranged, button) = trace.import_core(scope, name);
                            (
                                Rel::CollectionFat(arranged.as_collection(|k, _| k.clone()), *k),
                                button,
                            )
                        }
                    }
                }

                /// Allow logical compaction up to `frontier`: history strictly
                /// before it may consolidate, so a later import sees merged state
                /// at the frontier instead of the full update history (same
                /// contents, bounded memory). Never advance past the epoch the
                /// dataflow has sealed.
                pub fn set_logical_compaction(&mut self, frontier: &[T]) {
                    match self {
                        $(
                            SetTraceGeneric::[<TraceSet $K>](trace) => {
                                trace.set_logical_compaction(AntichainRef::new(frontier))
                            }
                        )*
                        SetTraceGeneric::TraceSetFat(trace, _) => {
                            trace.set_logical_compaction(AntichainRef::new(frontier))
                        }
                    }
                }

                /// Diagnostic: the trace's current (since, upper) frontiers.
                pub fn frontiers(&mut self) -> (Vec<T>, Vec<T>) {
                    use timely::progress::Antichain;
                    let mut upper = Antichain::new();
                    match self {
                        $(
                            SetTraceGeneric::[<TraceSet $K>](trace) => {
                                trace.read_upper(&mut upper);
                                (
                                    trace.get_logical_compaction().to_vec(),
                                    upper.elements().to_vec(),
                                )
                            }
                        )*
                        SetTraceGeneric::TraceSetFat(trace, _) => {
                            trace.read_upper(&mut upper);
                            (
                                trace.get_logical_compaction().to_vec(),
                                upper.elements().to_vec(),
                            )
                        }
                    }
                }

                /// Allow physical batch merging up to `frontier` (must trail
                /// logical compaction).
                pub fn set_physical_compaction(&mut self, frontier: &[T]) {
                    match self {
                        $(
                            SetTraceGeneric::[<TraceSet $K>](trace) => {
                                trace.set_physical_compaction(AntichainRef::new(frontier))
                            }
                        )*
                        SetTraceGeneric::TraceSetFat(trace, _) => {
                            trace.set_physical_compaction(AntichainRef::new(frontier))
                        }
                    }
                }
            }

            impl<'scope, T: Timestamp> ArrangedSet<'scope, T>
            where
                T: Data + Lattice + TotalOrder,
            {
                /// A detached, cloneable handle to this arrangement's shared
                /// trace (see [`SetTraceGeneric`]).
                pub fn trace_generic(&self) -> SetTraceGeneric<T> {
                    match self {
                        $(
                            ArrangedSet::[<ArrangedSet $K>](set) => {
                                SetTraceGeneric::[<TraceSet $K>](set.trace.clone())
                            }
                        )*
                        ArrangedSet::ArrangedSetFat(set, k) => {
                            SetTraceGeneric::TraceSetFat(set.trace.clone(), *k)
                        }
                    }
                }
            }
        }
    };
}

impl_set_traces!(0, 1, 2, 3, 4, 5, 6, 7, 8);

#[cfg(test)]
mod trace_tests {
    use crate::inspect::{inspect_streaming_generic, probe_streaming_generic};
    use crate::reader::{construct_session_and_table, update_session_generic};
    use crate::{Epoch, Semiring, Time};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    type Acc = Arc<Mutex<HashMap<Vec<i64>, isize>>>;

    /// Feed encoded rows, seal the epoch, and step once so the arrangement
    /// absorbs the batch.
    fn feed(
        worker: &mut timely::worker::Worker,
        session: &mut crate::session::InputSessionGeneric<Time>,
        rows: &[(&[i64], isize)],
        next_epoch: u64,
    ) {
        for (row, diff) in rows {
            update_session_generic(session, row, false, *diff as Semiring);
        }
        session.advance_to(Epoch(next_epoch));
        session.flush();
        worker.step();
    }

    /// Import `trace` into a fresh dataflow, accumulating (row -> net count).
    fn import_into_acc(
        worker: &mut timely::worker::Worker,
        trace: &mut super::SetTraceGeneric<Time>,
        probe: &mut timely::dataflow::operators::probe::Handle<Time>,
    ) -> (
        Acc,
        differential_dataflow::operators::arrange::ShutdownButton<
            timely::dataflow::operators::CapabilitySet<Time>,
        >,
    ) {
        let acc: Acc = Arc::new(Mutex::new(HashMap::new()));
        let acc_cb = Arc::clone(&acc);
        let button = worker.dataflow::<Time, _, _>(|scope| {
            let (rel, button) = trace.import_core(scope, "late");
            inspect_streaming_generic(&rel, move |row, diff| {
                *acc_cb.lock().unwrap().entry(row.to_vec()).or_insert(0) += diff;
            });
            probe_streaming_generic(&rel, probe);
            button
        });
        (acc, button)
    }

    fn net(acc: &Acc, row: &[i64]) -> isize {
        acc.lock().unwrap().get(row).copied().unwrap_or(0)
    }

    #[test]
    fn import_replays_history_and_follows_updates() {
        timely::execute_directly(|worker| {
            let mut probe = timely::dataflow::operators::probe::Handle::<Time>::new();
            let (mut session, mut trace) = worker.dataflow::<Time, _, _>(|scope| {
                let (session, rel) = construct_session_and_table(scope, 2, false);
                (session, rel.arrange_set().trace_generic())
            });

            // Epoch 0: two distinct rows, one inserted twice (multiplicity 2).
            feed(
                worker,
                &mut session,
                &[(&[1, 2], 1), (&[1, 2], 1), (&[2, 3], 1)],
                1,
            );

            // A dataflow built AFTER that history must replay it in full...
            let (acc, _button) = import_into_acc(worker, &mut trace, &mut probe);
            worker.step_while(|| probe.less_than(&Epoch(1)));
            assert_eq!(net(&acc, &[1, 2]), 2, "multiplicities survive the import");
            assert_eq!(net(&acc, &[2, 3]), 1);

            // ...and then follow live updates: a retraction and a fresh insert.
            feed(worker, &mut session, &[(&[2, 3], -1), (&[5, 6], 1)], 2);
            worker.step_while(|| probe.less_than(&Epoch(2)));
            assert_eq!(net(&acc, &[2, 3]), 0, "retraction reached the import");
            assert_eq!(net(&acc, &[5, 6]), 1);

            session.close();
            while worker.step() {}
        });
    }

    #[test]
    fn import_after_compaction_sees_consolidated_state() {
        timely::execute_directly(|worker| {
            let mut probe = timely::dataflow::operators::probe::Handle::<Time>::new();
            let (mut session, mut trace) = worker.dataflow::<Time, _, _>(|scope| {
                let (session, rel) = construct_session_and_table(scope, 1, false);
                (session, rel.arrange_set().trace_generic())
            });

            // A churny history: insert, retract, re-insert across epochs.
            feed(worker, &mut session, &[(&[7], 1), (&[8], 1)], 1);
            feed(worker, &mut session, &[(&[7], -1)], 2);
            feed(worker, &mut session, &[(&[7], 1), (&[9], 1)], 3);

            // Let the trace consolidate everything before epoch 3, then import:
            // the NET state must be identical to the uncompacted history.
            trace.set_logical_compaction(&[Epoch(3)]);
            trace.set_physical_compaction(&[Epoch(3)]);
            let (acc, _button) = import_into_acc(worker, &mut trace, &mut probe);
            worker.step_while(|| probe.less_than(&Epoch(3)));
            assert_eq!(net(&acc, &[7]), 1);
            assert_eq!(net(&acc, &[8]), 1);
            assert_eq!(net(&acc, &[9]), 1);

            // And it still follows post-compaction updates.
            feed(worker, &mut session, &[(&[8], -1)], 4);
            worker.step_while(|| probe.less_than(&Epoch(4)));
            assert_eq!(net(&acc, &[8]), 0);

            session.close();
            while worker.step() {}
        });
    }

    #[test]
    fn shutdown_button_releases_the_import() {
        timely::execute_directly(|worker| {
            let mut probe = timely::dataflow::operators::probe::Handle::<Time>::new();
            let (mut session, mut trace) = worker.dataflow::<Time, _, _>(|scope| {
                let (session, rel) = construct_session_and_table(scope, 1, false);
                (session, rel.arrange_set().trace_generic())
            });
            feed(worker, &mut session, &[(&[1], 1)], 1);

            let (acc, mut button) = import_into_acc(worker, &mut trace, &mut probe);
            worker.step_while(|| probe.less_than(&Epoch(1)));
            assert_eq!(net(&acc, &[1]), 1);

            // Press the button: the import's frontier empties (dataflow can
            // retire), and later base updates no longer reach the accumulator.
            // Stepping is bounded: the base session stays open, so `while
            // worker.step()` would spin forever.
            button.press();
            for _ in 0..16 {
                worker.step();
            }
            assert!(probe.done(), "released import drains its frontier");

            feed(worker, &mut session, &[(&[2], 1)], 2);
            for _ in 0..16 {
                worker.step();
            }
            assert_eq!(net(&acc, &[2]), 0, "no updates after shutdown");

            session.close();
            while worker.step() {}
        });
    }
}
