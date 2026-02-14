use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rdkafka::config::ClientConfig;
use rdkafka::consumer::{BaseConsumer, Consumer};
use rdkafka::message::Message;

use dbflow_plugin::{
    ColumnDef, DataSchema, DataType, DataValue, Plugin, PluginContext, StreamingDataProvider,
    StreamingDataSource, StreamingUpdate,
};

pub struct KafkaPlugin;

impl Plugin for KafkaPlugin {
    fn name(&self) -> &str {
        "kafka"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
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

        let format = match config.get("format").map(|s| s.as_str()) {
            Some("json") => Format::Json,
            Some("raw") | None => Format::Raw,
            Some(other) => {
                return Err(format!(
                    "unknown kafka format '{}': expected 'raw' or 'json'",
                    other
                ))
            }
        };

        // Parse column types: comma-separated type names (integer, float, string).
        let parse_types = |cols: &[String], types_str: Option<&String>| -> Result<Vec<DataType>, String> {
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
                    "kafka format 'json' requires 'columns' config attribute".to_string()
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
        let consumer: BaseConsumer = match ClientConfig::new()
            .set("bootstrap.servers", &self.brokers)
            .set("group.id", &self.group_id)
            .set("auto.offset.reset", "earliest")
            .set("enable.auto.commit", "true")
            .create()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("kafka: failed to create consumer: {}", e);
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

                    let values = match self.format {
                        Format::Raw => vec![DataValue::String(payload.to_string())],
                        Format::Json => match serde_json::from_str::<serde_json::Value>(payload) {
                            Ok(serde_json::Value::Object(map)) => self
                                .schema
                                .columns
                                .iter()
                                .map(|col| extract_json_field(&map, &col.name, &col.data_type))
                                .collect(),
                            _ => {
                                // Non-object JSON or parse error: skip message.
                                eprintln!("kafka: skipping non-JSON-object message");
                                continue;
                            }
                        },
                    };

                    let update = StreamingUpdate::Insert(values);
                    if sender.send(update).is_err() {
                        break;
                    }
                }
                Some(Err(e)) => {
                    eprintln!("kafka error: {}", e);
                }
                None => {
                    // No message available, continue polling.
                }
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
