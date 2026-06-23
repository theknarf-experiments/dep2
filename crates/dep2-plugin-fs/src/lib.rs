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
//!   - `ignore` (optional) extra comma-separated directory names to skip (on top
//!              of `.gitignore`), defaulting to `.git,target,node_modules,.hg,.svn`.
//!
//! Discovery honors `.gitignore`/`.ignore`/global gitignore and skips hidden
//! entries, so git-ignored files never enter the engine.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use notify::{RecursiveMode, Watcher};

use dep2_plugin::{
    ColumnDef, DataSchema, DataType, DataValue, Plugin, PluginContext, SourceRunner, SourceState,
    StreamOutput, StreamingDataProvider, StreamingDataSource, ValueSink,
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

/// Scan `root`, returning the set of files (relative path, ext).
///
/// Walks with the `ignore` crate so git-ignored paths are skipped (honoring
/// `.gitignore`/`.ignore`/global gitignore and parent dirs, plus hidden
/// entries). The configured/default `ignore` directory names are skipped on top,
/// so `target`, `node_modules`, etc. are excluded even without a `.gitignore`.
fn scan(root: &Path, exts: &Option<HashSet<String>>, ignore: &HashSet<String>) -> HashSet<FileRow> {
    let names = ignore.clone();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(move |e| !names.contains(&*e.file_name().to_string_lossy()))
        .build();

    let mut out = HashSet::new();
    for entry in walker.flatten() {
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let ext = extension_of(path);
        if let Some(allow) = exts {
            if !allow.contains(&ext) {
                continue;
            }
        }
        let rel = path.strip_prefix(root).unwrap_or(path);
        out.insert((rel.to_string_lossy().replace('\\', "/"), ext));
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
            seeded: false,
            watch: None,
        }))
    }
}

struct FsStreamingSource {
    schema: DataSchema,
    root: PathBuf,
    exts: Option<HashSet<String>>,
    ignore: HashSet<String>,
    seeded: bool,
    watch: Option<FsWatch>,
}

/// Live-edit watch state for an fs source (set up after the seed).
struct FsWatch {
    current: HashSet<FileRow>,
    notify_rx: std::sync::mpsc::Receiver<notify::Event>,
    /// Held to keep the watcher alive.
    _watcher: notify::RecommendedWatcher,
}

impl StreamingDataSource for FsStreamingSource {
    fn outputs(&self) -> Vec<StreamOutput> {
        vec![StreamOutput {
            relation: String::new(),
            schema: self.schema.clone(),
        }]
    }

    fn build(self: Box<Self>) -> Box<dyn SourceRunner> {
        self
    }
}

impl SourceRunner for FsStreamingSource {
    fn step(&mut self, sink: &mut dyn ValueSink, shutdown: &AtomicBool) -> SourceState {
        if shutdown.load(Ordering::Relaxed) {
            return SourceState::Idle;
        }

        // 1. Seed: emit every current file as an insert, then arm the watcher.
        if !self.seeded {
            self.seeded = true;
            let current = scan(&self.root, &self.exts, &self.ignore);
            for row in &current {
                sink.push("", &row_to_values(row), 1);
            }
            let (notify_tx, notify_rx) = std::sync::mpsc::channel();
            if let Ok(mut watcher) =
                notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                    if let Ok(event) = res {
                        let _ = notify_tx.send(event);
                    }
                })
            {
                if watcher.watch(&self.root, RecursiveMode::Recursive).is_ok() {
                    self.watch = Some(FsWatch {
                        current,
                        notify_rx,
                        _watcher: watcher,
                    });
                }
            }
            return SourceState::Pending;
        }

        // 2. Watch: drain pending events non-blocking; rescan + diff on change.
        if self.watch.is_none() {
            return SourceState::Idle;
        }
        let mut changed = false;
        {
            let watch = self.watch.as_ref().unwrap();
            while watch.notify_rx.try_recv().is_ok() {
                changed = true;
            }
        }
        if !changed {
            return SourceState::Idle;
        }

        let new = scan(&self.root, &self.exts, &self.ignore);
        let old = std::mem::take(&mut self.watch.as_mut().unwrap().current);
        for row in old.difference(&new) {
            sink.push("", &row_to_values(row), -1);
        }
        for row in new.difference(&old) {
            sink.push("", &row_to_values(row), 1);
        }
        self.watch.as_mut().unwrap().current = new;
        SourceState::Pending
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
