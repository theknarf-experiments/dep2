use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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
    /// A string backed by a shared `Arc<str>`. Equivalent to `String` for the
    /// engine (both intern the same way), but lets a source that holds the same
    /// string in many rows (e.g. a file path or a syntax-node kind repeated across
    /// every node) push it as a refcount clone instead of a fresh allocation.
    Str(Arc<str>),
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
/// living on the timely worker thread, so a `Source` runs *inside* the worker and
/// feeds the dataflow's input directly: there is no intermediate channel or enum.
/// The engine adapts this to its `i64`-encoded input (interning string values)
/// behind the trait object.
pub trait ValueSink {
    /// Push one row into `relation`'s input. `diff` is +1 (insert) or -1 (retract).
    fn push(&mut self, relation: &str, row: &[DataValue], diff: isize);
}

/// A streaming data source the engine holds: cloneable configuration that knows
/// its outputs and enumerates its *work units*, and opens a per-worker `Source`
/// that ingests units. It is `Send + Sync` (the engine keeps it across worker
/// threads).
///
/// A "unit" is the source's natural unit of work, identified by a string: for
/// treesitter a unit is a file path; a single-file source (csv, fs) has one unit.
/// The **engine** — not the plugin — owns all orchestration: it shards units
/// across timely workers, decides how many to ingest per epoch (batching), and
/// drives the dataflow. A plugin never sees worker counts, batch sizes, or epochs,
/// so the orchestrator is free to change the flow without touching plugins.
pub trait StreamingDataSource: Send + Sync {
    /// The output relations this source produces. Most sources return one entry;
    /// multi-output sources (e.g. treesitter) return several.
    fn outputs(&self) -> Vec<StreamOutput>;

    /// Tell the source which of its outputs the program actually consumes, so a
    /// multi-output source can skip building rows for relations no rule reads
    /// (e.g. treesitter's ast_span/ast_line when only ast_node is used). Called
    /// once after `outputs()` and before `seed_units`/`open`. Default: no-op.
    fn set_wanted(&mut self, _wanted: &HashSet<String>) {}

    /// Enumerate the initial set of work units (e.g. the files to parse). Called
    /// once; the engine shards the result across workers. Cheap — no heavy work
    /// (e.g. parsing) here.
    fn seed_units(&self) -> Vec<String>;

    /// Open a per-worker source. Called once per timely worker, on that worker's
    /// thread, so the returned `Source` may hold non-`Send` state (e.g. a wasm
    /// parser). The engine only ever asks it to `ingest` this worker's shard.
    fn open(&self) -> Box<dyn Source>;
}

/// The per-worker half of a streaming source. The engine drives it; it knows
/// nothing about worker counts, sharding, batching, or epochs — it just ingests
/// the units the engine hands it and reports what has changed.
pub trait Source {
    /// Reconcile one unit against the dataflow's current input, pushing the delta
    /// into `sink` (insert new rows, retract rows for a unit that has changed or
    /// disappeared). The engine calls this only for this worker's shard of units,
    /// batching as it sees fit. Should be idempotent: ingesting an unchanged unit
    /// is a no-op.
    fn ingest(&mut self, unit: &str, sink: &mut dyn ValueSink);

    /// Non-blocking poll for units that have changed since the last call (live
    /// edits). The engine shards the result and re-`ingest`s the ones it owns.
    /// Default: a static source that never changes.
    fn poll_changes(&mut self) -> Vec<String> {
        Vec::new()
    }
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
