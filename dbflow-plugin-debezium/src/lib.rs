use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use dbflow_plugin::{
    crossbeam_channel, ColumnDef, DataSchema, DataType, DataValue, Plugin, PluginContext,
    StreamingDataProvider, StreamingDataSource, StreamingUpdate,
};

pub struct DebeziumPlugin;

impl Plugin for DebeziumPlugin {
    fn name(&self) -> &str {
        "debezium"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
        ctx.register_streaming_data_provider(Box::new(DebeziumStreamingProvider));
    }
}

struct DebeziumStreamingProvider;

impl StreamingDataProvider for DebeziumStreamingProvider {
    fn name(&self) -> &str {
        "debezium"
    }

    fn open_stream(
        &self,
        config: &HashMap<String, String>,
    ) -> Result<Box<dyn StreamingDataSource>, String> {
        let listen = config
            .get("listen")
            .ok_or("debezium streaming provider requires 'listen' config attribute")?
            .clone();

        let table = config
            .get("table")
            .ok_or("debezium streaming provider requires 'table' config attribute")?
            .clone();

        let columns_str = config
            .get("columns")
            .ok_or("debezium streaming provider requires 'columns' config attribute")?;

        let columns: Vec<String> = columns_str
            .split(',')
            .map(|s| s.trim().to_string())
            .collect();

        let types: Vec<DataType> = if let Some(types_str) = config.get("types") {
            types_str
                .split(',')
                .map(|s| match s.trim() {
                    "integer" => DataType::Integer,
                    "float" => DataType::Float,
                    _ => DataType::String,
                })
                .collect()
        } else {
            columns.iter().map(|_| DataType::String).collect()
        };

        if columns.len() != types.len() {
            return Err(format!(
                "columns count ({}) does not match types count ({})",
                columns.len(),
                types.len()
            ));
        }

        let schema = DataSchema {
            columns: columns
                .iter()
                .zip(types.iter())
                .map(|(name, dt)| ColumnDef {
                    name: name.clone(),
                    data_type: dt.clone(),
                })
                .collect(),
        };

        // Parse table filter: supports "table" or "schema.table".
        let (schema_filter, table_filter) = if let Some(dot) = table.find('.') {
            (
                Some(table[..dot].to_string()),
                table[dot + 1..].to_string(),
            )
        } else {
            (None, table)
        };

        let listener = tiny_http::Server::http(&listen)
            .map_err(|e| format!("failed to bind HTTP server on '{}': {}", listen, e))?;

        Ok(Box::new(DebeziumStreamingSource {
            schema,
            listener,
            schema_filter,
            table_filter,
            columns,
        }))
    }
}

struct DebeziumStreamingSource {
    schema: DataSchema,
    listener: tiny_http::Server,
    schema_filter: Option<String>,
    table_filter: String,
    columns: Vec<String>,
}

#[derive(Deserialize)]
struct DebeziumEnvelope {
    before: Option<serde_json::Value>,
    after: Option<serde_json::Value>,
    source: Option<DebeziumSource>,
    op: String,
}

#[derive(Deserialize)]
struct DebeziumSource {
    table: Option<String>,
    schema: Option<String>,
}

impl StreamingDataSource for DebeziumStreamingSource {
    fn schema(&self) -> &DataSchema {
        &self.schema
    }

    fn run(
        self: Box<Self>,
        sender: crossbeam_channel::Sender<StreamingUpdate>,
        shutdown: Arc<AtomicBool>,
    ) {
        let timeout = Duration::from_millis(200);

        while !shutdown.load(Ordering::Relaxed) {
            let mut request = match self.listener.recv_timeout(timeout) {
                Ok(Some(req)) => req,
                Ok(None) => continue, // timeout, check shutdown
                Err(_) => break,      // server error
            };

            // Only accept POST requests.
            if request.method() != &tiny_http::Method::Post {
                let response = tiny_http::Response::from_string("Method Not Allowed")
                    .with_status_code(tiny_http::StatusCode(405));
                let _ = request.respond(response);
                continue;
            }

            // Read body.
            let mut body = String::new();
            if request.as_reader().read_to_string(&mut body).is_err() {
                let response = tiny_http::Response::from_string("Bad Request")
                    .with_status_code(tiny_http::StatusCode(400));
                let _ = request.respond(response);
                continue;
            }

            // Parse body as a single envelope or an array of envelopes.
            let envelopes: Vec<DebeziumEnvelope> =
                match serde_json::from_str::<DebeziumEnvelope>(&body) {
                    Ok(e) => vec![e],
                    Err(_) => match serde_json::from_str::<Vec<DebeziumEnvelope>>(&body) {
                        Ok(arr) => arr,
                        Err(_) => {
                            let response = tiny_http::Response::from_string("Invalid JSON")
                                .with_status_code(tiny_http::StatusCode(400));
                            let _ = request.respond(response);
                            continue;
                        }
                    },
                };

            let mut channel_ok = true;
            for envelope in &envelopes {
                if !self.matches_table(envelope) {
                    continue;
                }
                let updates = self.map_event(envelope);
                for update in updates {
                    if sender.send(update).is_err() {
                        channel_ok = false;
                        break;
                    }
                }
                if !channel_ok {
                    break;
                }
            }

            if channel_ok {
                let response = tiny_http::Response::from_string("OK")
                    .with_status_code(tiny_http::StatusCode(200));
                let _ = request.respond(response);
            } else {
                let response = tiny_http::Response::from_string("Service Unavailable")
                    .with_status_code(tiny_http::StatusCode(503));
                let _ = request.respond(response);
                break;
            }
        }

        let _ = sender.send(StreamingUpdate::Eof);
    }
}

impl DebeziumStreamingSource {
    fn matches_table(&self, envelope: &DebeziumEnvelope) -> bool {
        let source = match &envelope.source {
            Some(s) => s,
            None => return false,
        };

        let table_matches = source
            .table
            .as_deref()
            .map(|t| t == self.table_filter)
            .unwrap_or(false);

        if !table_matches {
            return false;
        }

        // If schema filter was specified, also check schema.
        if let Some(ref expected_schema) = self.schema_filter {
            source
                .schema
                .as_deref()
                .map(|s| s == expected_schema.as_str())
                .unwrap_or(false)
        } else {
            true
        }
    }

    fn map_event(&self, envelope: &DebeziumEnvelope) -> Vec<StreamingUpdate> {
        let mut updates = Vec::new();

        match envelope.op.as_str() {
            "c" | "r" => {
                // Create or snapshot-read: insert from `after`.
                if let Some(row) = self.extract_row(&envelope.after) {
                    updates.push(StreamingUpdate::Insert(row));
                }
            }
            "d" => {
                // Delete: retract from `before`.
                if let Some(row) = self.extract_row(&envelope.before) {
                    updates.push(StreamingUpdate::Delete(row));
                }
            }
            "u" => {
                // Update: retract old from `before`, insert new from `after`.
                if let Some(row) = self.extract_row(&envelope.before) {
                    updates.push(StreamingUpdate::Delete(row));
                }
                if let Some(row) = self.extract_row(&envelope.after) {
                    updates.push(StreamingUpdate::Insert(row));
                }
            }
            _ => {} // Unknown op, ignore.
        }

        updates
    }

    fn extract_row(&self, value: &Option<serde_json::Value>) -> Option<Vec<DataValue>> {
        let obj = match value {
            Some(serde_json::Value::Object(map)) => map,
            _ => return None,
        };

        let mut row = Vec::with_capacity(self.columns.len());
        for (i, col_name) in self.columns.iter().enumerate() {
            let col_type = &self.schema.columns[i].data_type;
            let val = match obj.get(col_name) {
                Some(serde_json::Value::String(s)) => match col_type {
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
                Some(serde_json::Value::Number(n)) => match col_type {
                    DataType::Integer => DataValue::Integer(n.as_i64().unwrap_or(0)),
                    DataType::Float => DataValue::Float(n.as_f64().unwrap_or(0.0)),
                    DataType::String => DataValue::String(n.to_string()),
                },
                Some(serde_json::Value::Bool(b)) => match col_type {
                    DataType::Integer => DataValue::Integer(if *b { 1 } else { 0 }),
                    DataType::Float => DataValue::Float(if *b { 1.0 } else { 0.0 }),
                    DataType::String => DataValue::String(b.to_string()),
                },
                Some(serde_json::Value::Null) | None => DataValue::Null,
                _ => DataValue::Null,
            };
            row.push(val);
        }

        Some(row)
    }
}
