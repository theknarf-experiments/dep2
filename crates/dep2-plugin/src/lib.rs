use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;

pub use crossbeam_channel;

/// A plugin that can extend the Dep2 engine.
pub trait Plugin: Send + Sync {
    /// The name of this plugin.
    fn name(&self) -> &str;

    /// Called during plugin registration to allow the plugin to set up its capabilities.
    fn setup(&self, ctx: &mut PluginContext);
}

// ---------------------------------------------------------------------------
// Data provider types
// ---------------------------------------------------------------------------

/// A value from an external data source.
#[derive(Debug, Clone, PartialEq)]
pub enum DataValue {
    String(String),
    Integer(i64),
    Float(f64),
    Bool(bool),
    Null,
}

/// A column type in a data source schema.
#[derive(Debug, Clone, PartialEq)]
pub enum DataType {
    String,
    Integer,
    Float,
}

/// A column definition: name + type.
#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
}

/// The schema of a data source (ordered list of columns).
#[derive(Debug, Clone)]
pub struct DataSchema {
    pub columns: Vec<ColumnDef>,
}

/// A handle to an open data source, created by `DataProvider::open()`.
pub trait DataSource: Send + Sync {
    /// Return the schema (column names and types) for this source.
    fn schema(&self) -> &DataSchema;

    /// Fetch all rows from the source.
    fn fetch_all(&self) -> Result<Vec<Vec<DataValue>>, String>;
}

/// A factory that creates `DataSource` instances from configuration.
/// Each plugin registers one or more `DataProvider`s during setup.
pub trait DataProvider: Send + Sync {
    /// The provider type name (e.g., "csv", "kafka", "postgres").
    fn name(&self) -> &str;

    /// Open a data source given configuration key-value pairs
    /// from the HCL data block's attributes.
    fn open(&self, config: &HashMap<String, String>) -> Result<Box<dyn DataSource>, String>;
}

// ---------------------------------------------------------------------------
// Streaming data provider types
// ---------------------------------------------------------------------------

/// One output relation produced by a streaming source.
pub struct StreamOutput {
    /// Relation this output feeds. Leave empty for a single-output source whose
    /// relation name is chosen by the engine binding (e.g. csv, fs). Multi-output
    /// sources (e.g. treesitter) give each output a concrete relation name.
    pub relation: String,
    /// Column schema for this output.
    pub schema: DataSchema,
}

/// A sink a source pushes rows into. Backed by a differential `InputSession`
/// living on the timely worker thread, so a `SourceRunner` runs *inside* the
/// worker and feeds the dataflow's input directly: there is no intermediate
/// channel or enum. The engine adapts this to its `i64`-encoded input (interning
/// string values) behind the trait object.
pub trait ValueSink {
    /// Push one row into `relation`'s input. `diff` is +1 (insert) or -1 (retract).
    fn push(&mut self, relation: &str, row: &[DataValue], diff: isize);
}

/// Result of one cooperative `SourceRunner::step`.
pub enum SourceState {
    /// Did a bounded unit of work and has more seed work ready right now: the
    /// worker should keep stepping the source promptly.
    Pending,
    /// Caught up: the initial seed is exhausted and there are no pending changes.
    /// The worker can sleep briefly before polling again (e.g. for file edits).
    Idle,
}

/// A streaming data source the engine holds: it knows its outputs and builds the
/// running source. It is `Send` (the engine hands it to a worker thread), but the
/// running source it builds need not be, so the runner may keep non-`Send` state
/// (e.g. a wasm parser) that lives on the worker thread.
pub trait StreamingDataSource: Send {
    /// The output relations this source produces. Most sources return one entry;
    /// multi-output sources (e.g. treesitter) return several.
    fn outputs(&self) -> Vec<StreamOutput>;

    /// Tell the source which of its outputs the program actually consumes, so a
    /// multi-output source can skip building rows for relations no rule reads
    /// (e.g. treesitter's ast_span/ast_line when only ast_node is used). Called
    /// once after `outputs()` and before `build`. Default: no-op.
    fn set_wanted(&mut self, _wanted: &HashSet<String>) {}

    /// Build the running source. Called once, on the timely worker thread that
    /// will feed it, so the returned runner may hold non-`Send` state.
    fn build(self: Box<Self>) -> Box<dyn SourceRunner>;
}

/// The running half of a streaming source, driven cooperatively by a timely
/// worker. Instead of running on its own thread and sending updates over a
/// channel, it is *stepped* by the worker: each `step` does a bounded amount of
/// work (e.g. parse one file) and pushes the resulting rows into the `ValueSink`.
/// The worker advances an epoch and steps the dataflow between calls, so results
/// stream out live as the source loads (the engine's incremental contract).
/// Need not be `Send`: it is built and stepped on the same worker thread.
pub trait SourceRunner {
    /// Do a bounded unit of work, pushing any produced rows into `sink`. Called
    /// repeatedly by the worker. Return `Pending` while there is more seed work to
    /// do now, `Idle` once caught up. `shutdown` signals the engine is stopping.
    fn step(&mut self, sink: &mut dyn ValueSink, shutdown: &AtomicBool) -> SourceState;
}

/// A factory that creates `StreamingDataSource` instances from configuration.
pub trait StreamingDataProvider: Send + Sync {
    /// The provider type name (e.g., "kafka").
    fn name(&self) -> &str;

    /// Open a streaming data source given configuration key-value pairs.
    fn open_stream(
        &self,
        config: &HashMap<String, String>,
    ) -> Result<Box<dyn StreamingDataSource>, String>;
}

// ---------------------------------------------------------------------------
// Plugin context
// ---------------------------------------------------------------------------

/// Context passed to plugins during setup, allowing them to register capabilities.
pub struct PluginContext {
    registered: Vec<String>,
    data_providers: HashMap<String, Box<dyn DataProvider>>,
    streaming_data_providers: HashMap<String, Box<dyn StreamingDataProvider>>,
}

impl PluginContext {
    pub fn new() -> Self {
        Self {
            registered: Vec::new(),
            data_providers: HashMap::new(),
            streaming_data_providers: HashMap::new(),
        }
    }

    /// Register a plugin by name.
    pub fn register(&mut self, name: &str) {
        self.registered.push(name.to_string());
    }

    /// Return the list of registered plugin names.
    pub fn registered_plugins(&self) -> &[String] {
        &self.registered
    }

    /// Register a data provider. The provider's `name()` is used as the lookup key.
    pub fn register_data_provider(&mut self, provider: Box<dyn DataProvider>) {
        let name = provider.name().to_string();
        self.data_providers.insert(name, provider);
    }

    /// Look up a data provider by type name.
    pub fn get_data_provider(&self, name: &str) -> Option<&dyn DataProvider> {
        self.data_providers.get(name).map(|p| p.as_ref())
    }

    /// Register a streaming data provider.
    pub fn register_streaming_data_provider(&mut self, provider: Box<dyn StreamingDataProvider>) {
        let name = provider.name().to_string();
        self.streaming_data_providers.insert(name, provider);
    }

    /// Look up a streaming data provider by type name.
    pub fn get_streaming_data_provider(&self, name: &str) -> Option<&dyn StreamingDataProvider> {
        self.streaming_data_providers.get(name).map(|p| p.as_ref())
    }
}

impl Default for PluginContext {
    fn default() -> Self {
        Self::new()
    }
}
