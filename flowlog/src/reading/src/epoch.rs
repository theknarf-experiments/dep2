use std::fmt;

use columnation::{Columnation, CopyRegion};
use differential_dataflow::lattice::Lattice;
use serde::{Deserialize, Serialize};
use timely::order::{Empty, PartialOrder, TotalOrder};
use timely::progress::timestamp::{PathSummary, Refines, Timestamp};

/// Epoch-based timestamp for streaming execution.
///
/// Wraps a `u64` epoch counter. Unlike bare `u64`, `Epoch` implements
/// `timely::order::Empty` so it can be used as the outer timestamp in
/// `Product<Epoch, Iter>` for recursive Datalog evaluation.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Epoch(pub u64);

// --- timely traits ---

impl PartialOrder for Epoch {
    #[inline]
    fn less_than(&self, other: &Self) -> bool {
        self.0 < other.0
    }
    #[inline]
    fn less_equal(&self, other: &Self) -> bool {
        self.0 <= other.0
    }
}

impl TotalOrder for Epoch {}

impl Empty for Epoch {}

impl Timestamp for Epoch {
    type Summary = Epoch;
    fn minimum() -> Self {
        Epoch(u64::MIN)
    }
}

impl PathSummary<Epoch> for Epoch {
    #[inline]
    fn results_in(&self, src: &Epoch) -> Option<Epoch> {
        self.0.checked_add(src.0).map(Epoch)
    }
    #[inline]
    fn followed_by(&self, other: &Epoch) -> Option<Epoch> {
        self.0.checked_add(other.0).map(Epoch)
    }
}

impl Refines<()> for Epoch {
    fn to_inner(_: ()) -> Epoch {
        Default::default()
    }
    fn to_outer(self) {}
    fn summarize(_: <Epoch as Timestamp>::Summary) {}
}

// --- differential-dataflow traits ---

impl Lattice for Epoch {
    #[inline]
    fn join(&self, other: &Self) -> Self {
        Epoch(std::cmp::max(self.0, other.0))
    }
    #[inline]
    fn meet(&self, other: &Self) -> Self {
        Epoch(std::cmp::min(self.0, other.0))
    }
}

// --- columnation ---

impl Columnation for Epoch {
    type InnerRegion = CopyRegion<Epoch>;
}

// --- Display (for debugging) ---

impl fmt::Display for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
