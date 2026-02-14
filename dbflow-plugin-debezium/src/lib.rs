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

// ---------------------------------------------------------------------------
// Config validation
// ---------------------------------------------------------------------------

/// Known config keys for the Debezium plugin.
const KNOWN_KEYS: &[&str] = &["listen", "table", "columns", "types"];

/// Validate that only known config keys are present.
fn validate_config(config: &HashMap<String, String>) -> Result<(), String> {
    for key in config.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "debezium: unknown config attribute '{}' (known: {})",
                key,
                KNOWN_KEYS.join(", ")
            ));
        }
    }
    Ok(())
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
        validate_config(config)?;

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
            let val = extract_json_field(obj, col_name, col_type);
            row.push(val);
        }

        Some(row)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_string() {
        let mut map = serde_json::Map::new();
        map.insert("name".to_string(), serde_json::Value::String("alice".to_string()));
        assert_eq!(
            extract_json_field(&map, "name", &DataType::String),
            DataValue::String("alice".to_string())
        );
    }

    #[test]
    fn extract_json_integer() {
        let mut map = serde_json::Map::new();
        map.insert("age".to_string(), serde_json::json!(42));
        assert_eq!(
            extract_json_field(&map, "age", &DataType::Integer),
            DataValue::Integer(42)
        );
    }

    #[test]
    fn extract_json_float() {
        let mut map = serde_json::Map::new();
        map.insert("score".to_string(), serde_json::json!(3.14));
        assert_eq!(
            extract_json_field(&map, "score", &DataType::Float),
            DataValue::Float(3.14)
        );
    }

    #[test]
    fn extract_json_null() {
        let map = serde_json::Map::new();
        assert_eq!(
            extract_json_field(&map, "missing", &DataType::String),
            DataValue::Null
        );
    }

    #[test]
    fn extract_json_explicit_null() {
        let mut map = serde_json::Map::new();
        map.insert("val".to_string(), serde_json::Value::Null);
        assert_eq!(
            extract_json_field(&map, "val", &DataType::Integer),
            DataValue::Null
        );
    }

    #[test]
    fn extract_json_bool_as_integer() {
        let mut map = serde_json::Map::new();
        map.insert("active".to_string(), serde_json::Value::Bool(true));
        assert_eq!(
            extract_json_field(&map, "active", &DataType::Integer),
            DataValue::Integer(1)
        );
    }

    #[test]
    fn extract_json_string_to_integer() {
        let mut map = serde_json::Map::new();
        map.insert("count".to_string(), serde_json::Value::String("123".to_string()));
        assert_eq!(
            extract_json_field(&map, "count", &DataType::Integer),
            DataValue::Integer(123)
        );
    }

    #[test]
    fn extract_json_string_parse_fail() {
        let mut map = serde_json::Map::new();
        map.insert("count".to_string(), serde_json::Value::String("abc".to_string()));
        assert_eq!(
            extract_json_field(&map, "count", &DataType::Integer),
            DataValue::Null
        );
    }

    #[test]
    fn validate_config_rejects_unknown() {
        let mut config = HashMap::new();
        config.insert("listen".to_string(), "127.0.0.1:8080".to_string());
        config.insert("bogus".to_string(), "val".to_string());
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn validate_config_accepts_known() {
        let mut config = HashMap::new();
        config.insert("listen".to_string(), "127.0.0.1:8080".to_string());
        config.insert("table".to_string(), "users".to_string());
        config.insert("columns".to_string(), "id,name".to_string());
        config.insert("types".to_string(), "integer,string".to_string());
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn parse_envelope_create() {
        let json = r#"{
            "before": null,
            "after": {"id": 1, "name": "alice"},
            "source": {"table": "users", "schema": "public"},
            "op": "c"
        }"#;
        let envelope: DebeziumEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.op, "c");
        assert!(envelope.after.is_some());
        assert!(envelope.before.is_none());
    }

    #[test]
    fn parse_envelope_update() {
        let json = r#"{
            "before": {"id": 1, "name": "alice"},
            "after": {"id": 1, "name": "bob"},
            "source": {"table": "users", "schema": "public"},
            "op": "u"
        }"#;
        let envelope: DebeziumEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.op, "u");
        assert!(envelope.before.is_some());
        assert!(envelope.after.is_some());
    }

    #[test]
    fn parse_envelope_delete() {
        let json = r#"{
            "before": {"id": 1, "name": "alice"},
            "after": null,
            "source": {"table": "users", "schema": "public"},
            "op": "d"
        }"#;
        let envelope: DebeziumEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.op, "d");
        assert!(envelope.before.is_some());
        assert!(envelope.after.is_none());
    }

    #[test]
    fn parse_envelope_array() {
        let json = r#"[
            {"before": null, "after": {"id": 1}, "source": {"table": "t"}, "op": "c"},
            {"before": null, "after": {"id": 2}, "source": {"table": "t"}, "op": "c"}
        ]"#;
        let envelopes: Vec<DebeziumEnvelope> = serde_json::from_str(json).unwrap();
        assert_eq!(envelopes.len(), 2);
    }

    #[test]
    fn table_filter_simple() {
        let src = DebeziumStreamingSource {
            schema: DataSchema { columns: vec![] },
            listener: tiny_http::Server::http("127.0.0.1:0").unwrap(),
            schema_filter: None,
            table_filter: "users".to_string(),
            columns: vec![],
        };

        let env: DebeziumEnvelope = serde_json::from_str(
            r#"{"before":null,"after":null,"source":{"table":"users"},"op":"c"}"#,
        ).unwrap();
        assert!(src.matches_table(&env));

        let env2: DebeziumEnvelope = serde_json::from_str(
            r#"{"before":null,"after":null,"source":{"table":"orders"},"op":"c"}"#,
        ).unwrap();
        assert!(!src.matches_table(&env2));
    }

    #[test]
    fn table_filter_with_schema() {
        let src = DebeziumStreamingSource {
            schema: DataSchema { columns: vec![] },
            listener: tiny_http::Server::http("127.0.0.1:0").unwrap(),
            schema_filter: Some("public".to_string()),
            table_filter: "users".to_string(),
            columns: vec![],
        };

        let env: DebeziumEnvelope = serde_json::from_str(
            r#"{"before":null,"after":null,"source":{"table":"users","schema":"public"},"op":"c"}"#,
        ).unwrap();
        assert!(src.matches_table(&env));

        let env2: DebeziumEnvelope = serde_json::from_str(
            r#"{"before":null,"after":null,"source":{"table":"users","schema":"private"},"op":"c"}"#,
        ).unwrap();
        assert!(!src.matches_table(&env2));
    }

    #[test]
    fn table_filter_no_source() {
        let src = DebeziumStreamingSource {
            schema: DataSchema { columns: vec![] },
            listener: tiny_http::Server::http("127.0.0.1:0").unwrap(),
            schema_filter: None,
            table_filter: "users".to_string(),
            columns: vec![],
        };

        let env: DebeziumEnvelope = serde_json::from_str(
            r#"{"before":null,"after":null,"op":"c"}"#,
        ).unwrap();
        assert!(!src.matches_table(&env));
    }

    #[test]
    fn map_event_create() {
        let src = DebeziumStreamingSource {
            schema: DataSchema {
                columns: vec![
                    ColumnDef { name: "id".to_string(), data_type: DataType::Integer },
                    ColumnDef { name: "name".to_string(), data_type: DataType::String },
                ],
            },
            listener: tiny_http::Server::http("127.0.0.1:0").unwrap(),
            schema_filter: None,
            table_filter: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
        };

        let env: DebeziumEnvelope = serde_json::from_str(
            r#"{"before":null,"after":{"id":1,"name":"alice"},"source":{"table":"users"},"op":"c"}"#,
        ).unwrap();

        let updates = src.map_event(&env);
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            StreamingUpdate::Insert(row) => {
                assert_eq!(row[0], DataValue::Integer(1));
                assert_eq!(row[1], DataValue::String("alice".to_string()));
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn map_event_delete() {
        let src = DebeziumStreamingSource {
            schema: DataSchema {
                columns: vec![
                    ColumnDef { name: "id".to_string(), data_type: DataType::Integer },
                ],
            },
            listener: tiny_http::Server::http("127.0.0.1:0").unwrap(),
            schema_filter: None,
            table_filter: "t".to_string(),
            columns: vec!["id".to_string()],
        };

        let env: DebeziumEnvelope = serde_json::from_str(
            r#"{"before":{"id":1},"after":null,"source":{"table":"t"},"op":"d"}"#,
        ).unwrap();

        let updates = src.map_event(&env);
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            StreamingUpdate::Delete(row) => {
                assert_eq!(row[0], DataValue::Integer(1));
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn map_event_update() {
        let src = DebeziumStreamingSource {
            schema: DataSchema {
                columns: vec![
                    ColumnDef { name: "id".to_string(), data_type: DataType::Integer },
                    ColumnDef { name: "name".to_string(), data_type: DataType::String },
                ],
            },
            listener: tiny_http::Server::http("127.0.0.1:0").unwrap(),
            schema_filter: None,
            table_filter: "t".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
        };

        let env: DebeziumEnvelope = serde_json::from_str(
            r#"{"before":{"id":1,"name":"alice"},"after":{"id":1,"name":"bob"},"source":{"table":"t"},"op":"u"}"#,
        ).unwrap();

        let updates = src.map_event(&env);
        assert_eq!(updates.len(), 2);
        match &updates[0] {
            StreamingUpdate::Delete(row) => assert_eq!(row[1], DataValue::String("alice".to_string())),
            _ => panic!("expected Delete"),
        }
        match &updates[1] {
            StreamingUpdate::Insert(row) => assert_eq!(row[1], DataValue::String("bob".to_string())),
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn map_event_unknown_op() {
        let src = DebeziumStreamingSource {
            schema: DataSchema { columns: vec![] },
            listener: tiny_http::Server::http("127.0.0.1:0").unwrap(),
            schema_filter: None,
            table_filter: "t".to_string(),
            columns: vec![],
        };

        let env: DebeziumEnvelope = serde_json::from_str(
            r#"{"before":null,"after":null,"source":{"table":"t"},"op":"x"}"#,
        ).unwrap();

        let updates = src.map_event(&env);
        assert!(updates.is_empty());
    }

    #[test]
    fn extract_row_with_null_field() {
        let src = DebeziumStreamingSource {
            schema: DataSchema {
                columns: vec![
                    ColumnDef { name: "id".to_string(), data_type: DataType::Integer },
                    ColumnDef { name: "name".to_string(), data_type: DataType::String },
                ],
            },
            listener: tiny_http::Server::http("127.0.0.1:0").unwrap(),
            schema_filter: None,
            table_filter: "t".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
        };

        let val = Some(serde_json::json!({"id": 1, "name": null}));
        let row = src.extract_row(&val).unwrap();
        assert_eq!(row[0], DataValue::Integer(1));
        assert_eq!(row[1], DataValue::Null);
    }

    #[test]
    fn columns_types_mismatch_rejected() {
        let provider = DebeziumStreamingProvider;
        let mut config = HashMap::new();
        config.insert("listen".to_string(), "127.0.0.1:0".to_string());
        config.insert("table".to_string(), "t".to_string());
        config.insert("columns".to_string(), "a,b".to_string());
        config.insert("types".to_string(), "integer".to_string());
        assert!(provider.open_stream(&config).is_err());
    }
}
