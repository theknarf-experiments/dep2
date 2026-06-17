//! Filesystem streaming plugin.
//!
//! Seeds a `files(path, ext)` relation by walking a project root, then watches
//! the tree recursively and emits insert/delete diffs as files appear and
//! disappear. Paths are **relative to the root**, using `/` separators, so they
//! join cleanly with relations produced by the tree-sitter plugin (which shares
//! the same root + convention).
//!
//! Config keys:
//!   - `root`   (required) project directory to seed and watch.
//!   - `exts`   (optional) comma-separated extension allow-list, e.g. `rs,toml`.
//!   - `ignore` (optional) comma-separated directory names to skip. Defaults to
//!              `.git,target,node_modules,.hg,.svn`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};

use dep2_plugin::{
    crossbeam_channel, ColumnDef, DataSchema, DataType, DataValue, Plugin, PluginContext,
    StreamOutput, StreamingDataProvider, StreamingDataSource, StreamingUpdate,
};

pub struct FsPlugin;

impl Plugin for FsPlugin {
    fn name(&self) -> &str {
        "fs"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
        ctx.register_streaming_data_provider(Box::new(FsStreamingProvider));
    }
}

const KNOWN_KEYS: &[&str] = &["root", "exts", "ignore"];
const DEFAULT_IGNORE: &[&str] = &[".git", "target", "node_modules", ".hg", ".svn"];

fn validate_config(config: &std::collections::HashMap<String, String>) -> Result<(), String> {
    for key in config.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "fs: unknown config attribute '{}' (known: {})",
                key,
                KNOWN_KEYS.join(", ")
            ));
        }
    }
    Ok(())
}

fn parse_set(
    config: &std::collections::HashMap<String, String>,
    key: &str,
) -> Option<HashSet<String>> {
    config.get(key).map(|s| {
        s.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    })
}

/// The schema is fixed: `(path: string, ext: string)`.
fn fs_schema() -> DataSchema {
    DataSchema {
        columns: vec![
            ColumnDef {
                name: "path".to_string(),
                data_type: DataType::String,
            },
            ColumnDef {
                name: "ext".to_string(),
                data_type: DataType::String,
            },
        ],
    }
}

/// Extract a lowercase extension (without the dot), or "" if none.
fn extension_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// A discovered file: (relative path with `/` separators, extension).
type FileRow = (String, String);

/// Recursively scan `root`, returning the set of files (relative path, ext).
/// Directories named in `ignore` are skipped entirely.
fn scan(root: &Path, exts: &Option<HashSet<String>>, ignore: &HashSet<String>) -> HashSet<FileRow> {
    let mut out = HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue, // unreadable dir (perms / race) — skip
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().to_string();
            if file_type.is_dir() {
                if ignore.contains(&name) {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file() {
                let ext = extension_of(&path);
                if let Some(allow) = exts {
                    if !allow.contains(&ext) {
                        continue;
                    }
                }
                let rel = path.strip_prefix(root).unwrap_or(&path);
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                out.insert((rel_str, ext));
            }
        }
    }
    out
}

fn row_to_values(row: &FileRow) -> Vec<DataValue> {
    vec![
        DataValue::String(row.0.clone()),
        DataValue::String(row.1.clone()),
    ]
}

struct FsStreamingProvider;

impl StreamingDataProvider for FsStreamingProvider {
    fn name(&self) -> &str {
        "fs"
    }

    fn open_stream(
        &self,
        config: &std::collections::HashMap<String, String>,
    ) -> Result<Box<dyn StreamingDataSource>, String> {
        validate_config(config)?;
        let root = config
            .get("root")
            .ok_or("fs streaming provider requires 'root' config attribute")?;
        let root = PathBuf::from(root);
        if !root.is_dir() {
            return Err(format!("fs: root '{}' is not a directory", root.display()));
        }
        let exts = parse_set(config, "exts");
        let ignore = parse_set(config, "ignore")
            .unwrap_or_else(|| DEFAULT_IGNORE.iter().map(|s| s.to_string()).collect());

        Ok(Box::new(FsStreamingSource {
            schema: fs_schema(),
            root,
            exts,
            ignore,
        }))
    }
}

struct FsStreamingSource {
    schema: DataSchema,
    root: PathBuf,
    exts: Option<HashSet<String>>,
    ignore: HashSet<String>,
}

impl StreamingDataSource for FsStreamingSource {
    fn outputs(&self) -> Vec<StreamOutput> {
        vec![StreamOutput {
            relation: String::new(),
            schema: self.schema.clone(),
        }]
    }

    fn run(
        self: Box<Self>,
        sender: crossbeam_channel::Sender<StreamingUpdate>,
        shutdown: Arc<AtomicBool>,
    ) {
        // 1. Seed: emit every current file as an insert.
        let mut current = scan(&self.root, &self.exts, &self.ignore);
        for row in &current {
            if sender
                .send(StreamingUpdate::Insert(row_to_values(row)))
                .is_err()
            {
                return;
            }
        }

        // 2. Watch the tree recursively.
        let (notify_tx, notify_rx) = std::sync::mpsc::channel();
        let mut watcher =
            match notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = notify_tx.send(event);
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("fs streaming: failed to create watcher: {}", e);
                    return;
                }
            };
        if let Err(e) = watcher.watch(&self.root, RecursiveMode::Recursive) {
            eprintln!(
                "fs streaming: failed to watch '{}': {}",
                self.root.display(),
                e
            );
            return;
        }

        // 3. On any change, debounce, rescan, and diff against the current set.
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            match notify_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(_event) => {
                    // Coalesce a burst of events (editor save = many events).
                    while notify_rx.try_recv().is_ok() {}
                    std::thread::sleep(Duration::from_millis(50));

                    let new = scan(&self.root, &self.exts, &self.ignore);

                    for row in current.difference(&new) {
                        if sender
                            .send(StreamingUpdate::Delete(row_to_values(row)))
                            .is_err()
                        {
                            return;
                        }
                    }
                    for row in new.difference(&current) {
                        if sender
                            .send(StreamingUpdate::Insert(row_to_values(row)))
                            .is_err()
                        {
                            return;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn scan_finds_files_relative() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("a.rs"), "fn main() {}");
        write(&root.join("sub/b.toml"), "x = 1");
        let found = scan(root, &None, &HashSet::new());
        assert!(found.contains(&("a.rs".to_string(), "rs".to_string())));
        assert!(found.contains(&("sub/b.toml".to_string(), "toml".to_string())));
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn scan_respects_ignore_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("keep.rs"), "");
        write(&root.join("target/skip.rs"), "");
        let ignore: HashSet<String> = ["target".to_string()].into_iter().collect();
        let found = scan(root, &None, &ignore);
        assert!(found.contains(&("keep.rs".to_string(), "rs".to_string())));
        assert!(!found.iter().any(|(p, _)| p.starts_with("target/")));
    }

    #[test]
    fn scan_respects_ext_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("a.rs"), "");
        write(&root.join("b.md"), "");
        let allow: HashSet<String> = ["rs".to_string()].into_iter().collect();
        let found = scan(root, &Some(allow), &HashSet::new());
        assert_eq!(found.len(), 1);
        assert!(found.contains(&("a.rs".to_string(), "rs".to_string())));
    }

    #[test]
    fn extension_lowercased_and_empty() {
        assert_eq!(extension_of(Path::new("x.RS")), "rs");
        assert_eq!(extension_of(Path::new("Makefile")), "");
    }
}
