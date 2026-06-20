//! dep2-core: a thin, HCL-free engine that drives the FlowLog incremental
//! Datalog engine from streaming plugin data sources.
//!
//! The pipeline is:
//!   native `.dl` program  +  streaming source bindings
//!     -> FlowLog parses, executes, and decodes outputs
//!     -> decoded output rows via an output callback
//!
//! String and float support is an in-engine feature: FlowLog (the `reading`
//! crate's interner) encodes strings/floats to `i64` on input and decodes them
//! on output. dep2-core just feeds plugin values through that codec.

pub mod engine;

pub use dep2_plugin as plugin;
