use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};

use dbflow_plugin::{
    crossbeam_channel, ColumnDef, DataProvider, DataSchema, DataSource, DataType, DataValue,
    Plugin, PluginContext, StreamingDataProvider, StreamingDataSource, StreamingUpdate,
};

pub struct CsvPlugin;

impl Plugin for CsvPlugin {
    fn name(&self) -> &str {
        "csv"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
        ctx.register_data_provider(Box::new(CsvBatchProvider));
        ctx.register_streaming_data_provider(Box::new(CsvStreamingProvider));
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Parse the optional `delimiter` config. Defaults to `,`.
fn parse_delimiter(config: &HashMap<String, String>) -> Result<u8, String> {
    match config.get("delimiter").map(|s| s.as_str()) {
        None | Some(",") => Ok(b','),
        Some("\\t") | Some("tab") => Ok(b'\t'),
        Some("|") => Ok(b'|'),
        Some(";") => Ok(b';'),
        Some(d) if d.len() == 1 => Ok(d.as_bytes()[0]),
        Some(d) => Err(format!(
            "invalid delimiter '{}': must be a single character, 'tab', or '\\t'",
            d
        )),
    }
}

/// Build a csv::ReaderBuilder with the given delimiter.
fn csv_reader_builder(delimiter: u8) -> csv::ReaderBuilder {
    let mut builder = csv::ReaderBuilder::new();
    builder.delimiter(delimiter);
    builder
}

/// Infer column types from the first data row: i64 first, then f64, else String.
fn infer_types(headers: &[String], record: &csv::StringRecord) -> Vec<DataType> {
    let mut col_types: Vec<DataType> = headers.iter().map(|_| DataType::String).collect();
    for (i, field) in record.iter().enumerate() {
        if i < col_types.len() {
            if field.parse::<i64>().is_ok() {
                col_types[i] = DataType::Integer;
            } else if field.parse::<f64>().is_ok() {
                col_types[i] = DataType::Float;
            }
        }
    }
    col_types
}

/// Parse explicit column types from config (comma-separated: integer, float, string).
/// Returns None if no `types` config is present.
fn parse_explicit_types(
    config: &HashMap<String, String>,
    num_columns: usize,
) -> Result<Option<Vec<DataType>>, String> {
    match config.get("types") {
        Some(types_str) => {
            let types: Vec<DataType> = types_str
                .split(',')
                .map(|s| match s.trim() {
                    "integer" => DataType::Integer,
                    "float" => DataType::Float,
                    _ => DataType::String,
                })
                .collect();
            if types.len() != num_columns {
                return Err(format!(
                    "csv types count ({}) does not match columns count ({})",
                    types.len(),
                    num_columns
                ));
            }
            Ok(Some(types))
        }
        None => Ok(None),
    }
}

/// Build a DataSchema from headers and column types.
fn build_schema(headers: &[String], col_types: &[DataType]) -> DataSchema {
    DataSchema {
        columns: headers
            .iter()
            .zip(col_types.iter())
            .map(|(name, dt)| ColumnDef {
                name: name.clone(),
                data_type: dt.clone(),
            })
            .collect(),
    }
}

/// Parse a string field into a DataValue according to the column type.
/// Empty fields are treated as NULL. Parse failures for numeric types also yield NULL.
fn parse_field(s: &str, col_type: &DataType) -> DataValue {
    if s.is_empty() {
        return DataValue::Null;
    }
    match col_type {
        DataType::Integer => match s.parse::<i64>() {
            Ok(v) => DataValue::Integer(v),
            Err(_) => DataValue::Null,
        },
        DataType::Float => match s.parse::<f64>() {
            Ok(v) => DataValue::Float(v),
            Err(_) => DataValue::Null,
        },
        DataType::String => DataValue::String(s.to_string()),
    }
}

/// Convert a row of string fields to DataValues using the schema.
fn row_to_values(row: &[String], schema: &DataSchema) -> Vec<DataValue> {
    row.iter()
        .zip(schema.columns.iter())
        .map(|(s, col)| parse_field(s, &col.data_type))
        .collect()
}

// ---------------------------------------------------------------------------
// Batch data provider
// ---------------------------------------------------------------------------

struct CsvBatchProvider;

impl DataProvider for CsvBatchProvider {
    fn name(&self) -> &str {
        "csv"
    }

    fn open(&self, config: &HashMap<String, String>) -> Result<Box<dyn DataSource>, String> {
        let path = config
            .get("path")
            .ok_or("csv data provider requires 'path' config attribute")?
            .clone();

        let delimiter = parse_delimiter(config)?;

        let mut reader = csv_reader_builder(delimiter)
            .from_path(&path)
            .map_err(|e| format!("failed to open CSV '{}': {}", path, e))?;

        let headers: Vec<String> = reader
            .headers()
            .map_err(|e| format!("failed to read CSV headers: {}", e))?
            .iter()
            .map(|h| h.to_string())
            .collect();

        if headers.is_empty() {
            return Err("CSV file has no columns".to_string());
        }

        // Use explicit types if provided, otherwise infer from first data row.
        let explicit_types = parse_explicit_types(config, headers.len())?;

        let mut rows: Vec<Vec<DataValue>> = Vec::new();
        let mut col_types: Vec<DataType> = explicit_types
            .clone()
            .unwrap_or_else(|| headers.iter().map(|_| DataType::String).collect());
        let mut first = true;

        for result in reader.records() {
            let record = result.map_err(|e| format!("CSV parse error: {}", e))?;
            if first && explicit_types.is_none() {
                col_types = infer_types(&headers, &record);
                first = false;
            }
            let row: Vec<DataValue> = record
                .iter()
                .zip(col_types.iter())
                .map(|(s, dt)| parse_field(s, dt))
                .collect();
            rows.push(row);
        }

        let schema = build_schema(&headers, &col_types);
        Ok(Box::new(CsvBatchSource { schema, rows }))
    }
}

struct CsvBatchSource {
    schema: DataSchema,
    rows: Vec<Vec<DataValue>>,
}

impl DataSource for CsvBatchSource {
    fn schema(&self) -> &DataSchema {
        &self.schema
    }

    fn fetch_all(&self) -> Result<Vec<Vec<DataValue>>, String> {
        Ok(self.rows.clone())
    }
}

// ---------------------------------------------------------------------------
// Streaming data provider
// ---------------------------------------------------------------------------

struct CsvStreamingProvider;

impl StreamingDataProvider for CsvStreamingProvider {
    fn name(&self) -> &str {
        "csv"
    }

    fn open_stream(
        &self,
        config: &HashMap<String, String>,
    ) -> Result<Box<dyn StreamingDataSource>, String> {
        let path = config
            .get("path")
            .ok_or("csv streaming provider requires 'path' config attribute")?;

        let delimiter = parse_delimiter(config)?;

        // Read headers and first data row to infer column types.
        let mut reader = csv_reader_builder(delimiter)
            .from_path(path)
            .map_err(|e| format!("failed to open CSV '{}': {}", path, e))?;

        let headers: Vec<String> = reader
            .headers()
            .map_err(|e| format!("failed to read CSV headers: {}", e))?
            .iter()
            .map(|h| h.to_string())
            .collect();

        if headers.is_empty() {
            return Err("CSV file has no columns".to_string());
        }

        // Use explicit types if provided, otherwise infer from first data row.
        let explicit_types = parse_explicit_types(config, headers.len())?;
        let col_types = if let Some(types) = explicit_types {
            types
        } else {
            let mut col_types: Vec<DataType> = headers.iter().map(|_| DataType::String).collect();
            if let Some(Ok(record)) = reader.records().next() {
                col_types = infer_types(&headers, &record);
            }
            col_types
        };

        let schema = build_schema(&headers, &col_types);

        Ok(Box::new(CsvStreamingSource {
            schema,
            path: path.clone(),
            delimiter,
        }))
    }
}

struct CsvStreamingSource {
    schema: DataSchema,
    path: String,
    delimiter: u8,
}

/// Read a CSV file and return a multiset: row → count.
fn read_csv_multiset(path: &str, delimiter: u8) -> Result<HashMap<Vec<String>, usize>, String> {
    let mut reader = csv_reader_builder(delimiter)
        .from_path(path)
        .map_err(|e| format!("failed to open CSV '{}': {}", path, e))?;

    let mut multiset: HashMap<Vec<String>, usize> = HashMap::new();
    for result in reader.records() {
        let record = result.map_err(|e| format!("CSV parse error: {}", e))?;
        let row: Vec<String> = record.iter().map(|f| f.to_string()).collect();
        *multiset.entry(row).or_insert(0) += 1;
    }
    Ok(multiset)
}

impl StreamingDataSource for CsvStreamingSource {
    fn schema(&self) -> &DataSchema {
        &self.schema
    }

    fn run(
        self: Box<Self>,
        sender: crossbeam_channel::Sender<StreamingUpdate>,
        shutdown: Arc<AtomicBool>,
    ) {
        // 1. Read initial CSV contents
        let mut current = match read_csv_multiset(&self.path, self.delimiter) {
            Ok(ms) => ms,
            Err(e) => {
                eprintln!("csv streaming: failed initial read: {}", e);
                return;
            }
        };

        // Send all initial rows as inserts
        for (row, count) in &current {
            let values = row_to_values(row, &self.schema);
            for _ in 0..*count {
                if sender
                    .send(StreamingUpdate::Insert(values.clone()))
                    .is_err()
                {
                    return;
                }
            }
        }

        // 2. Set up file watcher
        let (notify_tx, notify_rx) = std::sync::mpsc::channel();
        let mut watcher =
            match notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = notify_tx.send(event);
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("csv streaming: failed to create file watcher: {}", e);
                    return;
                }
            };

        let watch_path = Path::new(&self.path);
        // Watch the parent directory since some editors replace files atomically
        let watch_target = watch_path.parent().unwrap_or(watch_path);
        if let Err(e) = watcher.watch(watch_target, RecursiveMode::NonRecursive) {
            eprintln!("csv streaming: failed to watch path: {}", e);
            return;
        }

        // 3. Watch loop
        let canonical_path =
            std::fs::canonicalize(&self.path).unwrap_or_else(|_| watch_path.to_path_buf());
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            match notify_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => {
                    // Only react to modify/create events on our file
                    match event.kind {
                        EventKind::Modify(_) | EventKind::Create(_) => {
                            // Check if the event is for our file
                            let is_our_file = event.paths.iter().any(|p| {
                                std::fs::canonicalize(p)
                                    .map(|cp| cp == canonical_path)
                                    .unwrap_or(false)
                            });
                            if !is_our_file {
                                continue;
                            }
                        }
                        _ => continue,
                    }

                    // Small delay to let writes complete
                    std::thread::sleep(Duration::from_millis(50));

                    // Re-read the CSV
                    let new = match read_csv_multiset(&self.path, self.delimiter) {
                        Ok(ms) => ms,
                        Err(_) => continue, // file might be mid-write
                    };

                    // Compute diff: deletions and insertions
                    // Deletions: rows in current but not (or fewer) in new
                    for (row, &old_count) in &current {
                        let new_count = new.get(row).copied().unwrap_or(0);
                        if old_count > new_count {
                            let values = row_to_values(row, &self.schema);
                            for _ in 0..(old_count - new_count) {
                                if sender
                                    .send(StreamingUpdate::Delete(values.clone()))
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }

                    // Insertions: rows in new but not (or fewer) in current
                    for (row, &new_count) in &new {
                        let old_count = current.get(row).copied().unwrap_or(0);
                        if new_count > old_count {
                            let values = row_to_values(row, &self.schema);
                            for _ in 0..(new_count - old_count) {
                                if sender
                                    .send(StreamingUpdate::Insert(values.clone()))
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }

                    current = new;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }
}
