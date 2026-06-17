//! Tree-sitter streaming plugin.
//!
//! Parses each source file in a project with a tree-sitter grammar loaded from
//! a `.wasm` file, flattens the syntax tree, and feeds it into an `ast_node`
//! relation. On file content changes it re-parses and emits insert/delete diffs.
//!
//! Grammars are loaded at runtime from `.wasm`, dispatched by file extension, so
//! any language with a compiled tree-sitter grammar works without recompiling.
//!
//! The `ast_node` relation is:
//! ```text
//! ast_node(file: string, id: number, parent: number, kind: string,
//!          named: number, start: number, end: number, text: string)
//! ```
//! - `file`   relative path (matches the `fs` plugin's convention, so the two
//!            relations join).
//! - `id`     per-file pre-order index of the node (`parent` = -1 for the root).
//! - `kind`   the grammar node type (e.g. `function_item`, `identifier`, `"{"`).
//! - `named`  1 for named grammar nodes, 0 for anonymous tokens/punctuation.
//! - `start`/`end` byte offsets into the file.
//! - `text`   the source slice for leaf nodes (empty for interior nodes).
//!
//! Config keys:
//!   - `root`     (required) project directory to parse and watch.
//!   - `grammars` (required) comma-separated `ext=path.wasm` pairs, e.g.
//!                `rs=/abs/tree-sitter-rust.wasm,py=/abs/tree-sitter-python.wasm`.
//!   - `ignore`   (optional) comma-separated directory names to skip.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use notify::{RecursiveMode, Watcher};
use tree_sitter::{wasmtime::Engine, Node, Parser, WasmStore};

use dep2_plugin::{
    crossbeam_channel, ColumnDef, DataSchema, DataType, DataValue, Plugin, PluginContext,
    StreamingDataProvider, StreamingDataSource, StreamingUpdate,
};

pub struct TreeSitterPlugin;

impl Plugin for TreeSitterPlugin {
    fn name(&self) -> &str {
        "treesitter"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
        ctx.register_streaming_data_provider(Box::new(TreeSitterStreamingProvider));
    }
}

const KNOWN_KEYS: &[&str] = &["root", "grammars", "ignore"];
const DEFAULT_IGNORE: &[&str] = &[".git", "target", "node_modules", ".hg", ".svn"];

fn validate_config(config: &HashMap<String, String>) -> Result<(), String> {
    for key in config.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "treesitter: unknown config attribute '{}' (known: {})",
                key,
                KNOWN_KEYS.join(", ")
            ));
        }
    }
    Ok(())
}

/// Parse the `grammars` config into an `ext -> wasm path` map.
fn parse_grammars(config: &HashMap<String, String>) -> Result<HashMap<String, PathBuf>, String> {
    let raw = config
        .get("grammars")
        .ok_or("treesitter requires 'grammars' config (ext=path.wasm,...)")?;
    let mut map = HashMap::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (ext, path) = entry
            .split_once('=')
            .ok_or_else(|| format!("invalid grammar entry '{}': expected ext=path.wasm", entry))?;
        map.insert(ext.trim().to_ascii_lowercase(), PathBuf::from(path.trim()));
    }
    if map.is_empty() {
        return Err("treesitter 'grammars' config is empty".to_string());
    }
    Ok(map)
}

fn parse_ignore(config: &HashMap<String, String>) -> HashSet<String> {
    match config.get("ignore") {
        Some(s) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        None => DEFAULT_IGNORE.iter().map(|s| s.to_string()).collect(),
    }
}

fn ast_schema() -> DataSchema {
    let col = |name: &str, dt: DataType| ColumnDef {
        name: name.to_string(),
        data_type: dt,
    };
    DataSchema {
        columns: vec![
            col("file", DataType::String),
            col("id", DataType::Integer),
            col("parent", DataType::Integer),
            col("kind", DataType::String),
            col("named", DataType::Integer),
            col("start", DataType::Integer),
            col("end", DataType::Integer),
            col("text", DataType::String),
        ],
    }
}

/// Derive the tree-sitter language name from a grammar `.wasm` filename.
/// `tree-sitter-rust.wasm` -> `rust`, `tree-sitter-typescript.wasm` ->
/// `typescript`. Any leading `tree-sitter-`/`tree_sitter_` is stripped and
/// dashes are normalised to underscores (the C symbol form).
fn grammar_lang_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let stem = stem
        .strip_prefix("tree-sitter-")
        .or_else(|| stem.strip_prefix("tree_sitter_"))
        .unwrap_or(stem);
    stem.replace('-', "_")
}

fn extension_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// One flattened AST node row.
/// (file, id, parent, kind, named, start, end, text)
type Row = (String, i64, i64, String, i64, i64, i64, String);

fn row_to_values(r: &Row) -> Vec<DataValue> {
    vec![
        DataValue::String(r.0.clone()),
        DataValue::Integer(r.1),
        DataValue::Integer(r.2),
        DataValue::String(r.3.clone()),
        DataValue::Integer(r.4),
        DataValue::Integer(r.5),
        DataValue::Integer(r.6),
        DataValue::String(r.7.clone()),
    ]
}

/// Recursively flatten a node and its children into `out`, assigning dense
/// pre-order ids. Returns the next available id.
fn flatten(
    node: Node,
    parent_id: i64,
    next_id: i64,
    file: &str,
    src: &str,
    out: &mut Vec<Row>,
) -> i64 {
    let id = next_id;
    let start = node.start_byte();
    let end = node.end_byte();
    let text = if node.child_count() == 0 {
        src.get(start..end).unwrap_or("").to_string()
    } else {
        String::new()
    };
    out.push((
        file.to_string(),
        id,
        parent_id,
        node.kind().to_string(),
        if node.is_named() { 1 } else { 0 },
        start as i64,
        end as i64,
        text,
    ));

    let mut child_next = id + 1;
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            child_next = flatten(cursor.node(), id, child_next, file, src, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    child_next
}

struct TreeSitterStreamingProvider;

impl StreamingDataProvider for TreeSitterStreamingProvider {
    fn name(&self) -> &str {
        "treesitter"
    }

    fn open_stream(
        &self,
        config: &HashMap<String, String>,
    ) -> Result<Box<dyn StreamingDataSource>, String> {
        validate_config(config)?;
        let root = config
            .get("root")
            .ok_or("treesitter requires 'root' config attribute")?;
        let root = PathBuf::from(root);
        if !root.is_dir() {
            return Err(format!(
                "treesitter: root '{}' is not a directory",
                root.display()
            ));
        }
        let grammars = parse_grammars(config)?;
        // Fail fast if any grammar file is missing.
        for (ext, path) in &grammars {
            if !path.is_file() {
                return Err(format!(
                    "treesitter: grammar for '{}' not found at '{}'",
                    ext,
                    path.display()
                ));
            }
        }
        let ignore = parse_ignore(config);

        Ok(Box::new(TreeSitterStreamingSource {
            schema: ast_schema(),
            root,
            grammars,
            ignore,
        }))
    }
}

struct TreeSitterStreamingSource {
    schema: DataSchema,
    root: PathBuf,
    grammars: HashMap<String, PathBuf>,
    ignore: HashSet<String>,
}

/// A loaded parser plus its per-extension languages. Created on the worker
/// thread (wasm types are not `Send`).
struct ParseEngine {
    parser: Parser,
    languages: HashMap<String, tree_sitter::Language>,
}

impl ParseEngine {
    fn new(grammars: &HashMap<String, PathBuf>) -> Result<Self, String> {
        let engine = Engine::default();
        let mut store = WasmStore::new(&engine)
            .map_err(|e| format!("treesitter: failed to create wasm store: {}", e))?;
        let mut languages = HashMap::new();
        for (ext, path) in grammars {
            let bytes = std::fs::read(path).map_err(|e| {
                format!("treesitter: can't read grammar '{}': {}", path.display(), e)
            })?;
            // WasmStore looks for the exported `tree_sitter_<name>` function, so
            // `name` must be the grammar's language name (e.g. "rust"), derived
            // from the conventional `tree-sitter-<lang>.wasm` filename.
            let name = grammar_lang_name(path);
            let lang = store.load_language(&name, &bytes).map_err(|e| {
                format!(
                    "treesitter: failed to load grammar '{}': {}",
                    path.display(),
                    e
                )
            })?;
            languages.insert(ext.clone(), lang);
        }
        let mut parser = Parser::new();
        parser
            .set_wasm_store(store)
            .map_err(|e| format!("treesitter: failed to attach wasm store: {}", e))?;
        Ok(Self { parser, languages })
    }

    /// Parse a file into its flattened row set. Returns an empty set if the
    /// extension has no grammar, the file is unreadable, or parsing fails.
    fn parse_file(&mut self, rel: &str, abs: &Path, ext: &str) -> HashSet<Row> {
        let lang = match self.languages.get(ext) {
            Some(l) => l.clone(),
            None => return HashSet::new(),
        };
        let src = match std::fs::read_to_string(abs) {
            Ok(s) => s,
            Err(_) => return HashSet::new(), // binary / unreadable / mid-write
        };
        if self.parser.set_language(&lang).is_err() {
            return HashSet::new();
        }
        let tree = match self.parser.parse(&src, None) {
            Some(t) => t,
            None => return HashSet::new(),
        };
        let mut rows = Vec::new();
        flatten(tree.root_node(), -1, 0, rel, &src, &mut rows);
        rows.into_iter().collect()
    }
}

/// Discover candidate source files: (relative path, ext, absolute path).
fn scan_files(
    root: &Path,
    grammars: &HashMap<String, PathBuf>,
    ignore: &HashSet<String>,
) -> Vec<(String, String, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().to_string();
            if ft.is_dir() {
                if !ignore.contains(&name) {
                    stack.push(path);
                }
            } else if ft.is_file() {
                let ext = extension_of(&path);
                if grammars.contains_key(&ext) {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((rel, ext, path));
                }
            }
        }
    }
    out
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl StreamingDataSource for TreeSitterStreamingSource {
    fn schema(&self) -> &DataSchema {
        &self.schema
    }

    fn run(
        self: Box<Self>,
        sender: crossbeam_channel::Sender<StreamingUpdate>,
        shutdown: Arc<AtomicBool>,
    ) {
        let mut engine = match ParseEngine::new(&self.grammars) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("{}", e);
                return;
            }
        };

        // Per-file state: emitted rows and last-seen mtime.
        let mut current: HashMap<String, HashSet<Row>> = HashMap::new();
        let mut mtimes: HashMap<String, SystemTime> = HashMap::new();

        // 1. Seed: parse every file and emit its rows.
        for (rel, ext, abs) in scan_files(&self.root, &self.grammars, &self.ignore) {
            let rows = engine.parse_file(&rel, &abs, &ext);
            for row in &rows {
                if sender
                    .send(StreamingUpdate::Insert(row_to_values(row)))
                    .is_err()
                {
                    return;
                }
            }
            if let Some(t) = mtime(&abs) {
                mtimes.insert(rel.clone(), t);
            }
            current.insert(rel, rows);
        }

        // 2. Watch recursively.
        let (notify_tx, notify_rx) = std::sync::mpsc::channel();
        let mut watcher =
            match notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = notify_tx.send(event);
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("treesitter: failed to create watcher: {}", e);
                    return;
                }
            };
        if let Err(e) = watcher.watch(&self.root, RecursiveMode::Recursive) {
            eprintln!(
                "treesitter: failed to watch '{}': {}",
                self.root.display(),
                e
            );
            return;
        }

        // 3. On change, rescan the file list (cheap) and re-parse only files
        //    that are new or whose mtime changed; delete rows for vanished files.
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            match notify_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(_) => {
                    while notify_rx.try_recv().is_ok() {}
                    std::thread::sleep(Duration::from_millis(50));

                    let found = scan_files(&self.root, &self.grammars, &self.ignore);
                    let present: HashSet<String> =
                        found.iter().map(|(rel, _, _)| rel.clone()).collect();

                    // Deletions: files we tracked that are no longer present.
                    let removed: Vec<String> = current
                        .keys()
                        .filter(|rel| !present.contains(*rel))
                        .cloned()
                        .collect();
                    for rel in removed {
                        if let Some(rows) = current.remove(&rel) {
                            mtimes.remove(&rel);
                            for row in &rows {
                                if sender
                                    .send(StreamingUpdate::Delete(row_to_values(row)))
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }

                    // New or modified files.
                    for (rel, ext, abs) in found {
                        let now = mtime(&abs);
                        let changed = match (mtimes.get(&rel), now) {
                            (Some(prev), Some(now)) => *prev != now,
                            _ => true, // new file, or mtime unavailable -> reparse
                        };
                        if !changed {
                            continue;
                        }
                        let new_rows = engine.parse_file(&rel, &abs, &ext);
                        let old_rows = current.get(&rel).cloned().unwrap_or_default();
                        for row in old_rows.difference(&new_rows) {
                            if sender
                                .send(StreamingUpdate::Delete(row_to_values(row)))
                                .is_err()
                            {
                                return;
                            }
                        }
                        for row in new_rows.difference(&old_rows) {
                            if sender
                                .send(StreamingUpdate::Insert(row_to_values(row)))
                                .is_err()
                            {
                                return;
                            }
                        }
                        if let Some(t) = now {
                            mtimes.insert(rel.clone(), t);
                        }
                        current.insert(rel, new_rows);
                    }
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

    #[test]
    fn parse_grammars_ok() {
        let mut config = HashMap::new();
        config.insert(
            "grammars".to_string(),
            "rs=/a/rust.wasm,py=/b/py.wasm".to_string(),
        );
        let map = parse_grammars(&config).unwrap();
        assert_eq!(map.get("rs"), Some(&PathBuf::from("/a/rust.wasm")));
        assert_eq!(map.get("py"), Some(&PathBuf::from("/b/py.wasm")));
    }

    #[test]
    fn parse_grammars_missing() {
        let config = HashMap::new();
        assert!(parse_grammars(&config).is_err());
    }

    #[test]
    fn parse_grammars_bad_entry() {
        let mut config = HashMap::new();
        config.insert("grammars".to_string(), "justext".to_string());
        assert!(parse_grammars(&config).is_err());
    }

    #[test]
    fn lang_name_from_filename() {
        assert_eq!(
            grammar_lang_name(Path::new("/x/tree-sitter-rust.wasm")),
            "rust"
        );
        assert_eq!(
            grammar_lang_name(Path::new("tree-sitter-typescript.wasm")),
            "typescript"
        );
        assert_eq!(
            grammar_lang_name(Path::new("/x/tree-sitter-c-sharp.wasm")),
            "c_sharp"
        );
        assert_eq!(grammar_lang_name(Path::new("rust.wasm")), "rust");
    }

    #[test]
    fn validate_rejects_unknown() {
        let mut config = HashMap::new();
        config.insert("nope".to_string(), "x".to_string());
        assert!(validate_config(&config).is_err());
    }
}
