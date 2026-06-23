use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
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

/// An update from a streaming data source.
pub enum StreamingUpdate {
    /// Insert a row into the source's first (default) output.
    Insert(Vec<DataValue>),
    /// Retract a row from the source's first (default) output.
    Delete(Vec<DataValue>),
    /// Insert a row into the named output relation.
    InsertInto(String, Vec<DataValue>),
    /// Retract a row from the named output relation.
    DeleteInto(String, Vec<DataValue>),
    /// A batch of `(row, diff)` updates for one named output relation. Lets a
    /// source amortize channel traffic — one send for a whole file's rows instead
    /// of one per row (a few sends per file vs. millions across a large repo).
    BatchInto(String, Vec<(Vec<DataValue>, isize)>),
    /// The stream has ended.
    Eof,
}

/// A streaming data source that runs indefinitely, sending rows through a channel.
pub trait StreamingDataSource: Send {
    /// The output relations this source produces. Most sources return one entry;
    /// multi-output sources (e.g. treesitter) return several.
    fn outputs(&self) -> Vec<StreamOutput>;

    /// Tell the source which of its outputs the program actually consumes, so a
    /// multi-output source can skip building and sending rows for relations no
    /// rule reads (e.g. treesitter's ast_span/ast_line when only ast_node is
    /// used). Called once after `outputs()` and before `run`. The default is a
    /// no-op — single-output sources need not implement it.
    fn set_wanted(&mut self, _wanted: &HashSet<String>) {}

    /// Run the source on the calling thread. Send rows through `sender`.
    /// Return when exhausted or when `shutdown` is set to true.
    fn run(
        self: Box<Self>,
        sender: crossbeam_channel::Sender<StreamingUpdate>,
        shutdown: Arc<AtomicBool>,
    );
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
