//! Tree-sitter streaming plugin.
//!
//! Parses each source file with a tree-sitter grammar loaded at runtime from a
//! `.wasm` file, flattens the syntax tree, and feeds it into three relations:
//!
//! ```text
//! ast_node(file: string, node: string, parent: string, kind: string,
//!          named: number, text: string)
//! ast_span(file: string, node: string, start: number, end: number)
//! ast_child(file: string, node: string, idx: number)
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
//! - `ast_child` gives each node's index among its parent's children (root = 0),
//!   so rules can ask positional questions like "the first child / qualifier".
//!
//! On change a file is **incrementally re-parsed** (tree-sitter reuses unchanged
//! subtrees), then the new row sets are diffed against the previous ones and
//! only the delta is streamed.
//!
//! Config keys:
//!   - `root`     (required) project directory to parse and watch.
//!   - `grammars` (required) comma-separated `ext=path.wasm` pairs, e.g.
//!                `rs=/abs/tree-sitter-rust.wasm,py=/abs/tree-sitter-python.wasm`.
//!   - `ignore`   (optional) extra comma-separated directory names to skip, on
//!                top of `.gitignore` (which is always honored).
//!
//! Discovery honors `.gitignore`/`.ignore`/global gitignore and skips hidden
//! entries, so git-ignored files never enter the engine.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
// Fast non-crypto hashing: the per-node row sets are the dominant ingestion cost,
// and the default SipHash hasher made hashing ~70% of build_rows.
use rustc_hash::{FxHashSet, FxHasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

use notify::{RecursiveMode, Watcher};
use tree_sitter::{wasmtime::Engine, InputEdit, Language, Node, Parser, Point, Tree, WasmStore};

use dep2_plugin::{
    ColumnDef, DataSchema, DataType, DataValue, Plugin, PluginContext, SourceRunner, SourceState,
    StreamOutput, StreamingDataProvider, StreamingDataSource, ValueSink,
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
const CHILD_RELATION: &str = "ast_child";
// Raw, language-agnostic line facts so rules can do line-oriented analysis
// (cloc-style counts) that a token AST can't express (blank lines, line numbers).
const LINE_RELATION: &str = "line"; // (file, lang, lineno, blank)
const ASTLINE_RELATION: &str = "ast_line"; // (file, node, start_line, end_line)

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

fn child_schema() -> DataSchema {
    DataSchema {
        columns: vec![
            col("file", DataType::String),
            col("node", DataType::String),
            col("idx", DataType::Integer),
        ],
    }
}

fn line_schema() -> DataSchema {
    DataSchema {
        columns: vec![
            col("file", DataType::String),
            col("lang", DataType::String),
            col("lineno", DataType::Integer),
            col("blank", DataType::Integer),
            // Globally-unique line id so rules can COUNT physical lines: the
            // aggregation counts distinct values, and `(file, lineno)` is the
            // unique line identity (line numbers alone collide across files).
            col("gid", DataType::Integer),
        ],
    }
}

fn astline_schema() -> DataSchema {
    DataSchema {
        columns: vec![
            col("file", DataType::String),
            col("node", DataType::String),
            col("start_line", DataType::Integer),
            col("end_line", DataType::Integer),
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
// (file, node, idx) — node's index among its parent's children (root = 0).
type ChildRow = (String, String, i64);
// (file, lang, lineno, blank, gid) — every physical line; blank = 1 if
// whitespace-only; gid is a globally-unique line id (hash of file+lineno).
type LineRow = (String, String, i64, i64, i64);
// (file, node, start_line, end_line) — node line span (0-based rows).
type AstLineRow = (String, String, i64, i64);

/// The relations a parsed file contributes.
#[derive(Default)]
struct Rows {
    nodes: FxHashSet<NodeRow>,
    spans: FxHashSet<SpanRow>,
    children: FxHashSet<ChildRow>,
    lines: FxHashSet<LineRow>,
    astlines: FxHashSet<AstLineRow>,
}

/// Which output relations the running program actually consumes. We avoid
/// building (and channel-sending) rows for relations no rule reads — on a large
/// repo the unused per-node side tables (ast_span, ast_astline) and ast_line
/// otherwise roughly double the rows funnelled through the ingestion channel.
#[derive(Clone, Copy)]
struct Want {
    spans: bool,
    children: bool,
    lines: bool,
    astlines: bool,
}

impl Want {
    /// Default before the engine tells us otherwise: build everything (preserves
    /// behavior for any caller that never calls `set_wanted`). `nodes` is always
    /// built — it is the core relation and every realistic program uses it.
    fn all() -> Self {
        Want {
            spans: true,
            children: true,
            lines: true,
            astlines: true,
        }
    }

    fn from_set(s: &HashSet<String>) -> Self {
        Want {
            spans: s.contains(SPAN_RELATION),
            children: s.contains(CHILD_RELATION),
            lines: s.contains(LINE_RELATION),
            astlines: s.contains(ASTLINE_RELATION),
        }
    }
}

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

fn child_to_values(r: &ChildRow) -> Vec<DataValue> {
    vec![
        DataValue::String(r.0.clone()),
        DataValue::String(r.1.clone()),
        DataValue::Integer(r.2),
    ]
}

fn line_to_values(r: &LineRow) -> Vec<DataValue> {
    vec![
        DataValue::String(r.0.clone()),
        DataValue::String(r.1.clone()),
        DataValue::Integer(r.2),
        DataValue::Integer(r.3),
        DataValue::Integer(r.4),
    ]
}

fn astline_to_values(r: &AstLineRow) -> Vec<DataValue> {
    vec![
        DataValue::String(r.0.clone()),
        DataValue::String(r.1.clone()),
        DataValue::Integer(r.2),
        DataValue::Integer(r.3),
    ]
}

/// Push the node/span/child diffs between two row bundles (old -> new) into the
/// sink. For a seed or newly-created file, pass `Rows::default()` as `old`; for a
/// deleted file, pass it as `new`. Unwanted relations (skipped by `build_rows`)
/// have empty sets here, so their diff is a no-op.
fn push_rows_diff(sink: &mut dyn ValueSink, old: &Rows, new: &Rows) {
    push_rel_diff(sink, NODE_RELATION, &old.nodes, &new.nodes, node_to_values);
    push_rel_diff(sink, SPAN_RELATION, &old.spans, &new.spans, span_to_values);
    push_rel_diff(
        sink,
        CHILD_RELATION,
        &old.children,
        &new.children,
        child_to_values,
    );
    push_rel_diff(sink, LINE_RELATION, &old.lines, &new.lines, line_to_values);
    push_rel_diff(
        sink,
        ASTLINE_RELATION,
        &old.astlines,
        &new.astlines,
        astline_to_values,
    )
}

/// Recursively flatten a node, assigning structural-path ids. `idx` is the
/// node's index among its parent's children (the root is index 0).
fn flatten(
    node: Node,
    path: &str,
    parent_path: &str,
    idx: i64,
    file: &str,
    src: &str,
    want: Want,
    out: &mut Rows,
) {
    let start = node.start_byte();
    let end = node.end_byte();
    // Emit source text for any node with no *named* children — true leaves
    // (identifiers, keywords) as before, plus value nodes whose only children are
    // anonymous tokens (e.g. a TOML `string` is `"` content `"`, a YAML flow
    // scalar). This makes config values like Cargo.toml `name = "x"` readable in
    // Datalog; callers strip surrounding quotes as needed.
    let text = if node.named_child_count() == 0 {
        src.get(start..end).unwrap_or("").to_string()
    } else {
        String::new()
    };
    out.nodes.insert((
        file.to_string(),
        path.to_string(),
        parent_path.to_string(),
        node.kind().to_string(),
        if node.is_named() { 1 } else { 0 },
        text,
    ));
    if want.spans {
        out.spans
            .insert((file.to_string(), path.to_string(), start as i64, end as i64));
    }
    if want.children {
        out.children
            .insert((file.to_string(), path.to_string(), idx));
    }
    if want.astlines {
        out.astlines.insert((
            file.to_string(),
            path.to_string(),
            node.start_position().row as i64,
            node.end_position().row as i64,
        ));
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        let mut i = 0i64;
        loop {
            let child_path = format!("{}.{}", path, i);
            flatten(cursor.node(), &child_path, path, i, file, src, want, out);
            i += 1;
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Build the relation row sets for a parsed tree. Root id is `0`. `lang` is the
/// grammar's language name, attached to each physical line.
fn build_rows(tree: &Tree, file: &str, src: &str, lang: &str, want: Want) -> Rows {
    let mut rows = Rows::default();
    flatten(tree.root_node(), "0", "", 0, file, src, want, &mut rows);
    if !want.lines {
        return rows;
    }
    // Raw physical lines (0-based, matching tree-sitter rows): `str::lines` gives
    // the physical line count with no spurious trailing empty after a final '\n'.
    for (i, line) in src.lines().enumerate() {
        let blank = if line.trim().is_empty() { 1 } else { 0 };
        // Globally-unique, stable line id: hash of (file, lineno). Distinct per
        // physical line so rules can COUNT lines (line numbers alone collide
        // across files); stable across edits so the streamed diff stays minimal.
        let mut h = FxHasher::default();
        file.hash(&mut h);
        (i as u64).hash(&mut h);
        let gid = (h.finish() >> 1) as i64; // >> 1 keeps it positive (off the NULL sentinel)
        rows.lines
            .insert((file.to_string(), lang.to_string(), i as i64, blank, gid));
    }
    rows
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

/// Push the set-difference of `old` and `new` for `relation` into the sink
/// (retract rows only in `old`, insert rows only in `new`).
fn push_rel_diff<R: Eq + Hash>(
    sink: &mut dyn ValueSink,
    relation: &str,
    old: &FxHashSet<R>,
    new: &FxHashSet<R>,
    to_values: impl Fn(&R) -> Vec<DataValue>,
) {
    for r in old.difference(new) {
        sink.push(relation, &to_values(r), -1);
    }
    for r in new.difference(old) {
        sink.push(relation, &to_values(r), 1);
    }
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
            want: Want::all(),
        }))
    }
}

struct TreeSitterStreamingSource {
    root: PathBuf,
    grammars: HashMap<String, PathBuf>,
    ignore: HashSet<String>,
    want: Want,
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
            // A grammar that can't be read or loaded (e.g. one needing an external
            // scanner the wasm runtime doesn't provide) is skipped rather than
            // aborting the whole source — other grammars still work.
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("treesitter: skipping grammar '{}': {}", path.display(), e);
                    continue;
                }
            };
            let name = grammar_lang_name(path);
            match store.load_language(&name, &bytes) {
                Ok(lang) => {
                    languages.insert(ext.clone(), lang);
                }
                Err(e) => {
                    eprintln!(
                        "treesitter: skipping grammar '{}' (failed to load): {}",
                        path.display(),
                        e
                    );
                }
            }
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
    // `None` right after the parallel seed (trees aren't `Send`, so they can't be
    // collected off the parse threads); populated on the first watch re-parse,
    // enabling incremental re-parse from then on.
    tree: Option<Tree>,
    rows: Rows,
    mtime: Option<SystemTime>,
}

/// Discover candidate source files: (relative path, ext, absolute path).
///
/// Walks with the `ignore` crate so git-ignored paths never enter the engine:
/// it honors `.gitignore`/`.ignore`/global gitignore/`.git/info/exclude` (and
/// parent dirs up to the repo root) and skips hidden entries. `ignore` (the
/// configured/default directory names) is applied on top, so `target`,
/// `node_modules`, etc. are skipped even in projects without a `.gitignore`.
fn scan_files(
    root: &Path,
    grammars: &HashMap<String, PathBuf>,
    ignore: &HashSet<String>,
) -> Vec<(String, String, PathBuf)> {
    let names = ignore.clone();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(move |e| !names.contains(&*e.file_name().to_string_lossy()))
        .build();

    let mut out = Vec::new();
    for entry in walker.flatten() {
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let ext = extension_of(path);
        if grammars.contains_key(&ext) {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, ext, path.to_path_buf()));
        }
    }
    // Deterministic order (filesystem walk order isn't guaranteed stable), so a
    // capped subset is reproducible and the seed is sharded consistently.
    out.sort_by(|a, b| a.0.cmp(&b.0));
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
            StreamOutput {
                relation: CHILD_RELATION.to_string(),
                schema: child_schema(),
            },
            StreamOutput {
                relation: LINE_RELATION.to_string(),
                schema: line_schema(),
            },
            StreamOutput {
                relation: ASTLINE_RELATION.to_string(),
                schema: astline_schema(),
            },
        ]
    }

    fn set_wanted(&mut self, wanted: &HashSet<String>) {
        self.want = Want::from_set(wanted);
    }

    fn build(self: Box<Self>) -> Box<dyn SourceRunner> {
        let lang_of: HashMap<String, String> = self
            .grammars
            .iter()
            .map(|(ext, path)| (ext.clone(), grammar_lang_name(path)))
            .collect();
        // ParseEngine holds wasm types (not Send); create it here, on the worker
        // thread that will step this runner.
        let engine = match ParseEngine::new(&self.grammars) {
            Ok(e) => Some(e),
            Err(e) => {
                eprintln!("{}", e);
                None
            }
        };
        let mut files = scan_files(&self.root, &self.grammars, &self.ignore);
        if let Ok(cap) = std::env::var("DEP2_MAX_FILES") {
            if let Ok(n) = cap.parse::<usize>() {
                files.truncate(n);
            }
        }
        Box::new(TreeSitterRunner {
            grammars: self.grammars,
            ignore: self.ignore,
            root: self.root,
            want: self.want,
            lang_of,
            engine,
            files,
            cursor: 0,
            current: HashMap::new(),
            dbg_timing: std::env::var("DEP2_TS_TIMING").is_ok(),
            t_seed: Instant::now(),
            seed_done: false,
            watch: None,
        })
    }
}

/// The running tree-sitter source: parses one file per `step` (so results stream
/// in live), then watches the tree for edits. Holds the (non-`Send`) wasm parser,
/// so it is built and stepped entirely on one worker thread.
struct TreeSitterRunner {
    grammars: HashMap<String, PathBuf>,
    ignore: HashSet<String>,
    root: PathBuf,
    want: Want,
    lang_of: HashMap<String, String>,
    engine: Option<ParseEngine>,
    /// Seed: the file list and a cursor — one file is parsed per `step`.
    files: Vec<(String, String, PathBuf)>,
    cursor: usize,
    current: HashMap<String, FileState>,
    dbg_timing: bool,
    t_seed: Instant,
    seed_done: bool,
    watch: Option<TsWatch>,
}

/// Live-edit watch state (armed once the seed finishes).
struct TsWatch {
    notify_rx: std::sync::mpsc::Receiver<notify::Event>,
    /// Held to keep the watcher alive.
    _watcher: notify::RecommendedWatcher,
}

impl TreeSitterRunner {
    fn lang_for(&self, ext: &str) -> String {
        self.lang_of
            .get(ext)
            .cloned()
            .unwrap_or_else(|| ext.to_string())
    }

    /// Parse the next seed file and push its rows.
    fn step_seed(&mut self, sink: &mut dyn ValueSink) {
        let idx = self.cursor;
        self.cursor += 1;
        let (rel, ext, abs) = self.files[idx].clone();
        let content = match std::fs::read_to_string(&abs) {
            Ok(c) => c,
            Err(_) => return,
        };
        let tree = match self.engine.as_mut().unwrap().parse(&ext, &content, None) {
            Some(t) => t,
            None => return,
        };
        let lang = self.lang_for(&ext);
        let rows = build_rows(&tree, &rel, &content, &lang, self.want);
        push_rows_diff(sink, &Rows::default(), &rows);
        self.current.insert(
            rel,
            FileState {
                content,
                tree: None,
                rows,
                mtime: mtime(&abs),
            },
        );
    }

    /// Arm the recursive file watcher (after the seed).
    fn arm_watch(&mut self) {
        let (notify_tx, notify_rx) = std::sync::mpsc::channel();
        if let Ok(mut watcher) =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = notify_tx.send(event);
                }
            })
        {
            if watcher.watch(&self.root, RecursiveMode::Recursive).is_ok() {
                self.watch = Some(TsWatch {
                    notify_rx,
                    _watcher: watcher,
                });
            } else {
                eprintln!("treesitter: failed to watch '{}'", self.root.display());
            }
        }
    }

    /// Rescan after a change: delete vanished files, re-parse changed ones
    /// (incrementally when a tree is cached), and push only the diffs.
    fn process_changes(&mut self, sink: &mut dyn ValueSink) {
        let found = scan_files(&self.root, &self.grammars, &self.ignore);
        let present: HashSet<String> = found.iter().map(|(r, _, _)| r.clone()).collect();

        let removed: Vec<String> = self
            .current
            .keys()
            .filter(|r| !present.contains(*r))
            .cloned()
            .collect();
        for rel in removed {
            if let Some(state) = self.current.remove(&rel) {
                push_rows_diff(sink, &state.rows, &Rows::default());
            }
        }

        let want = self.want;
        for (rel, ext, abs) in found {
            let now = mtime(&abs);
            let lang = self.lang_for(&ext);
            if self.current.contains_key(&rel) {
                let state = self.current.get_mut(&rel).unwrap();
                if now == state.mtime {
                    continue; // unchanged
                }
                let new_content = match std::fs::read_to_string(&abs) {
                    Ok(c) => c,
                    Err(_) => continue, // mid-write; retry next event
                };
                // Incremental re-parse when we already cached the tree; after the
                // seed it's absent, so the first edit does a full re-parse.
                let parsed = match &mut state.tree {
                    Some(old_tree) => {
                        let edit = compute_edit(&state.content, &new_content);
                        old_tree.edit(&edit);
                        self.engine
                            .as_mut()
                            .unwrap()
                            .parse(&ext, &new_content, Some(&*old_tree))
                    }
                    None => self
                        .engine
                        .as_mut()
                        .unwrap()
                        .parse(&ext, &new_content, None),
                };
                let new_tree = match parsed {
                    Some(t) => t,
                    None => continue,
                };
                let new_rows = build_rows(&new_tree, &rel, &new_content, &lang, want);
                push_rows_diff(sink, &state.rows, &new_rows);
                state.content = new_content;
                state.tree = Some(new_tree);
                state.rows = new_rows;
                state.mtime = now;
            } else {
                // Newly created file: full parse + seed.
                let content = match std::fs::read_to_string(&abs) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let tree = match self.engine.as_mut().unwrap().parse(&ext, &content, None) {
                    Some(t) => t,
                    None => continue,
                };
                let rows = build_rows(&tree, &rel, &content, &lang, want);
                push_rows_diff(sink, &Rows::default(), &rows);
                self.current.insert(
                    rel,
                    FileState {
                        content,
                        tree: Some(tree),
                        rows,
                        mtime: now,
                    },
                );
            }
        }
    }
}

impl SourceRunner for TreeSitterRunner {
    fn step(&mut self, sink: &mut dyn ValueSink, shutdown: &AtomicBool) -> SourceState {
        if shutdown.load(Ordering::Relaxed) || self.engine.is_none() {
            return SourceState::Idle;
        }

        // 1. Seed: one file per step, so the graph fills in live.
        if self.cursor < self.files.len() {
            self.step_seed(sink);
            return SourceState::Pending;
        }

        // 2. Seed just finished: report timing, free the file list, arm the watcher.
        if !self.seed_done {
            self.seed_done = true;
            if self.dbg_timing {
                let wall = self.t_seed.elapsed();
                eprintln!(
                    "[ts seed] {} files in {:.1}s ({:.0}/s)",
                    self.current.len(),
                    wall.as_secs_f64(),
                    self.current.len() as f64 / wall.as_secs_f64().max(1e-9),
                );
            }
            self.files = Vec::new();
            self.arm_watch();
        }

        // 3. Watch: drain pending OS events non-blocking; rescan + diff on change.
        let changed = match self.watch.as_ref() {
            Some(w) => {
                let mut c = false;
                while w.notify_rx.try_recv().is_ok() {
                    c = true;
                }
                c
            }
            None => return SourceState::Idle,
        };
        if !changed {
            return SourceState::Idle;
        }
        self.process_changes(sink);
        SourceState::Pending
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

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// `compute_edit` must describe a valid old->new splice: the bytes
            /// before `start` and after the end markers are unchanged, and
            /// reconstructing new from old via the edit yields exactly `new`.
            /// This is the contract tree-sitter relies on for incremental parse.
            #[test]
            fn compute_edit_reconstructs(old in ".{0,40}", new in ".{0,40}") {
                let e = compute_edit(&old, &new);
                let ob = old.as_bytes();
                let nb = new.as_bytes();

                // bounds
                prop_assert!(e.start_byte <= e.old_end_byte && e.old_end_byte <= ob.len());
                prop_assert!(e.start_byte <= e.new_end_byte && e.new_end_byte <= nb.len());

                // common prefix and suffix are genuinely common
                prop_assert_eq!(&ob[..e.start_byte], &nb[..e.start_byte]);
                prop_assert_eq!(&ob[e.old_end_byte..], &nb[e.new_end_byte..]);

                // reconstruction: prefix ++ new-middle ++ suffix == new
                let mut rebuilt = Vec::new();
                rebuilt.extend_from_slice(&ob[..e.start_byte]);
                rebuilt.extend_from_slice(&nb[e.start_byte..e.new_end_byte]);
                rebuilt.extend_from_slice(&ob[e.old_end_byte..]);
                prop_assert_eq!(rebuilt, nb.to_vec());
            }

            /// Identical inputs yield an empty edit (nothing to re-parse).
            #[test]
            fn compute_edit_identity_is_empty(s in ".{0,40}") {
                let e = compute_edit(&s, &s);
                prop_assert_eq!(e.start_byte, s.len());
                prop_assert_eq!(e.old_end_byte, s.len());
                prop_assert_eq!(e.new_end_byte, s.len());
            }

            /// Language name derivation strips the conventional prefix and
            /// normalises dashes, and never contains a dash.
            #[test]
            fn lang_name_has_no_dash(stem in "[a-z][a-z0-9_-]{0,12}") {
                let p = std::path::PathBuf::from(format!("/g/tree-sitter-{}.wasm", stem));
                let name = grammar_lang_name(&p);
                prop_assert!(!name.contains('-'), "dash in {}", name);
                prop_assert_eq!(name, stem.replace('-', "_"));
            }
        }
    }
}
