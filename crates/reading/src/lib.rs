pub mod arrangements;
pub mod config;
pub mod inspect;
pub mod interner;
pub mod reader;
pub mod rel;
pub mod row;
pub mod semiring;
pub mod session;

// export configuration constants for backwards compatibility
pub use config::{FALLBACK_ARITY, KV_MAX, PROD_MAX, ROW_MAX};

// export semiring types and functions for convenience
pub use semiring::{diff_to_i32, semiring_one, Min, Semiring, SEMIRING_TYPE};

// String/float codec: makes `string` and `float` first-class column types
// inside the engine (see `interner`).
pub use interner::{
    decode, decode_cells, decode_row, decode_value, encode_literals, encode_token, float_to_i64,
    intern, lock_interner, InternLock,
};

#[cfg(all(feature = "present-type", not(feature = "isize-type")))]
pub use differential_dataflow::operators::iterate::SemigroupVariable as RecVariable;
/// The iterative recursion variable, chosen by semiring:
///
/// - `isize` (an Abelian group): the full `Variable`, which subtracts the prior
///   iterate so recursive facts that lose support are *retracted*. Required for
///   correct incremental maintenance of recursion under deletion (otherwise a
///   fact kept alive only by circular support never goes away).
/// - `Present` (a semigroup, no negation): `SemigroupVariable`, the "only grows"
///   variant, which is all that semiring can support.
#[cfg(all(feature = "isize-type", not(feature = "present-type")))]
pub use differential_dataflow::operators::iterate::Variable as RecVariable;

// feature propagation through dependency chain && mutually exclusive feature configuration
// workspace
//     ↓ --features isize-type
// executing crate
//     ↓ enables isize-type = ["reading/isize-type", "macros/isize-type"]
// macros crate
//     ↓ enables isize-type = ["reading/isize-type"]
// reading crate
//     ↓ compiles with isize type

pub mod epoch;
pub use epoch::Epoch;
pub type Time = Epoch;
pub type Iter = u16;
