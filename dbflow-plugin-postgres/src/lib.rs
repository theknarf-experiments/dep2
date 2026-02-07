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

        Ok(Box::new(PostgresStreamingSource {
            connection,
            channel,
            schema: DataSchema {
                columns: vec![ColumnDef {
                    name: "value".to_string(),
                    data_type: DataType::String,
                }],
            },
        }))
    }
}

/// A streaming PostgreSQL source that uses LISTEN/NOTIFY to receive notifications.
struct PostgresStreamingSource {
    connection: String,
    channel: String,
    schema: DataSchema,
}

/// Wraps a channel name in double quotes for safe use in SQL LISTEN statements.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
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
                let update = StreamingUpdate::Insert(vec![DataValue::String(payload)]);
                if sender.send(update).is_err() {
                    return;
                }
            }
        }

        let _ = sender.send(StreamingUpdate::Eof);
    }
}
