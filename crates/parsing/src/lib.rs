//! FlowLog program representation.
//!
//! This crate holds the AST for FlowLog, a logic programming language —
//! declarations, rules, heads, atoms, arithmetic/comparison expressions and
//! aggregations — together with the decl-driven typing/validation pass that
//! runs when a [`parser::Program`] is constructed.
//!
//! Parsing itself lives in the `syntax` crate (a chumsky parser with ariadne
//! error reports), which builds these types. The language's shape is
//! documented in the workspace Readme ("Writing rules").

pub mod aggregation; // Aggregation functions (sum, max, min, count)
pub mod arithmetic; // Arithmetic expressions and operations
pub mod compare; // Comparison operations (>, <, =, !=, etc.)
pub mod decl; // Relation declarations and column types
pub mod head; // Head expressions in logic rules
pub mod parser; // The Program container (construction runs typing/validation)
pub mod rule; // Complete rule definitions and structures
pub mod typing; // Decl-driven typing pass (float vs integer evaluation modes)
