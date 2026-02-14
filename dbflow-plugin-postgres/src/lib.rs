use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use postgres::fallible_iterator::FallibleIterator;
use postgres::types::Type;
use postgres::{Client, NoTls};

use dbflow_plugin::{
    ColumnDef, DataProvider, DataSchema, DataSource, DataType, DataValue, Plugin, PluginContext,
    StreamingDataProvider, StreamingDataSource, StreamingUpdate,
};

pub struct PostgresPlugin;

impl Plugin for PostgresPlugin {
    fn name(&self) -> &str {
        "postgres"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
        ctx.register_data_provider(Box::new(PostgresBatchProvider));
        ctx.register_streaming_data_provider(Box::new(PostgresStreamingProvider));
    }
}

/// The payload format for NOTIFY messages.
#[derive(Clone, Copy, PartialEq)]
enum Format {
    /// Raw text: single "value" column.
    Raw,
    /// JSON: parse the payload as a JSON object and extract named columns.
    Json,
}

// ---------------------------------------------------------------------------
// Batch data provider — runs a SQL query and returns results
// ---------------------------------------------------------------------------

struct PostgresBatchProvider;

impl DataProvider for PostgresBatchProvider {
    fn name(&self) -> &str {
        "postgres"
    }

    fn open(&self, config: &HashMap<String, String>) -> Result<Box<dyn DataSource>, String> {
        let connection = config
            .get("connection")
            .ok_or_else(|| "postgres data block missing 'connection' config".to_string())?;

        let query = config
            .get("query")
            .ok_or_else(|| "postgres batch provider requires 'query' config attribute".to_string())?;

        let mut client = Client::connect(connection, NoTls)
            .map_err(|e| format!("failed to connect to PostgreSQL: {}", e))?;

        let rows = client
            .query(query.as_str(), &[])
            .map_err(|e| format!("PostgreSQL query failed: {}", e))?;

        if rows.is_empty() {
            // Return empty result set. Need columns from config or return empty schema.
            let schema = parse_schema_from_config(config)?;
            return Ok(Box::new(PostgresBatchSource {
                schema,
                rows: Vec::new(),
            }));
        }

        // Infer schema from result columns.
        let columns = rows[0].columns();
        let schema = DataSchema {
            columns: columns
                .iter()
                .map(|col| {
                    let data_type = pg_type_to_data_type(col.type_());
                    ColumnDef {
                        name: col.name().to_string(),
                        data_type,
                    }
                })
                .collect(),
        };

        // Override types if explicitly provided.
        let schema = apply_type_overrides(schema, config)?;

        let mut result_rows = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut values = Vec::with_capacity(schema.columns.len());
            for (i, col_def) in schema.columns.iter().enumerate() {
                let val = extract_pg_value(row, i, &col_def.data_type);
                values.push(val);
            }
            result_rows.push(values);
        }

        Ok(Box::new(PostgresBatchSource {
            schema,
            rows: result_rows,
        }))
    }
}

/// Map PostgreSQL type OIDs to DataType.
fn pg_type_to_data_type(pg_type: &Type) -> DataType {
    match *pg_type {
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => DataType::Integer,
        Type::FLOAT4 | Type::FLOAT8 | Type::NUMERIC => DataType::Float,
        _ => DataType::String,
    }
}

/// Extract a value from a PostgreSQL row by column index.
/// SQL NULL values are returned as `DataValue::Null`.
fn extract_pg_value(row: &postgres::Row, idx: usize, data_type: &DataType) -> DataValue {
    // Check for SQL NULL first using Option-based extraction.
    match data_type {
        DataType::Integer => {
            if let Ok(v) = row.try_get::<_, Option<i64>>(idx) {
                match v {
                    Some(val) => DataValue::Integer(val),
                    None => DataValue::Null,
                }
            } else if let Ok(v) = row.try_get::<_, Option<i32>>(idx) {
                match v {
                    Some(val) => DataValue::Integer(val as i64),
                    None => DataValue::Null,
                }
            } else if let Ok(v) = row.try_get::<_, Option<i16>>(idx) {
                match v {
                    Some(val) => DataValue::Integer(val as i64),
                    None => DataValue::Null,
                }
            } else {
                DataValue::Null
            }
        }
        DataType::Float => {
            if let Ok(v) = row.try_get::<_, Option<f64>>(idx) {
                match v {
                    Some(val) => DataValue::Float(val),
                    None => DataValue::Null,
                }
            } else if let Ok(v) = row.try_get::<_, Option<f32>>(idx) {
                match v {
                    Some(val) => DataValue::Float(val as f64),
                    None => DataValue::Null,
                }
            } else if let Ok(v) = row.try_get::<_, Option<i64>>(idx) {
                match v {
                    Some(val) => DataValue::Float(val as f64),
                    None => DataValue::Null,
                }
            } else {
                DataValue::Null
            }
        }
        DataType::String => {
            if let Ok(v) = row.try_get::<_, Option<String>>(idx) {
                match v {
                    Some(val) => DataValue::String(val),
                    None => DataValue::Null,
                }
            } else if let Ok(v) = row.try_get::<_, Option<i64>>(idx) {
                match v {
                    Some(val) => DataValue::String(val.to_string()),
                    None => DataValue::Null,
                }
            } else if let Ok(v) = row.try_get::<_, Option<f64>>(idx) {
                match v {
                    Some(val) => DataValue::String(val.to_string()),
                    None => DataValue::Null,
                }
            } else if let Ok(v) = row.try_get::<_, Option<bool>>(idx) {
                match v {
                    Some(val) => DataValue::String(val.to_string()),
                    None => DataValue::Null,
                }
            } else {
                DataValue::Null
            }
        }
    }
}

/// Parse schema from config (columns + optional types).
fn parse_schema_from_config(config: &HashMap<String, String>) -> Result<DataSchema, String> {
    if let Some(columns_str) = config.get("columns") {
        let columns: Vec<String> = columns_str
            .split(',')
            .map(|s| s.trim().to_string())
            .collect();

        let types = parse_types(&columns, config.get("types"))?;

        Ok(DataSchema {
            columns: columns
                .iter()
                .zip(types.iter())
                .map(|(name, dt)| ColumnDef {
                    name: name.clone(),
                    data_type: dt.clone(),
                })
                .collect(),
        })
    } else {
        Ok(DataSchema {
            columns: Vec::new(),
        })
    }
}

/// Apply type overrides from config to an existing schema.
fn apply_type_overrides(
    mut schema: DataSchema,
    config: &HashMap<String, String>,
) -> Result<DataSchema, String> {
    if let Some(types_str) = config.get("types") {
        let types: Vec<DataType> = types_str
            .split(',')
            .map(|s| match s.trim() {
                "integer" => DataType::Integer,
                "float" => DataType::Float,
                _ => DataType::String,
            })
            .collect();
        if types.len() != schema.columns.len() {
            return Err(format!(
                "postgres types count ({}) does not match columns count ({})",
                types.len(),
                schema.columns.len()
            ));
        }
        for (col, dt) in schema.columns.iter_mut().zip(types.iter()) {
            col.data_type = dt.clone();
        }
    }
    Ok(schema)
}

struct PostgresBatchSource {
    schema: DataSchema,
    rows: Vec<Vec<DataValue>>,
}

impl DataSource for PostgresBatchSource {
    fn schema(&self) -> &DataSchema {
        &self.schema
    }

    fn fetch_all(&self) -> Result<Vec<Vec<DataValue>>, String> {
        Ok(self.rows.clone())
    }
}

// ---------------------------------------------------------------------------
// Streaming data provider — LISTEN/NOTIFY
// ---------------------------------------------------------------------------

/// Factory that creates PostgreSQL LISTEN/NOTIFY streaming sources from HCL config.
struct PostgresStreamingProvider;

impl StreamingDataProvider for PostgresStreamingProvider {
    fn name(&self) -> &str {
        "postgres"
    }

    fn open_stream(
        &self,
        config: &HashMap<String, String>,
    ) -> Result<Box<dyn StreamingDataSource>, String> {
        let connection = config
            .get("connection")
            .ok_or_else(|| "postgres data block missing 'connection' config".to_string())?
            .clone();
        let channel = config
            .get("channel")
            .ok_or_else(|| "postgres data block missing 'channel' config".to_string())?
            .clone();

        let format = match config.get("format").map(|s| s.as_str()) {
            Some("json") => Format::Json,
            Some("raw") | None => Format::Raw,
            Some(other) => {
                return Err(format!(
                    "unknown postgres format '{}': expected 'raw' or 'json'",
                    other
                ))
            }
        };

        let schema = match format {
            Format::Raw => DataSchema {
                columns: vec![ColumnDef {
                    name: "value".to_string(),
                    data_type: DataType::String,
                }],
            },
            Format::Json => {
                let columns_str = config.get("columns").ok_or_else(|| {
                    "postgres format 'json' requires 'columns' config attribute".to_string()
                })?;
                let columns: Vec<String> =
                    columns_str.split(',').map(|s| s.trim().to_string()).collect();

                let types = parse_types(&columns, config.get("types"))?;

                DataSchema {
                    columns: columns
                        .iter()
                        .zip(types.iter())
                        .map(|(name, dt)| ColumnDef {
                            name: name.clone(),
                            data_type: dt.clone(),
                        })
                        .collect(),
                }
            }
        };

        Ok(Box::new(PostgresStreamingSource {
            connection,
            channel,
            format,
            schema,
        }))
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Parse column types: comma-separated type names (integer, float, string).
fn parse_types(
    cols: &[String],
    types_str: Option<&String>,
) -> Result<Vec<DataType>, String> {
    if let Some(ts) = types_str {
        let types: Vec<DataType> = ts
            .split(',')
            .map(|s| match s.trim() {
                "integer" => DataType::Integer,
                "float" => DataType::Float,
                _ => DataType::String,
            })
            .collect();
        if types.len() != cols.len() {
            return Err(format!(
                "postgres columns count ({}) does not match types count ({})",
                cols.len(),
                types.len()
            ));
        }
        Ok(types)
    } else {
        Ok(cols.iter().map(|_| DataType::String).collect())
    }
}

/// A streaming PostgreSQL source that uses LISTEN/NOTIFY to receive notifications.
struct PostgresStreamingSource {
    connection: String,
    channel: String,
    format: Format,
    schema: DataSchema,
}

/// Wraps a channel name in double quotes for safe use in SQL LISTEN statements.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Extract a field from a JSON object map, coercing to the expected DataType.
/// JSON null or missing fields yield `DataValue::Null`.
fn extract_json_field(
    map: &serde_json::Map<String, serde_json::Value>,
    name: &str,
    data_type: &DataType,
) -> DataValue {
    match map.get(name) {
        Some(serde_json::Value::String(s)) => match data_type {
            DataType::Integer => match s.parse::<i64>() {
                Ok(v) => DataValue::Integer(v),
                Err(_) => DataValue::Null,
            },
            DataType::Float => match s.parse::<f64>() {
                Ok(v) => DataValue::Float(v),
                Err(_) => DataValue::Null,
            },
            DataType::String => DataValue::String(s.clone()),
        },
        Some(serde_json::Value::Number(n)) => match data_type {
            DataType::Integer => DataValue::Integer(n.as_i64().unwrap_or(0)),
            DataType::Float => DataValue::Float(n.as_f64().unwrap_or(0.0)),
            DataType::String => DataValue::String(n.to_string()),
        },
        Some(serde_json::Value::Bool(b)) => match data_type {
            DataType::Integer => DataValue::Integer(if *b { 1 } else { 0 }),
            DataType::Float => DataValue::Float(if *b { 1.0 } else { 0.0 }),
            DataType::String => DataValue::String(b.to_string()),
        },
        Some(serde_json::Value::Null) | None => DataValue::Null,
        _ => DataValue::Null,
    }
}

impl StreamingDataSource for PostgresStreamingSource {
    fn schema(&self) -> &DataSchema {
        &self.schema
    }

    fn run(
        self: Box<Self>,
        sender: dbflow_plugin::crossbeam_channel::Sender<StreamingUpdate>,
        shutdown: Arc<AtomicBool>,
    ) {
        let mut client = match Client::connect(&self.connection, NoTls) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("postgres: failed to connect: {}", e);
                let _ = sender.send(StreamingUpdate::Eof);
                return;
            }
        };

        let listen_sql = format!("LISTEN {}", quote_ident(&self.channel));
        if let Err(e) = client.execute(&listen_sql, &[]) {
            eprintln!("postgres: failed to execute LISTEN: {}", e);
            let _ = sender.send(StreamingUpdate::Eof);
            return;
        }

        while !shutdown.load(Ordering::Relaxed) {
            let mut notifications = client.notifications();
            let mut iter = notifications.timeout_iter(Duration::from_millis(100));

            while let Ok(Some(notification)) = iter.next() {
                let payload = notification.payload().to_string();

                let values = match self.format {
                    Format::Raw => vec![DataValue::String(payload)],
                    Format::Json => {
                        match serde_json::from_str::<serde_json::Value>(&payload) {
                            Ok(serde_json::Value::Object(map)) => self
                                .schema
                                .columns
                                .iter()
                                .map(|col| {
                                    extract_json_field(&map, &col.name, &col.data_type)
                                })
                                .collect(),
                            _ => {
                                // Non-object JSON or parse error: skip notification.
                                eprintln!("postgres: skipping non-JSON-object notification");
                                continue;
                            }
                        }
                    }
                };

                let update = StreamingUpdate::Insert(values);
                if sender.send(update).is_err() {
                    return;
                }
            }
        }

        let _ = sender.send(StreamingUpdate::Eof);
    }
}
