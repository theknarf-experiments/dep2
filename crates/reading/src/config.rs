//! Build-time configuration constants for FlowLog
//!
//! These constants control the compile-time code generation and runtime limits
//! for various operations in the FlowLog engine.

/// Maximum arity for key-value in code generation
pub const KV_MAX: usize = 4;

/// Maximum arity for row in code generation
///
/// Every plain-row codegen space is generated over this, the cartesian
/// product's included. The planner measures a plain row against
/// [`FALLBACK_ARITY`] when it decides fat mode, so a codegen space narrower
/// than that budget leaves shapes planned thin with no arm to run in — a miss
/// that surfaces as every worker panicking mid-run rather than as a load-time
/// error.
pub const ROW_MAX: usize = 7;

/// Maximum arity before falling back to fat representations
pub const FALLBACK_ARITY: usize = ROW_MAX;

/// Configuration for compile-time code generation limits
pub struct CodegenLimits;

impl CodegenLimits {
    pub const KV_MAX: usize = KV_MAX;
    pub const ROW_MAX: usize = ROW_MAX;
    pub const FALLBACK_ARITY: usize = FALLBACK_ARITY;
}
