//! dep2-core: a thin, HCL-free engine that drives the FlowLog incremental
//! Datalog engine from streaming plugin data sources.
//!
//! The pipeline is:
//!   native `.dl` program  +  streaming source bindings
//!     -> intern string literals into a shared string table
//!     -> FlowLog `streaming_program_execution`
//!     -> decoded output rows via an output callback
//!
//! FlowLog itself is integer-only: every string (file path, AST node kind,
//! identifier, ...) is interned to an `i64` through [`string_table`], and the
//! same table is used to decode outputs.

pub mod engine;
pub mod string_table;

pub use dep2_plugin as plugin;
