use differential_dataflow::difference::{IsZero, Monoid, Multiply, Semigroup};
use serde::{Deserialize, Serialize};

// Re-export Present for convenience
#[cfg(all(feature = "present-type", not(feature = "isize-type")))]
use differential_dataflow::difference::Present;

// Conditional compilation for semiring type selection
#[cfg(all(feature = "present-type", not(feature = "isize-type")))]
pub type Semiring = Present;

#[cfg(all(feature = "isize-type", not(feature = "present-type")))]
pub type Semiring = isize;

// Helper function to create the appropriate semiring value
#[cfg(all(feature = "present-type", not(feature = "isize-type")))]
pub fn semiring_one() -> Semiring {
    Present {}
}

#[cfg(all(feature = "isize-type", not(feature = "present-type")))]
pub fn semiring_one() -> Semiring {
    1
}

/// Convert a difference into a signed integer count, used by antijoin
/// (`Rel::subtract`) to accumulate `#self - #other` per key.
///
/// With the `isize` semiring this preserves the real multiplicity so that
/// retractions propagate incrementally; with the `Present` semiring (existence
/// only, no retraction) every record counts as `1`.
#[cfg(all(feature = "present-type", not(feature = "isize-type")))]
pub fn diff_to_i32(_d: &Semiring) -> i32 {
    1
}

#[cfg(all(feature = "isize-type", not(feature = "present-type")))]
pub fn diff_to_i32(d: &Semiring) -> i32 {
    *d as i32
}

// Compile-time check to ensure exactly one semiring feature is enabled
#[cfg(all(feature = "present-type", feature = "isize-type"))]
compile_error!("Cannot enable both present-type and isize-type features at once");

#[cfg(not(any(feature = "present-type", feature = "isize-type")))]
compile_error!("Must enable exactly one semiring feature: either present-type or isize-type");

// Debug: expose which semiring type is active
#[cfg(all(feature = "present-type", not(feature = "isize-type")))]
pub const SEMIRING_TYPE: &str = "Present";

#[cfg(all(feature = "isize-type", not(feature = "present-type")))]
pub const SEMIRING_TYPE: &str = "isize";

/// MIN Semiring
#[derive(Copy, Debug, Clone, Hash, PartialOrd, Ord, PartialEq, Eq, Serialize, Deserialize)]
pub struct Min {
    pub value: u64,
}

impl Min {
    /// Creates a new `Min` with a value.
    pub fn new(value: u64) -> Self {
        Min { value }
    }

    /// Creates a new `Min` representing infinity (u64::MAX).
    /// This serves as the additive identity in the MIN semiring:
    /// min(a, ∞) = a for any value a.
    pub fn infinity() -> Self {
        Min { value: u64::MAX }
    }

    /// Returns true if this Min represents infinity.
    pub fn is_infinity(&self) -> bool {
        self.value == u64::MAX
    }
}

impl IsZero for Min {
    fn is_zero(&self) -> bool {
        false // always return false
    }
}

impl Semigroup for Min {
    fn plus_equals(&mut self, rhs: &Self) {
        self.value = std::cmp::min(self.value, rhs.value);
    }
}

impl Monoid for Min {
    fn zero() -> Self {
        Min::infinity() // additive identity is infinity
    }
}

// For converting i64 differences to Min (preserves the Min value)
impl Multiply<i64> for Min {
    type Output = Min;

    fn multiply(self, _rhs: &i64) -> Self::Output {
        self
    }
}

// Convenience implementations for easier use
impl From<u64> for Min {
    fn from(value: u64) -> Self {
        Min::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_min_semigroup() {
        let mut a = Min::new(5);
        let b = Min::new(3);
        a.plus_equals(&b);
        assert_eq!(a.value, 3);

        let mut inf = Min::infinity();
        inf.plus_equals(&Min::new(42));
        assert_eq!(inf.value, 42);
    }

    #[test]
    fn test_min_zero() {
        let zero = Min::zero();
        // In a MIN semiring, is_zero() is always false (values are never "absent").
        assert!(!zero.is_zero());
        assert!(zero.is_infinity());
        assert_eq!(zero.value, u64::MAX);
    }

    mod property_tests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn min_associativity(a in any::<u64>(), b in any::<u64>(), c in any::<u64>()) {
                // (a ⊕ b) ⊕ c == a ⊕ (b ⊕ c)
                let mut ab = Min::new(a);
                ab.plus_equals(&Min::new(b));
                let mut ab_c = ab;
                ab_c.plus_equals(&Min::new(c));

                let mut bc = Min::new(b);
                bc.plus_equals(&Min::new(c));
                let mut a_bc = Min::new(a);
                a_bc.plus_equals(&bc);

                prop_assert_eq!(ab_c, a_bc);
            }

            #[test]
            fn min_commutativity(a in any::<u64>(), b in any::<u64>()) {
                let mut ab = Min::new(a);
                ab.plus_equals(&Min::new(b));
                let mut ba = Min::new(b);
                ba.plus_equals(&Min::new(a));
                prop_assert_eq!(ab, ba);
            }

            #[test]
            fn min_identity(a in any::<u64>()) {
                // a ⊕ zero == a (zero = infinity)
                let mut result = Min::new(a);
                result.plus_equals(&Min::zero());
                prop_assert_eq!(result.value, a);
            }

            #[test]
            fn min_idempotence(a in any::<u64>()) {
                let mut result = Min::new(a);
                result.plus_equals(&Min::new(a));
                prop_assert_eq!(result.value, a);
            }

            #[test]
            fn min_infinity_absorbing(x in 0..u64::MAX) {
                // min(x, ∞) == x for all finite x
                let mut result = Min::new(x);
                result.plus_equals(&Min::infinity());
                prop_assert_eq!(result.value, x);
            }

            #[test]
            fn min_is_zero_always_false(v in any::<u64>()) {
                prop_assert!(!Min::new(v).is_zero());
            }
        }
    }
}
