# dep2 — live semantic code analysis with Datalog

`dep2` streams a project's filesystem and parsed syntax trees into relations
and runs **incremental Datalog** over them, so query results update live as you
edit code.

```
 project files ──▶ fs plugin ─────▶  files(path, ext)
                                                        ┐
 source code  ──▶ treesitter plugin ─▶ ast_node(...)    ├─▶ FlowLog (incremental Datalog) ─▶ live results
                  (wasm grammars)                        ┘            ▲
 your rules.dl ───────────────────────────────────────────────────────┘
```

It is built on **FlowLog**, an incremental Datalog engine over
[differential-dataflow](https://github.com/TimelyDataflow/differential-dataflow).
When a file changes, only the affected facts are re-derived — insertions and
deletions flow through your rules as `+`/`-` updates.

> This project was forked from an HCL→Datalog tool (DbFlow); the HCL front-end
> was removed in favour of feeding FlowLog its native Datalog directly, plus two
> new streaming plugins (`fs`, `treesitter`).

## How it works

FlowLog runs on `i64` columns internally for speed, but **`string` and `float`
are first-class column types inside the engine**: the engine interns strings to
ids and stores floats as their bit pattern on input, and decodes both back to
text on output (see `crates/reading/src/interner.rs`). So a `.dl` program +
`.facts` files using `string`/`float` columns work with FlowLog standalone — the
codec is an engine feature, not something the dep2 layer bolts on. Relations are
fed by **streaming plugins** that emit insert/delete diffs:

- **`fs`** — walks a project root, seeds `files(path, ext)`, then watches the
  tree and emits diffs as files are created/deleted.
- **`treesitter`** — parses each source file with a tree-sitter grammar loaded
  at runtime from a `.wasm`, flattens the tree into `ast_node` (structural-path
  ids) plus an `ast_span` side table of byte offsets, and **incrementally
  re-parses** on change — streaming only the minimal diff.

Both use **relative paths** with `/` separators, so `files` and `ast_node` join.

### Relation schemas

```
files(path: string, ext: string)

ast_node(file: string, node: string, parent: string, kind: string,
         named: number, text: string)
ast_span(file: string, node: string, start: number, end: number)
```
- `node` — **structural-path id**: `0` is the file root, `0.2` its third child,
  `0.2.1` that node's second child, … `parent` is the parent's path (empty at
  the root). Positional rather than a global counter, so an edit only changes ids
  under the edited subtree — unchanged subtrees keep identical `ast_node` rows
  and fall out of the diff.
- `kind` — grammar node type (`function_item`, `identifier`, `"{"`, …).
- `named` — `1` for named grammar nodes, `0` for anonymous tokens/punctuation.
- `text` — source slice (leaf nodes only).
- byte offsets live in `ast_span`, keyed by `(file, node)` — kept out of the
  structural graph because offsets shift on every insert, which would otherwise
  churn the whole file. Join `ast_span` only when you need positions.

## Build

```bash
cargo build --release
```

The `treesitter` plugin pulls in `wasmtime` (for running wasm grammars), so the
first build takes a few minutes.

## Get a grammar

Grammar `.wasm` files must be built with a tree-sitter CLI matching the
`tree-sitter` crate version, or loading fails (`failed to parse dylink
section`). Use the helper (needs `npm`, plus a local emscripten or Docker):

```bash
scripts/build-grammar.sh tree-sitter-rust ./grammars
# -> ./grammars/tree-sitter-rust.wasm
```

## Run

```
dep2 run <program.dl> --source '[RELATION=]PROVIDER:k=v;k=v...' [--source ...] \
     [--addr 127.0.0.1:7878] [--no-serve] [--print] [-w N]
```

`RELATION` is omitted for multi-output providers (e.g. `treesitter`, which feeds
`ast_node` + `ast_span`); single-output providers (`fs`, `csv`) use it to name
their relation. Config pairs are `;`-separated (so values may contain commas).

The program runs continuously until Ctrl-C, serving a query API (see below) on
`--addr` and keeping the current state of every **terminal** IDB (those not
consumed by another rule) up to date. Pass `--print` to also stream
`+ rel(...)` / `- rel(...)` updates to stdout, or `--no-serve` to skip the API
and just print.

### Examples

List Rust source files (fs plugin):
```bash
dep2 run examples/files.dl --source 'files=fs:root=/path/to/project'
```

Extract Rust function definitions (treesitter plugin):
```bash
dep2 run examples/rust_functions.dl \
  --source 'treesitter:root=/path/to/project;grammars=rs=./grammars/tree-sitter-rust.wasm'
```

Other programs in `examples/`:
- `ast_dump.dl` — every named AST node as `(file, node, kind, text)`.
- `rust_calls.dl` — call graph via a recursive AST-descendant closure.
- `rust_function_spans.dl` — function defs with byte spans (joins `ast_span`).
- `rust_unused_functions.dl` — unused functions via stratified negation.
- `rust_large_functions.dl` — functions over a byte-size threshold (head
  arithmetic + `ast_span` join). Run on this repo's `crates/executing/src`, it
  flags `streaming_program_execution` (~32 KB) and `program_execution` (~28 KB).
- `rust_panic_audit.dl` — `.unwrap()` / `.expect()` call sites with byte offset.

The `grammars=` value maps `ext=path.wasm` (comma-separated for multiple
languages, e.g. `grammars=rs=...rust.wasm,py=...python.wasm`). The language name
is derived from the wasm filename (`tree-sitter-rust.wasm` → `rust`).

## Query the running engine

While `dep2 run` is live it serves the current materialized state of the output
relations over HTTP/JSON, and re-derives it incrementally as files change.

CLI:
```bash
dep2 query                 # list output relations and their row counts
dep2 query func            # dump the rows of relation `func`
dep2 query func --json     # raw JSON
dep2 query --addr HOST:PORT ...
```

HTTP/JSON (curl-friendly):
```
GET /relations             -> { "relations": [ { "name", "count" }, ... ] }
GET /relations/<name>      -> { "name", "count", "rows": [ [col, ...], ... ] }
```
```bash
curl -s http://127.0.0.1:7878/relations/func
```

## Writing rules

Programs are native FlowLog Datalog. Declare streamed relations under `.in`,
derived relations under `.printsize`, and write rules under `.rule`:

```datalog
.in
.decl ast_node(file: string, node: string, parent: string, kind: string, named: number, text: string)

.printsize
.decl func(file: string, name: string)

.rule
func(File, Name) :-
    ast_node(File, F, _, "function_item", _, _),
    ast_node(File, _, F, "identifier", _, Name).
```

Columns are declared `number` (i64), `string`, or `float`. String literals
(`"function_item"`) are interned by the engine and matched against streamed/loaded
string values; `float` columns are stored and compared by value and aggregate
correctly (`min`/`max`/`sum`). Note: `string` ordering (`<`) and float arithmetic
in rule expressions are not supported — strings support equality, floats are
carried/aggregated as data.

## Limitations

- `ast_span` (byte offsets) churns on most edits — offsets after the edit shift,
  so it is *not* minimal-diff. That churn is deliberately isolated to the side
  table; the structural `ast_node` graph stays stable. Avoid joining `ast_span`
  in hot analyses unless you need positions.
- Change *detection* still rescans the directory tree on each event (the `fs`
  plugin) / re-reads changed files (`treesitter`); the re-parse itself is
  incremental. Fine for typical projects.

## Workspace layout

All crates live under `crates/` (a flat workspace, `members = ["crates/*"]`):

```
crates/dep2/                  the CLI binary
crates/dep2-core/             HCL-free engine: string interning + streaming wiring
crates/dep2-plugin/           plugin traits (Plugin, StreamingDataProvider, ...)
crates/dep2-plugin-fs/        filesystem seed + watch
crates/dep2-plugin-treesitter/ wasm-grammar parsing + flatten
crates/dep2-plugin-csv/       CSV streaming (kept as a reference data source)
crates/{parsing,strata,catalog,optimizing,planning,reading,executing,macros,debugging}/
                                the FlowLog incremental Datalog engine
examples/                       example .dl analysis programs
scripts/build-grammar.sh        build an ABI-compatible grammar .wasm
```
