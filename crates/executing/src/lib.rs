pub mod aggregation;
pub mod arg;
pub mod collector;
pub mod compare;
pub mod dataflow;
pub mod jn;
pub mod map;
pub mod transformer;

pub type Time = reading::Epoch;
/// Inner (fixpoint) iteration counter. u32: a divergent recursion used to
/// WRAP the old u16 at 65535, silently corrupting nested timestamps (the
/// Product lattice order is violated by wraparound).
pub type Iter = u32;
