use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};

use dbflow_plugin::{
    crossbeam_channel, ColumnDef, DataSchema, DataType, DataValue, Plugin, PluginContext,
    StreamingDataProvider, StreamingDataSource, StreamingUpdate,
};

pub struct CsvPlugin;

impl Plugin for CsvPlugin {
    fn name(&self) -> &str {
        "csv"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
        ctx.register_streaming_data_provider(Box::new(CsvStreamingProvider));
    }
}

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

        // Read headers to build the schema
        let mut reader = csv::Reader::from_path(path)
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

        let schema = DataSchema {
            columns: headers
                .iter()
                .map(|name| ColumnDef {
                    name: name.clone(),
                    data_type: DataType::String,
                })
                .collect(),
        };

        Ok(Box::new(CsvStreamingSource {
            schema,
            path: path.clone(),
        }))
    }
}

struct CsvStreamingSource {
    schema: DataSchema,
    path: String,
}

/// Read a CSV file and return a multiset: row → count.
fn read_csv_multiset(path: &str) -> Result<HashMap<Vec<String>, usize>, String> {
    let mut reader = csv::Reader::from_path(path)
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
        let mut current = match read_csv_multiset(&self.path) {
            Ok(ms) => ms,
            Err(e) => {
                eprintln!("csv streaming: failed initial read: {}", e);
                return;
            }
        };

        // Send all initial rows as inserts
        for (row, count) in &current {
            let values: Vec<DataValue> = row.iter().map(|s| DataValue::String(s.clone())).collect();
            for _ in 0..*count {
                if sender.send(StreamingUpdate::Insert(values.clone())).is_err() {
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
                    let new = match read_csv_multiset(&self.path) {
                        Ok(ms) => ms,
                        Err(_) => continue, // file might be mid-write
                    };

                    // Compute diff: deletions and insertions
                    // Deletions: rows in current but not (or fewer) in new
                    for (row, &old_count) in &current {
                        let new_count = new.get(row).copied().unwrap_or(0);
                        if old_count > new_count {
                            let values: Vec<DataValue> =
                                row.iter().map(|s| DataValue::String(s.clone())).collect();
                            for _ in 0..(old_count - new_count) {
                                if sender.send(StreamingUpdate::Delete(values.clone())).is_err() {
                                    return;
                                }
                            }
                        }
                    }

                    // Insertions: rows in new but not (or fewer) in current
                    for (row, &new_count) in &new {
                        let old_count = current.get(row).copied().unwrap_or(0);
                        if new_count > old_count {
                            let values: Vec<DataValue> =
                                row.iter().map(|s| DataValue::String(s.clone())).collect();
                            for _ in 0..(new_count - old_count) {
                                if sender.send(StreamingUpdate::Insert(values.clone())).is_err() {
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
