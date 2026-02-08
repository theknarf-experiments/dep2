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

        Ok(Box::new(KafkaStreamingSource {
            brokers,
            topic,
            group_id,
            schema: DataSchema {
                columns: vec![ColumnDef {
                    name: "value".to_string(),
                    data_type: DataType::String,
                }],
            },
        }))
    }
}

/// A streaming Kafka source that polls messages and sends them as rows.
struct KafkaStreamingSource {
    brokers: String,
    topic: String,
    group_id: String,
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
        let consumer: BaseConsumer = ClientConfig::new()
            .set("bootstrap.servers", &self.brokers)
            .set("group.id", &self.group_id)
            .set("auto.offset.reset", "earliest")
            .set("enable.auto.commit", "true")
            .create()
            .expect("failed to create Kafka consumer");

        consumer
            .subscribe(&[&self.topic])
            .expect("failed to subscribe to Kafka topic");

        while !shutdown.load(Ordering::Relaxed) {
            match consumer.poll(Duration::from_millis(100)) {
                Some(Ok(msg)) => {
                    let payload = msg.payload_view::<str>().and_then(|r| r.ok()).unwrap_or("");
                    let update =
                        StreamingUpdate::Insert(vec![DataValue::String(payload.to_string())]);
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
