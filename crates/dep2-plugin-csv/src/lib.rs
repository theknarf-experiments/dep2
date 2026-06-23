use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use notify::{EventKind, RecursiveMode, Watcher};

use dep2_plugin::{
    ColumnDef, DataProvider, DataSchema, DataSource, DataType, DataValue, Plugin, PluginContext,
    SourceRunner, SourceState, StreamOutput, StreamingDataProvider, StreamingDataSource, ValueSink,
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
// Config validation
// ---------------------------------------------------------------------------

/// Known config keys for the CSV plugin.
const KNOWN_KEYS: &[&str] = &["path", "delimiter", "types"];

/// Validate that only known config keys are present.
fn validate_config(config: &HashMap<String, String>) -> Result<(), String> {
    for key in config.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "csv: unknown config attribute '{}' (known: {})",
                key,
                KNOWN_KEYS.join(", ")
            ));
        }
    }
    Ok(())
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
        Some(":") => Ok(b':'),
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
        validate_config(config)?;

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
        validate_config(config)?;

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
            seeded: false,
            watch: None,
        }))
    }
}

struct CsvStreamingSource {
    schema: DataSchema,
    path: String,
    delimiter: u8,
    /// Whether the initial contents have been pushed yet.
    seeded: bool,
    /// File-watch state, created lazily after the seed.
    watch: Option<CsvWatch>,
}

/// Live-edit watch state for a CSV source (set up after the seed).
struct CsvWatch {
    current: HashMap<Vec<String>, usize>,
    notify_rx: std::sync::mpsc::Receiver<notify::Event>,
    canonical_path: PathBuf,
    /// Held to keep the watcher alive.
    _watcher: notify::RecommendedWatcher,
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

impl CsvStreamingSource {
    /// Push the whole multiset to `sink` with the given diff (each row `count`×).
    fn push_multiset(
        &self,
        sink: &mut dyn ValueSink,
        ms: &HashMap<Vec<String>, usize>,
        diff: isize,
    ) {
        for (row, count) in ms {
            let values = row_to_values(row, &self.schema);
            for _ in 0..*count {
                sink.push("", &values, diff);
            }
        }
    }
}

impl StreamingDataSource for CsvStreamingSource {
    fn outputs(&self) -> Vec<StreamOutput> {
        vec![StreamOutput {
            relation: String::new(),
            schema: self.schema.clone(),
        }]
    }

    fn build(self: Box<Self>) -> Box<dyn SourceRunner> {
        self
    }
}

impl SourceRunner for CsvStreamingSource {
    fn step(&mut self, sink: &mut dyn ValueSink, shutdown: &AtomicBool) -> SourceState {
        if shutdown.load(Ordering::Relaxed) {
            return SourceState::Idle;
        }

        // 1. Seed: read the whole CSV once and push every row, then arm the watcher.
        if !self.seeded {
            self.seeded = true;
            let current = read_csv_multiset(&self.path, self.delimiter).unwrap_or_else(|e| {
                eprintln!("csv streaming: failed initial read: {}", e);
                HashMap::new()
            });
            self.push_multiset(sink, &current, 1);

            // Arm a file watcher (on the parent dir — editors replace atomically).
            let (notify_tx, notify_rx) = std::sync::mpsc::channel();
            if let Ok(mut watcher) =
                notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                    if let Ok(event) = res {
                        let _ = notify_tx.send(event);
                    }
                })
            {
                let watch_path = Path::new(&self.path);
                let watch_target = watch_path.parent().unwrap_or(watch_path);
                if watcher
                    .watch(watch_target, RecursiveMode::NonRecursive)
                    .is_ok()
                {
                    let canonical_path = std::fs::canonicalize(&self.path)
                        .unwrap_or_else(|_| watch_path.to_path_buf());
                    self.watch = Some(CsvWatch {
                        current,
                        notify_rx,
                        canonical_path,
                        _watcher: watcher,
                    });
                }
            }
            return SourceState::Pending;
        }

        // 2. Watch: drain pending OS events non-blocking; re-read + diff on change.
        if self.watch.is_none() {
            return SourceState::Idle;
        }
        let mut relevant = false;
        {
            let watch = self.watch.as_ref().unwrap();
            while let Ok(event) = watch.notify_rx.try_recv() {
                if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
                    && event.paths.iter().any(|p| {
                        std::fs::canonicalize(p)
                            .map(|cp| cp == watch.canonical_path)
                            .unwrap_or(false)
                    })
                {
                    relevant = true;
                }
            }
        }
        if !relevant {
            return SourceState::Idle;
        }

        // Re-read; if the file is mid-write, skip this round (a later event retries).
        let new = match read_csv_multiset(&self.path, self.delimiter) {
            Ok(ms) => ms,
            Err(_) => return SourceState::Idle,
        };
        let old = std::mem::take(&mut self.watch.as_mut().unwrap().current);
        for (row, &old_count) in &old {
            let new_count = new.get(row).copied().unwrap_or(0);
            if old_count > new_count {
                let values = row_to_values(row, &self.schema);
                for _ in 0..(old_count - new_count) {
                    sink.push("", &values, -1);
                }
            }
        }
        for (row, &new_count) in &new {
            let old_count = old.get(row).copied().unwrap_or(0);
            if new_count > old_count {
                let values = row_to_values(row, &self.schema);
                for _ in 0..(new_count - old_count) {
                    sink.push("", &values, 1);
                }
            }
        }
        self.watch.as_mut().unwrap().current = new;
        SourceState::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_delimiter_default() {
        let config = HashMap::new();
        assert_eq!(parse_delimiter(&config).unwrap(), b',');
    }

    #[test]
    fn parse_delimiter_tab() {
        let mut config = HashMap::new();
        config.insert("delimiter".to_string(), "tab".to_string());
        assert_eq!(parse_delimiter(&config).unwrap(), b'\t');

        let mut config = HashMap::new();
        config.insert("delimiter".to_string(), "\\t".to_string());
        assert_eq!(parse_delimiter(&config).unwrap(), b'\t');
    }

    #[test]
    fn parse_delimiter_pipe() {
        let mut config = HashMap::new();
        config.insert("delimiter".to_string(), "|".to_string());
        assert_eq!(parse_delimiter(&config).unwrap(), b'|');
    }

    #[test]
    fn parse_delimiter_semicolon() {
        let mut config = HashMap::new();
        config.insert("delimiter".to_string(), ";".to_string());
        assert_eq!(parse_delimiter(&config).unwrap(), b';');
    }

    #[test]
    fn parse_delimiter_colon() {
        let mut config = HashMap::new();
        config.insert("delimiter".to_string(), ":".to_string());
        assert_eq!(parse_delimiter(&config).unwrap(), b':');
    }

    #[test]
    fn parse_delimiter_invalid() {
        let mut config = HashMap::new();
        config.insert("delimiter".to_string(), "abc".to_string());
        assert!(parse_delimiter(&config).is_err());
    }

    #[test]
    fn parse_field_empty_is_null() {
        assert_eq!(parse_field("", &DataType::String), DataValue::Null);
        assert_eq!(parse_field("", &DataType::Integer), DataValue::Null);
        assert_eq!(parse_field("", &DataType::Float), DataValue::Null);
    }

    #[test]
    fn parse_field_integer() {
        assert_eq!(
            parse_field("42", &DataType::Integer),
            DataValue::Integer(42)
        );
        assert_eq!(
            parse_field("-1", &DataType::Integer),
            DataValue::Integer(-1)
        );
        assert_eq!(parse_field("abc", &DataType::Integer), DataValue::Null);
    }

    #[test]
    fn parse_field_float() {
        assert_eq!(
            parse_field("3.14", &DataType::Float),
            DataValue::Float(3.14)
        );
        assert_eq!(parse_field("abc", &DataType::Float), DataValue::Null);
    }

    #[test]
    fn parse_field_string() {
        assert_eq!(
            parse_field("hello", &DataType::String),
            DataValue::String("hello".to_string())
        );
    }

    #[test]
    fn infer_types_basic() {
        let headers = vec!["name".to_string(), "age".to_string(), "score".to_string()];
        let mut record = csv::StringRecord::new();
        record.push_field("alice");
        record.push_field("30");
        record.push_field("9.5");
        let types = infer_types(&headers, &record);
        assert_eq!(
            types,
            vec![DataType::String, DataType::Integer, DataType::Float]
        );
    }

    #[test]
    fn parse_explicit_types_ok() {
        let mut config = HashMap::new();
        config.insert("types".to_string(), "integer,string".to_string());
        let types = parse_explicit_types(&config, 2).unwrap().unwrap();
        assert_eq!(types, vec![DataType::Integer, DataType::String]);
    }

    #[test]
    fn parse_explicit_types_mismatch() {
        let mut config = HashMap::new();
        config.insert("types".to_string(), "integer".to_string());
        assert!(parse_explicit_types(&config, 2).is_err());
    }

    #[test]
    fn parse_explicit_types_none() {
        let config = HashMap::new();
        assert!(parse_explicit_types(&config, 2).unwrap().is_none());
    }

    #[test]
    fn build_schema_basic() {
        let headers = vec!["a".to_string(), "b".to_string()];
        let types = vec![DataType::Integer, DataType::String];
        let schema = build_schema(&headers, &types);
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.columns[0].name, "a");
        assert_eq!(schema.columns[0].data_type, DataType::Integer);
    }

    #[test]
    fn row_to_values_basic() {
        let schema = DataSchema {
            columns: vec![
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::String,
                },
                ColumnDef {
                    name: "age".to_string(),
                    data_type: DataType::Integer,
                },
            ],
        };
        let row = vec!["alice".to_string(), "30".to_string()];
        let values = row_to_values(&row, &schema);
        assert_eq!(
            values,
            vec![
                DataValue::String("alice".to_string()),
                DataValue::Integer(30),
            ]
        );
    }

    #[test]
    fn validate_config_rejects_unknown() {
        let mut config = HashMap::new();
        config.insert("path".to_string(), "/tmp/test.csv".to_string());
        config.insert("bad_key".to_string(), "val".to_string());
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn validate_config_accepts_known() {
        let mut config = HashMap::new();
        config.insert("path".to_string(), "/tmp/test.csv".to_string());
        config.insert("delimiter".to_string(), ",".to_string());
        config.insert("types".to_string(), "string".to_string());
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn batch_open_missing_path() {
        let provider = CsvBatchProvider;
        let config = HashMap::new();
        assert!(provider.open(&config).is_err());
    }

    #[test]
    fn batch_open_nonexistent_file() {
        let provider = CsvBatchProvider;
        let mut config = HashMap::new();
        config.insert(
            "path".to_string(),
            "/tmp/nonexistent_dep2_test.csv".to_string(),
        );
        assert!(provider.open(&config).is_err());
    }

    #[test]
    fn batch_open_real_csv() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.csv");
        std::fs::write(&path, "name,age\nalice,30\nbob,25\n").unwrap();

        let provider = CsvBatchProvider;
        let mut config = HashMap::new();
        config.insert("path".to_string(), path.to_string_lossy().to_string());

        let source = provider.open(&config).unwrap();
        assert_eq!(source.schema().columns.len(), 2);
        assert_eq!(source.schema().columns[0].name, "name");
        assert_eq!(source.schema().columns[1].name, "age");
        assert_eq!(source.schema().columns[1].data_type, DataType::Integer);

        let rows = source.fetch_all().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], DataValue::String("alice".to_string()));
        assert_eq!(rows[0][1], DataValue::Integer(30));
    }

    #[test]
    fn batch_open_with_explicit_types() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.csv");
        std::fs::write(&path, "a,b\n1,2\n3,4\n").unwrap();

        let provider = CsvBatchProvider;
        let mut config = HashMap::new();
        config.insert("path".to_string(), path.to_string_lossy().to_string());
        config.insert("types".to_string(), "string,float".to_string());

        let source = provider.open(&config).unwrap();
        assert_eq!(source.schema().columns[0].data_type, DataType::String);
        assert_eq!(source.schema().columns[1].data_type, DataType::Float);

        let rows = source.fetch_all().unwrap();
        assert_eq!(rows[0][0], DataValue::String("1".to_string()));
        assert_eq!(rows[0][1], DataValue::Float(2.0));
    }

    #[test]
    fn batch_open_empty_csv() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.csv");
        std::fs::write(&path, "a,b\n").unwrap();

        let provider = CsvBatchProvider;
        let mut config = HashMap::new();
        config.insert("path".to_string(), path.to_string_lossy().to_string());

        let source = provider.open(&config).unwrap();
        assert_eq!(source.schema().columns.len(), 2);
        let rows = source.fetch_all().unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn batch_open_with_nulls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nulls.csv");
        std::fs::write(&path, "name,age\nalice,\n,25\n").unwrap();

        let provider = CsvBatchProvider;
        let mut config = HashMap::new();
        config.insert("path".to_string(), path.to_string_lossy().to_string());

        let source = provider.open(&config).unwrap();
        let rows = source.fetch_all().unwrap();
        assert_eq!(rows[0][1], DataValue::Null); // empty age
        assert_eq!(rows[1][0], DataValue::Null); // empty name
    }

    #[test]
    fn batch_pipe_delimiter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipe.csv");
        std::fs::write(&path, "a|b\n1|2\n").unwrap();

        let provider = CsvBatchProvider;
        let mut config = HashMap::new();
        config.insert("path".to_string(), path.to_string_lossy().to_string());
        config.insert("delimiter".to_string(), "|".to_string());

        let source = provider.open(&config).unwrap();
        let rows = source.fetch_all().unwrap();
        assert_eq!(rows[0][0], DataValue::Integer(1));
        assert_eq!(rows[0][1], DataValue::Integer(2));
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Any i64 formatted to text parses back to the same Integer.
            #[test]
            fn parse_field_integer_roundtrip(v in any::<i64>()) {
                prop_assert_eq!(parse_field(&v.to_string(), &DataType::Integer), DataValue::Integer(v));
            }

            /// Non-empty, non-numeric text under an Integer column becomes Null
            /// (never panics, never silently mis-parses).
            #[test]
            fn parse_field_integer_nonnumeric_is_null(s in "[a-zA-Z][a-zA-Z0-9]{0,8}") {
                prop_assert_eq!(parse_field(&s, &DataType::Integer), DataValue::Null);
            }

            /// String column is the identity on non-empty input; empty is Null.
            #[test]
            fn parse_field_string_roundtrip(s in ".{0,16}") {
                let expected = if s.is_empty() {
                    DataValue::Null
                } else {
                    DataValue::String(s.clone())
                };
                prop_assert_eq!(parse_field(&s, &DataType::String), expected);
            }

            /// Explicit-types parsing succeeds iff the count matches the columns.
            #[test]
            fn explicit_types_count_must_match(
                tys in prop::collection::vec(prop::sample::select(vec!["integer", "float", "string"]), 1..6),
                ncols in 1usize..6,
            ) {
                let mut config = HashMap::new();
                config.insert("types".to_string(), tys.join(","));
                let res = parse_explicit_types(&config, ncols);
                if tys.len() == ncols {
                    let got = res.unwrap().unwrap();
                    prop_assert_eq!(got.len(), ncols);
                } else {
                    prop_assert!(res.is_err());
                }
            }
        }
    }
}
