//! Tree-sitter streaming plugin.
//!
//! Parses each source file with a tree-sitter grammar loaded at runtime from a
//! `.wasm` file, flattens the syntax tree, and feeds it into two relations:
//!
//! ```text
//! ast_node(file: string, node: string, parent: string, kind: string,
//!          named: number, text: string)
//! ast_span(file: string, node: string, start: number, end: number)
//! ```
//!
//! - `node` is a **structural path** id: `0` is the file root, `0.2` its third
//!   child, `0.2.1` that node's second child, ... `parent` is the parent's path
//!   (empty for the root). Because the id is positional rather than a global
//!   counter, an edit only changes the ids under the edited subtree (and later
//!   siblings) — unchanged subtrees keep identical `ast_node` rows and so fall
//!   out of the diff entirely.
//! - byte offsets live in the `ast_span` side table keyed by `(file, node)`, so
//!   the structural graph stays stable across edits (offsets shift on every
//!   insert; keeping them out of `ast_node` keeps that churn isolated).
//!
//! On change a file is **incrementally re-parsed** (tree-sitter reuses unchanged
//! subtrees), then the new row sets are diffed against the previous ones and
//! only the delta is streamed.
//!
//! Config keys:
//!   - `root`     (required) project directory to parse and watch.
//!   - `grammars` (required) comma-separated `ext=path.wasm` pairs, e.g.
//!                `rs=/abs/tree-sitter-rust.wasm,py=/abs/tree-sitter-python.wasm`.
//!   - `ignore`   (optional) comma-separated directory names to skip.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use notify::{RecursiveMode, Watcher};
use tree_sitter::{wasmtime::Engine, InputEdit, Language, Node, Parser, Point, Tree, WasmStore};

use dep2_plugin::{
    crossbeam_channel, ColumnDef, DataSchema, DataType, DataValue, Plugin, PluginContext,
    StreamOutput, StreamingDataProvider, StreamingDataSource, StreamingUpdate,
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
const NODE_RELATION: &str = "ast_node";
const SPAN_RELATION: &str = "ast_span";

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

fn col(name: &str, dt: DataType) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: dt,
    }
}

fn node_schema() -> DataSchema {
    DataSchema {
        columns: vec![
            col("file", DataType::String),
            col("node", DataType::String),
            col("parent", DataType::String),
            col("kind", DataType::String),
            col("named", DataType::Integer),
            col("text", DataType::String),
        ],
    }
}

fn span_schema() -> DataSchema {
    DataSchema {
        columns: vec![
            col("file", DataType::String),
            col("node", DataType::String),
            col("start", DataType::Integer),
            col("end", DataType::Integer),
        ],
    }
}

/// Derive the tree-sitter language name from a grammar `.wasm` filename.
/// `tree-sitter-rust.wasm` -> `rust`, `tree-sitter-c-sharp.wasm` -> `c_sharp`.
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

// (file, node, parent, kind, named, text)
type NodeRow = (String, String, String, String, i64, String);
// (file, node, start, end)
type SpanRow = (String, String, i64, i64);

fn node_to_values(r: &NodeRow) -> Vec<DataValue> {
    vec![
        DataValue::String(r.0.clone()),
        DataValue::String(r.1.clone()),
        DataValue::String(r.2.clone()),
        DataValue::String(r.3.clone()),
        DataValue::Integer(r.4),
        DataValue::String(r.5.clone()),
    ]
}

fn span_to_values(r: &SpanRow) -> Vec<DataValue> {
    vec![
        DataValue::String(r.0.clone()),
        DataValue::String(r.1.clone()),
        DataValue::Integer(r.2),
        DataValue::Integer(r.3),
    ]
}

/// Recursively flatten a node, assigning structural-path ids.
fn flatten(
    node: Node,
    path: &str,
    parent_path: &str,
    file: &str,
    src: &str,
    nodes: &mut Vec<NodeRow>,
    spans: &mut Vec<SpanRow>,
) {
    let start = node.start_byte();
    let end = node.end_byte();
    let text = if node.child_count() == 0 {
        src.get(start..end).unwrap_or("").to_string()
    } else {
        String::new()
    };
    nodes.push((
        file.to_string(),
        path.to_string(),
        parent_path.to_string(),
        node.kind().to_string(),
        if node.is_named() { 1 } else { 0 },
        text,
    ));
    spans.push((file.to_string(), path.to_string(), start as i64, end as i64));

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        let mut i = 0usize;
        loop {
            let child_path = format!("{}.{}", path, i);
            flatten(cursor.node(), &child_path, path, file, src, nodes, spans);
            i += 1;
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Build the node and span row sets for a parsed tree. Root id is `0`.
fn build_rows(tree: &Tree, file: &str, src: &str) -> (HashSet<NodeRow>, HashSet<SpanRow>) {
    let mut nodes = Vec::new();
    let mut spans = Vec::new();
    flatten(tree.root_node(), "0", "", file, src, &mut nodes, &mut spans);
    (nodes.into_iter().collect(), spans.into_iter().collect())
}

/// Position of a byte offset as a tree-sitter `Point` (row, byte column).
fn byte_to_point(s: &str, byte: usize) -> Point {
    let mut row = 0;
    let mut col = 0;
    for &b in &s.as_bytes()[..byte.min(s.len())] {
        if b == b'\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    Point { row, column: col }
}

/// Compute the byte range that changed between `old` and `new` as an `InputEdit`
/// (common prefix + common suffix). Lets tree-sitter reuse unchanged subtrees.
fn compute_edit(old: &str, new: &str) -> InputEdit {
    let (ob, nb) = (old.as_bytes(), new.as_bytes());
    let mut start = 0;
    let max_pre = ob.len().min(nb.len());
    while start < max_pre && ob[start] == nb[start] {
        start += 1;
    }
    let mut old_end = ob.len();
    let mut new_end = nb.len();
    while old_end > start && new_end > start && ob[old_end - 1] == nb[new_end - 1] {
        old_end -= 1;
        new_end -= 1;
    }
    InputEdit {
        start_byte: start,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: byte_to_point(old, start),
        old_end_position: byte_to_point(old, old_end),
        new_end_position: byte_to_point(new, new_end),
    }
}

/// Emit the set-difference of `old` and `new` as Delete/Insert updates for
/// `relation`. Returns false if the channel is closed.
fn diff_emit<R: Eq + Hash>(
    sender: &crossbeam_channel::Sender<StreamingUpdate>,
    relation: &str,
    old: &HashSet<R>,
    new: &HashSet<R>,
    to_values: impl Fn(&R) -> Vec<DataValue>,
) -> bool {
    for r in old.difference(new) {
        if sender
            .send(StreamingUpdate::DeleteInto(
                relation.to_string(),
                to_values(r),
            ))
            .is_err()
        {
            return false;
        }
    }
    for r in new.difference(old) {
        if sender
            .send(StreamingUpdate::InsertInto(
                relation.to_string(),
                to_values(r),
            ))
            .is_err()
        {
            return false;
        }
    }
    true
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
            root,
            grammars,
            ignore,
        }))
    }
}

struct TreeSitterStreamingSource {
    root: PathBuf,
    grammars: HashMap<String, PathBuf>,
    ignore: HashSet<String>,
}

/// A loaded parser plus its per-extension languages. Created on the worker
/// thread (wasm types are not `Send`).
struct ParseEngine {
    parser: Parser,
    languages: HashMap<String, Language>,
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

    /// Parse `src` for extension `ext`, optionally reusing `old` for incremental
    /// parsing. Returns None if the extension has no grammar or parsing fails.
    fn parse(&mut self, ext: &str, src: &str, old: Option<&Tree>) -> Option<Tree> {
        let lang = self.languages.get(ext)?.clone();
        self.parser.set_language(&lang).ok()?;
        self.parser.parse(src, old)
    }
}

/// Per-file incremental state, kept on the worker thread.
struct FileState {
    content: String,
    tree: Tree,
    nodes: HashSet<NodeRow>,
    spans: HashSet<SpanRow>,
    mtime: Option<SystemTime>,
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
    fn outputs(&self) -> Vec<StreamOutput> {
        vec![
            StreamOutput {
                relation: NODE_RELATION.to_string(),
                schema: node_schema(),
            },
            StreamOutput {
                relation: SPAN_RELATION.to_string(),
                schema: span_schema(),
            },
        ]
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

        let mut current: HashMap<String, FileState> = HashMap::new();

        // 1. Seed: parse every file and emit its rows.
        for (rel, ext, abs) in scan_files(&self.root, &self.grammars, &self.ignore) {
            let content = match std::fs::read_to_string(&abs) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let tree = match engine.parse(&ext, &content, None) {
                Some(t) => t,
                None => continue,
            };
            let (nodes, spans) = build_rows(&tree, &rel, &content);
            let empty_n = HashSet::new();
            let empty_s = HashSet::new();
            if !diff_emit(&sender, NODE_RELATION, &empty_n, &nodes, node_to_values) {
                return;
            }
            if !diff_emit(&sender, SPAN_RELATION, &empty_s, &spans, span_to_values) {
                return;
            }
            current.insert(
                rel.clone(),
                FileState {
                    content,
                    tree,
                    nodes,
                    spans,
                    mtime: mtime(&abs),
                },
            );
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

        // 3. On change: rescan the file list, re-parse changed files incrementally,
        //    delete rows for vanished files, and stream only the diffs.
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

                    // Deletions: tracked files no longer present.
                    let removed: Vec<String> = current
                        .keys()
                        .filter(|rel| !present.contains(*rel))
                        .cloned()
                        .collect();
                    for rel in removed {
                        if let Some(state) = current.remove(&rel) {
                            let empty_n = HashSet::new();
                            let empty_s = HashSet::new();
                            if !diff_emit(
                                &sender,
                                NODE_RELATION,
                                &state.nodes,
                                &empty_n,
                                node_to_values,
                            ) {
                                return;
                            }
                            if !diff_emit(
                                &sender,
                                SPAN_RELATION,
                                &state.spans,
                                &empty_s,
                                span_to_values,
                            ) {
                                return;
                            }
                        }
                    }

                    // New or modified files.
                    for (rel, ext, abs) in found {
                        let now = mtime(&abs);
                        match current.get_mut(&rel) {
                            Some(state) => {
                                if now == state.mtime {
                                    continue; // unchanged
                                }
                                let new_content = match std::fs::read_to_string(&abs) {
                                    Ok(c) => c,
                                    Err(_) => continue, // mid-write; retry next event
                                };
                                // Incremental re-parse: edit the old tree, reuse it.
                                let edit = compute_edit(&state.content, &new_content);
                                state.tree.edit(&edit);
                                let new_tree =
                                    match engine.parse(&ext, &new_content, Some(&state.tree)) {
                                        Some(t) => t,
                                        None => continue,
                                    };
                                let (new_nodes, new_spans) =
                                    build_rows(&new_tree, &rel, &new_content);
                                if !diff_emit(
                                    &sender,
                                    NODE_RELATION,
                                    &state.nodes,
                                    &new_nodes,
                                    node_to_values,
                                ) {
                                    return;
                                }
                                if !diff_emit(
                                    &sender,
                                    SPAN_RELATION,
                                    &state.spans,
                                    &new_spans,
                                    span_to_values,
                                ) {
                                    return;
                                }
                                state.content = new_content;
                                state.tree = new_tree;
                                state.nodes = new_nodes;
                                state.spans = new_spans;
                                state.mtime = now;
                            }
                            None => {
                                // Newly created file: full parse + seed.
                                let content = match std::fs::read_to_string(&abs) {
                                    Ok(c) => c,
                                    Err(_) => continue,
                                };
                                let tree = match engine.parse(&ext, &content, None) {
                                    Some(t) => t,
                                    None => continue,
                                };
                                let (nodes, spans) = build_rows(&tree, &rel, &content);
                                let empty_n = HashSet::new();
                                let empty_s = HashSet::new();
                                if !diff_emit(
                                    &sender,
                                    NODE_RELATION,
                                    &empty_n,
                                    &nodes,
                                    node_to_values,
                                ) {
                                    return;
                                }
                                if !diff_emit(
                                    &sender,
                                    SPAN_RELATION,
                                    &empty_s,
                                    &spans,
                                    span_to_values,
                                ) {
                                    return;
                                }
                                current.insert(
                                    rel.clone(),
                                    FileState {
                                        content,
                                        tree,
                                        nodes,
                                        spans,
                                        mtime: now,
                                    },
                                );
                            }
                        }
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
        assert!(parse_grammars(&HashMap::new()).is_err());
    }

    #[test]
    fn lang_name_from_filename() {
        assert_eq!(
            grammar_lang_name(Path::new("/x/tree-sitter-rust.wasm")),
            "rust"
        );
        assert_eq!(
            grammar_lang_name(Path::new("tree-sitter-c-sharp.wasm")),
            "c_sharp"
        );
    }

    #[test]
    fn compute_edit_basic() {
        // "abXcd" -> "abYYcd": common prefix "ab", common suffix "cd".
        let e = compute_edit("abXcd", "abYYcd");
        assert_eq!(e.start_byte, 2);
        assert_eq!(e.old_end_byte, 3);
        assert_eq!(e.new_end_byte, 4);
    }

    #[test]
    fn validate_rejects_unknown() {
        let mut config = HashMap::new();
        config.insert("nope".to_string(), "x".to_string());
        assert!(validate_config(&config).is_err());
    }
}
