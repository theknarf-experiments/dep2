use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rdkafka::config::ClientConfig;
use rdkafka::consumer::{BaseConsumer, Consumer};
use rdkafka::message::Message;

use dbflow_plugin::{
    ColumnDef, DataProvider, DataSchema, DataSource, DataType, DataValue, Plugin, PluginContext,
    StreamingDataProvider, StreamingDataSource, StreamingUpdate,
};

pub struct KafkaPlugin;

impl Plugin for KafkaPlugin {
    fn name(&self) -> &str {
        "kafka"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
        ctx.register_data_provider(Box::new(KafkaBatchProvider));
        ctx.register_streaming_data_provider(Box::new(KafkaStreamingProvider));
    }
}

/// The message format for Kafka messages.
#[derive(Clone, Copy, PartialEq)]
enum Format {
    /// Raw text: single "value" column containing the message payload.
    Raw,
    /// JSON: parse each message as a JSON object and extract named columns.
    Json,
}

/// Known config keys for the Kafka plugin.
const KNOWN_KEYS: &[&str] = &[
    "brokers", "topic", "group_id", "format", "columns", "types", "timeout",
];

/// Validate that only known config keys are present.
fn validate_config(config: &HashMap<String, String>) -> Result<(), String> {
    for key in config.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "kafka: unknown config attribute '{}' (known: {})",
                key,
                KNOWN_KEYS.join(", ")
            ));
        }
    }
    Ok(())
}

/// Parse the format config value.
fn parse_format(config: &HashMap<String, String>) -> Result<Format, String> {
    match config.get("format").map(|s| s.as_str()) {
        Some("json") => Ok(Format::Json),
        Some("raw") | None => Ok(Format::Raw),
        Some(other) => Err(format!(
            "unknown kafka format '{}': expected 'raw' or 'json'",
            other
        )),
    }
}

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
                "kafka columns count ({}) does not match types count ({})",
                cols.len(),
                types.len()
            ));
        }
        Ok(types)
    } else {
        Ok(cols.iter().map(|_| DataType::String).collect())
    }
}

/// Build a schema from format and config.
fn build_schema(format: Format, config: &HashMap<String, String>) -> Result<DataSchema, String> {
    match format {
        Format::Raw => Ok(DataSchema {
            columns: vec![ColumnDef {
                name: "value".to_string(),
                data_type: DataType::String,
            }],
        }),
        Format::Json => {
            let columns_str = config.get("columns").ok_or_else(|| {
                "kafka format 'json' requires 'columns' config attribute".to_string()
            })?;
            let columns: Vec<String> =
                columns_str.split(',').map(|s| s.trim().to_string()).collect();
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
        }
    }
}

/// Build a Kafka consumer from config.
fn build_consumer(
    brokers: &str,
    group_id: &str,
) -> Result<BaseConsumer, String> {
    ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .set("group.id", group_id)
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "true")
        .create()
        .map_err(|e| format!("kafka: failed to create consumer: {}", e))
}

/// Parse a Kafka message payload into a row of DataValues.
fn parse_message(
    payload: &str,
    format: Format,
    schema: &DataSchema,
) -> Option<Vec<DataValue>> {
    match format {
        Format::Raw => Some(vec![DataValue::String(payload.to_string())]),
        Format::Json => match serde_json::from_str::<serde_json::Value>(payload) {
            Ok(serde_json::Value::Object(map)) => Some(
                schema
                    .columns
                    .iter()
                    .map(|col| extract_json_field(&map, &col.name, &col.data_type))
                    .collect(),
            ),
            _ => None,
        },
    }
}

// ---------------------------------------------------------------------------
// Batch data provider — consumes messages with a timeout
// ---------------------------------------------------------------------------

struct KafkaBatchProvider;

impl DataProvider for KafkaBatchProvider {
    fn name(&self) -> &str {
        "kafka"
    }

    fn open(&self, config: &HashMap<String, String>) -> Result<Box<dyn DataSource>, String> {
        validate_config(config)?;

        let brokers = config
            .get("brokers")
            .ok_or_else(|| "kafka data block missing 'brokers' config".to_string())?;
        let topic = config
            .get("topic")
            .ok_or_else(|| "kafka data block missing 'topic' config".to_string())?;
        let group_id = config
            .get("group_id")
            .cloned()
            .unwrap_or_else(|| "dbflow-consumer".to_string());

        let timeout_secs: u64 = config
            .get("timeout")
            .map(|s| {
                s.parse::<u64>()
                    .map_err(|_| format!("invalid timeout '{}': must be a positive integer", s))
            })
            .transpose()?
            .unwrap_or(5);

        let format = parse_format(config)?;
        let schema = build_schema(format, config)?;

        let consumer = build_consumer(brokers, &group_id)?;
        consumer
            .subscribe(&[topic])
            .map_err(|e| format!("kafka: failed to subscribe to topic '{}': {}", topic, e))?;

        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let mut rows = Vec::new();

        while Instant::now() < deadline {
            match consumer.poll(Duration::from_millis(100)) {
                Some(Ok(msg)) => {
                    let payload = msg.payload_view::<str>().and_then(|r| r.ok()).unwrap_or("");
                    if let Some(values) = parse_message(payload, format, &schema) {
                        rows.push(values);
                    }
                }
                Some(Err(e)) => {
                    eprintln!("kafka batch error: {}", e);
                }
                None => {}
            }
        }

        Ok(Box::new(KafkaBatchSource { schema, rows }))
    }
}

struct KafkaBatchSource {
    schema: DataSchema,
    rows: Vec<Vec<DataValue>>,
}

impl DataSource for KafkaBatchSource {
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

/// Factory that creates Kafka streaming sources from HCL config.
struct KafkaStreamingProvider;

impl StreamingDataProvider for KafkaStreamingProvider {
    fn name(&self) -> &str {
        "kafka"
    }

    fn open_stream(
        &self,
        config: &HashMap<String, String>,
    ) -> Result<Box<dyn StreamingDataSource>, String> {
        validate_config(config)?;

        let brokers = config
            .get("brokers")
            .ok_or_else(|| "kafka data block missing 'brokers' config".to_string())?
            .clone();
        let topic = config
            .get("topic")
            .ok_or_else(|| "kafka data block missing 'topic' config".to_string())?
            .clone();
        let group_id = config
            .get("group_id")
            .cloned()
            .unwrap_or_else(|| "dbflow-consumer".to_string());

        let format = parse_format(config)?;
        let schema = build_schema(format, config)?;

        Ok(Box::new(KafkaStreamingSource {
            brokers,
            topic,
            group_id,
            format,
            schema,
        }))
    }
}

/// A streaming Kafka source that polls messages and sends them as rows.
struct KafkaStreamingSource {
    brokers: String,
    topic: String,
    group_id: String,
    format: Format,
    schema: DataSchema,
}

impl StreamingDataSource for KafkaStreamingSource {
    fn schema(&self) -> &DataSchema {
        &self.schema
    }

    fn run(
        self: Box<Self>,
        sender: dbflow_plugin::crossbeam_channel::Sender<StreamingUpdate>,
        shutdown: Arc<AtomicBool>,
    ) {
        let consumer = match build_consumer(&self.brokers, &self.group_id) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{}", e);
                let _ = sender.send(StreamingUpdate::Eof);
                return;
            }
        };

        if let Err(e) = consumer.subscribe(&[&self.topic]) {
            eprintln!("kafka: failed to subscribe to topic '{}': {}", self.topic, e);
            let _ = sender.send(StreamingUpdate::Eof);
            return;
        }

        while !shutdown.load(Ordering::Relaxed) {
            match consumer.poll(Duration::from_millis(100)) {
                Some(Ok(msg)) => {
                    let payload = msg.payload_view::<str>().and_then(|r| r.ok()).unwrap_or("");
                    if let Some(values) = parse_message(payload, self.format, &self.schema) {
                        if sender.send(StreamingUpdate::Insert(values)).is_err() {
                            break;
                        }
                    } else {
                        eprintln!("kafka: skipping non-JSON-object message");
                    }
                }
                Some(Err(e)) => {
                    eprintln!("kafka error: {}", e);
                }
                None => {}
            }
        }

        let _ = sender.send(StreamingUpdate::Eof);
    }
}

/// Extract a field from a JSON object map, coercing to the expected DataType.
/// JSON null or missing fields yield `DataValue::Null`.
/// Parse failures for numeric coercion also yield `DataValue::Null`.
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
    fn extract_json_string_field() {
        let mut map = serde_json::Map::new();
        map.insert("name".to_string(), serde_json::Value::String("alice".to_string()));
        assert_eq!(
            extract_json_field(&map, "name", &DataType::String),
            DataValue::String("alice".to_string())
        );
    }

    #[test]
    fn extract_json_integer_field() {
        let mut map = serde_json::Map::new();
        map.insert("age".to_string(), serde_json::json!(42));
        assert_eq!(
            extract_json_field(&map, "age", &DataType::Integer),
            DataValue::Integer(42)
        );
    }

    #[test]
    fn extract_json_float_field() {
        let mut map = serde_json::Map::new();
        map.insert("score".to_string(), serde_json::json!(3.14));
        assert_eq!(
            extract_json_field(&map, "score", &DataType::Float),
            DataValue::Float(3.14)
        );
    }

    #[test]
    fn extract_json_null_field() {
        let map = serde_json::Map::new();
        assert_eq!(
            extract_json_field(&map, "missing", &DataType::String),
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
    fn extract_json_string_to_integer_coercion() {
        let mut map = serde_json::Map::new();
        map.insert("count".to_string(), serde_json::Value::String("123".to_string()));
        assert_eq!(
            extract_json_field(&map, "count", &DataType::Integer),
            DataValue::Integer(123)
        );
    }

    #[test]
    fn extract_json_string_to_integer_fail_null() {
        let mut map = serde_json::Map::new();
        map.insert("count".to_string(), serde_json::Value::String("abc".to_string()));
        assert_eq!(
            extract_json_field(&map, "count", &DataType::Integer),
            DataValue::Null
        );
    }

    #[test]
    fn extract_json_explicit_null() {
        let mut map = serde_json::Map::new();
        map.insert("value".to_string(), serde_json::Value::Null);
        assert_eq!(
            extract_json_field(&map, "value", &DataType::String),
            DataValue::Null
        );
    }

    #[test]
    fn parse_message_raw() {
        let schema = DataSchema {
            columns: vec![ColumnDef {
                name: "value".to_string(),
                data_type: DataType::String,
            }],
        };
        let result = parse_message("hello", Format::Raw, &schema);
        assert_eq!(result, Some(vec![DataValue::String("hello".to_string())]));
    }

    #[test]
    fn parse_message_json() {
        let schema = DataSchema {
            columns: vec![
                ColumnDef { name: "name".to_string(), data_type: DataType::String },
                ColumnDef { name: "age".to_string(), data_type: DataType::Integer },
            ],
        };
        let result = parse_message(r#"{"name":"bob","age":30}"#, Format::Json, &schema);
        assert_eq!(
            result,
            Some(vec![
                DataValue::String("bob".to_string()),
                DataValue::Integer(30),
            ])
        );
    }

    #[test]
    fn parse_message_json_invalid() {
        let schema = DataSchema {
            columns: vec![ColumnDef { name: "x".to_string(), data_type: DataType::String }],
        };
        assert_eq!(parse_message("not json", Format::Json, &schema), None);
    }

    #[test]
    fn validate_config_rejects_unknown_key() {
        let mut config = HashMap::new();
        config.insert("brokers".to_string(), "localhost:9092".to_string());
        config.insert("unknown_key".to_string(), "value".to_string());
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn validate_config_accepts_known_keys() {
        let mut config = HashMap::new();
        config.insert("brokers".to_string(), "localhost:9092".to_string());
        config.insert("topic".to_string(), "test".to_string());
        config.insert("format".to_string(), "json".to_string());
        config.insert("columns".to_string(), "a,b".to_string());
        config.insert("types".to_string(), "string,integer".to_string());
        config.insert("group_id".to_string(), "test-group".to_string());
        config.insert("timeout".to_string(), "10".to_string());
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn parse_types_mismatch() {
        let cols = vec!["a".to_string(), "b".to_string()];
        let types = "integer".to_string();
        assert!(parse_types(&cols, Some(&types)).is_err());
    }

    #[test]
    fn parse_types_defaults_to_string() {
        let cols = vec!["a".to_string(), "b".to_string()];
        let types = parse_types(&cols, None).unwrap();
        assert_eq!(types, vec![DataType::String, DataType::String]);
    }

    #[test]
    fn build_schema_raw() {
        let config = HashMap::new();
        let schema = build_schema(Format::Raw, &config).unwrap();
        assert_eq!(schema.columns.len(), 1);
        assert_eq!(schema.columns[0].name, "value");
    }

    #[test]
    fn build_schema_json() {
        let mut config = HashMap::new();
        config.insert("columns".to_string(), "name, age".to_string());
        config.insert("types".to_string(), "string, integer".to_string());
        let schema = build_schema(Format::Json, &config).unwrap();
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.columns[0].name, "name");
        assert_eq!(schema.columns[0].data_type, DataType::String);
        assert_eq!(schema.columns[1].name, "age");
        assert_eq!(schema.columns[1].data_type, DataType::Integer);
    }

    #[test]
    fn build_schema_json_missing_columns() {
        let config = HashMap::new();
        assert!(build_schema(Format::Json, &config).is_err());
    }
}
