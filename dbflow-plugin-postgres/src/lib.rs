use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use postgres::fallible_iterator::FallibleIterator;
use postgres::{Client, NoTls};

use dbflow_plugin::{
    ColumnDef, DataSchema, DataType, DataValue, Plugin, PluginContext, StreamingDataProvider,
    StreamingDataSource, StreamingUpdate,
};

pub struct PostgresPlugin;

impl Plugin for PostgresPlugin {
    fn name(&self) -> &str {
        "postgres"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
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

                let types: Vec<DataType> = if let Some(types_str) = config.get("types") {
                    let types: Vec<DataType> = types_str
                        .split(',')
                        .map(|s| match s.trim() {
                            "integer" => DataType::Integer,
                            "float" => DataType::Float,
                            _ => DataType::String,
                        })
                        .collect();
                    if types.len() != columns.len() {
                        return Err(format!(
                            "postgres columns count ({}) does not match types count ({})",
                            columns.len(),
                            types.len()
                        ));
                    }
                    types
                } else {
                    columns.iter().map(|_| DataType::String).collect()
                };

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
fn extract_json_field(
    map: &serde_json::Map<String, serde_json::Value>,
    name: &str,
    data_type: &DataType,
) -> DataValue {
    match map.get(name) {
        Some(serde_json::Value::String(s)) => match data_type {
            DataType::Integer => DataValue::Integer(s.parse::<i64>().unwrap_or(0)),
            DataType::Float => DataValue::Float(s.parse::<f64>().unwrap_or(0.0)),
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
        Some(serde_json::Value::Null) | None => match data_type {
            DataType::Integer => DataValue::Integer(0),
            DataType::Float => DataValue::Float(0.0),
            DataType::String => DataValue::String(String::new()),
        },
        _ => DataValue::String(String::new()),
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
        let mut client =
            Client::connect(&self.connection, NoTls).expect("failed to connect to PostgreSQL");

        let listen_sql = format!("LISTEN {}", quote_ident(&self.channel));
        client
            .execute(&listen_sql, &[])
            .expect("failed to execute LISTEN");

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
