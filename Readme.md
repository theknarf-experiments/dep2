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
ast_child(file: string, node: string, idx: number)
ast_line(file: string, node: string, start_line: number, end_line: number)
line(file: string, lang: string, lineno: number, blank: number, gid: number)
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
- `ast_child` gives each node's index among its siblings (root = 0), so rules can
  ask positional questions ("the first child / qualifier"). Join when you need
  order.
- `ast_line` gives each node's 0-based line span; `line` is one row per *physical*
  line (`blank` = 1 if whitespace-only, `gid` a unique line id for counting).
  These are raw, language-agnostic facts that a token AST can't otherwise express
  (blank lines, line numbers) — they let line-oriented analyses (e.g. `cloc.dl`)
  be written purely as rules. Join only when you need line-level counts.

## Setup

A fresh clone is ready to develop with three commands:

```bash
mise trust && mise install && mise run setup
```

`mise run setup` activates the git hooks, builds the tree-sitter grammars the
default `mise run graph` uses into `./grammars`, installs the web deps, and
builds the engine (debug). It needs `npm` plus a local emscripten or Docker (to
compile grammars to wasm) and `pnpm` (for the web UI). Already-built grammars are
skipped, so re-running is cheap.

For a performance build of the engine, do a release build (the `treesitter`
plugin pulls in `wasmtime`, so the first build takes a few minutes):

```bash
cargo build --release
```

### Adding a grammar

`mise run setup` builds the grammars `mise run graph` uses (Rust, JS/TS, JSON,
TOML, HTML, CSS, Markdown, MDX). To add another, use the `build-grammar` task —
grammar `.wasm` files must be built with a tree-sitter CLI matching the
`tree-sitter` crate version, or loading fails (`failed to parse dylink section`):

```bash
mise run build-grammar tree-sitter-python
# -> ./grammars/tree-sitter-python.wasm
```

Some grammars need extra arguments — a subdirectory (the package ships several
grammars, pass `<out-dir> <subdir> <wasm-name>`) or a `github:` source (no npm
release):

```bash
mise run build-grammar tree-sitter-typescript grammars typescript tree-sitter-typescript
mise run build-grammar github:srazzak/tree-sitter-mdx
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
- `rust_calls.dl` — call graph: each call attributed to its nearest enclosing
  function via a linear `enclosing(node, fn)` closure (scales to large files).
  Matches plain `foo(..)`, path `Path::foo(..)` and method `recv.foo(..)` calls.
- `rust_recursive_fns.dl` — recursive functions (self or mutual) via the
  transitive closure of the call graph. Methods are qualified by their `impl`
  type and calls resolved per type (`Self::f`, `Type::f`, `self.f()`), so
  same-named methods of different types are *not* conflated; free functions match
  by name. Receiver-typed calls (`self.field.f()`) are skipped (no type
  inference). On `crates/strata/src` it precisely flags `processing_order_dfs`
  and `assigning_scc_dfs` (stratified negation + recursion).
- `rust_function_spans.dl` — function defs with byte spans (joins `ast_span`).
- `rust_unused_functions.dl` — unused functions via stratified negation.
- `rust_dead_code.dl` — workspace-wide dead functions: defined but never called
  anywhere (cross-file negation). Counts plain/path/method/turbofish calls plus
  macro-body calls, and excludes `pub` / `main` / `#[test]`. Read the *settled*
  result via `dep2 query dead_fn` (the live stream over-approximates then
  retracts). Residual false positives are trait-dispatched methods and functions
  passed by name — on this workspace those are the only hits, i.e. no true dead
  code. Point the source at a directory spanning all crates.
- `rust_large_functions.dl` — functions over a byte-size threshold (head
  arithmetic + `ast_span` join). Run on this repo's `crates/executing/src`, it
  flags `streaming_program_execution` (~32 KB) and `program_execution` (~28 KB).
- `rust_panic_audit.dl` — `.unwrap()` / `.expect()` call sites with byte offset.
- `rust_panic_propagation.dl` — functions that can panic *transitively*: direct
  `.unwrap()`/`.expect()` closed over the call graph (`can_panic`). Name-based and
  file-local, so it over-approximates; recursion is monotone reachability.
- `rust_xpanic.dl` — the cross-file, precise version: transitive panic closed
  over the *type-qualified* cross-file call graph (functions keyed by AST node id
  so edges compose across files). On `crates/strata/src`, `main` is flagged
  because it calls `Strata::from_parser` (resolved cross-file) which can panic.
  Method calls `recv.f()` are skipped, as in `rust_xcalls_typed`.
- `rust_xcalls.dl` — cross-file call graph: resolves each call to the file(s)
  defining a function of that name and surfaces edges that cross a file boundary
  (`cross_file_call`). On `crates/strata/src` it recovers e.g.
  `stratification.rs::from_parser` → `rewrite.rs::desugar_recursive_aggregation`.
  Name-based, so a name defined in several files resolves to all of them.
- `rust_xcalls_typed.dl` — type-qualified cross-file call graph: resolves
  `Type::method` to *that* type's method (and free functions by name), so
  same-named methods aren't conflated. On `crates/strata/src` it trims the
  name-based graph from ~15 edges (8 of them false test→`from_parser` resolutions)
  to 3 correct ones. Trade-off: method calls `recv.f()` are skipped (receiver
  type needs inference), which the name-based version catches.
- `rust_hot_fns.dl` — "hot" functions by fan-in: how many distinct functions call
  each workspace-defined name, via a cross-file `count` aggregation over the call
  graph (`fanin`; sort descending for the top). On `crates/executing/src` the
  internal leaders are `new`, `jn_compare`, `aggregate_values`, the `*_deconstructor`s.
- `rust_imports.dl` — cross-file import / module graph: `mod` declarations and
  each file's external crate/module dependencies (root segment of every `use`
  path, via a child-0 descent closure over `ast_child`). On `crates/executing/src`
  it reconstructs the crate's dependencies (planning, reading, catalog, …) and
  module tree.
- `rust_crate_deps.dl` — **crate-level** dependency graph: groups `rust_imports`'
  per-file deps by importing crate via `split_nth(File, "/", 0)` (run with
  `root=crates`). `intra_dep` is the workspace-internal graph and matches the
  Cargo dependency graph (e.g. `executing` → catalog/parsing/planning/reading/
  strata/macros/debugging). Demonstrates the string builtins enabling crate-aware
  queries.
- `rust_crate_coupling.dl` — crate coupling metrics over the internal dependency
  graph: `afferent(c, n)` (how many crates depend on `c`) and `efferent(c, n)`
  (how many `c` depends on), via `count` aggregation grouped by crate. On this
  repo the foundational crates are `parsing` (afferent 6), `catalog` (4); the
  high-level ones `executing`/`dep2_core` (efferent 7). Crate names are normalised
  with `replace(.., "-", "_")` so hyphenated dirs (`dep2-core`, `dep2-plugin-*`)
  resolve against their `use` names.
- `rust_crate_reach.dl` — transitive crate reachability: the closure of the
  internal dependency graph (recursion at crate granularity). `tdep_count(c, n)`
  is each crate's transitive fan-out (`dep2` 11, `dep2_core` 10, `executing` 8);
  `indirect_only(from, to)` is reachable-but-not-direct couplings (e.g.
  `executing → optimizing`, only via `planning`).
- `rust_crate_depth.dl` — crate dependency depth (architectural layer): the
  longest dependency chain per crate, via recursive `max` aggregation over the
  crate DAG. The layering comes out 0 (`parsing`) → 1 (`catalog`, `reading`,
  `strata`) → 2 (`macros`, `optimizing`) → 3 `planning` → 4 `executing` → 5
  `dep2_core` → 6 `dep2`.
- `rust_pubcrate.dl` — crate-aware refactoring hint: fully-`pub` functions called
  only from within their own crate (`pubcrate_candidate`) — candidates to demote
  to `pub(crate)`. Joins pub-fn defs (keyed by crate) against the per-crate call
  set and excludes names called from any other crate (stratified negation +
  `split_nth` + `!=` on crate names). Conservative by name (under-suggests). On
  this repo it flags 55, e.g. `executing::program_execution` and `eval_builtin`,
  while correctly keeping `streaming_program_execution` (used by `dep2-core`).

The analyses are **language-generic** — the engine and rule style don't change,
only the tree-sitter node-kind vocabulary does. JavaScript / TypeScript examples:

- `js_functions.dl` — JS/TS function definitions across the three forms:
  `function f(){}` (function_declaration), `class C { m(){} }` (method_definition),
  and `const f = () => {}` (arrow_function bound to a name).
- `js_calls.dl` — JS/TS call graph (nearest-enclosing-function attribution +
  plain/method calls), the same shape as `rust_calls.dl`. The *same* files run on
  `.ts` (with type annotations / `interface` / `implements`) using the TypeScript
  grammar — e.g. on a `.ts` file they recover `fact → fact` (recursion),
  `dbl → fact`, `area → side` (a `this.side()` method call).
- `cloc.dl` — a cloc-style line counter (`code`/`comment`/`blank`) grouped by
  crate and — unlike cloc — **splitting Rust's in-file `#[cfg(test)] mod` tests
  out from production code**, which a line-oriented tool can't do because the test
  code lives in the same file. The test region is found by a rule over the AST;
  all classification is rules over the raw `line`/`ast_line` facts. Its four
  buckets exactly partition every physical line (`code+test+comment+blank` =
  `wc -l`), and it reveals what cloc can't — e.g. `executing` is roughly half test
  code — that cloc lumps into one per-language number.
- `poly_recursive_fns.dl` — **cross-language** recursive-function detection over a
  mixed Rust + JS + TS tree parsed in one engine (`grammars=rs=...,js=...,ts=...`,
  all files sharing one `ast_node` relation). Small per-language *frontends*
  normalise each AST into shared `is_fn`/`fn_name`/`calls` relations, then one
  language-agnostic core computes the closure — flagging `rfact` (Rust), `jfib`
  (JS), and `tsum`/`loop` (TS) in a single run.

The `grammars=` value maps `ext=path.wasm` (comma-separated for multiple
languages, e.g. `grammars=rs=...rust.wasm,js=...javascript.wasm,ts=...typescript.wasm`).
The language name is derived from the wasm filename (`tree-sitter-rust.wasm` →
`rust`).

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

Querying a relation that the program computes but doesn't serve (an intermediate
consumed by another rule, not declared `.out`) returns a `404` explaining it —
*"relation 'X' is computed but not served (consumed by Y); declare it under .out
to expose it"* — rather than a bare "unknown relation".

## Add queries at runtime

A running engine accepts new Datalog *queries* without restarting: a query is a
full `.dl` program whose `.in` decls name relations the base program publishes
(its inputs and served derived relations). The engine compiles and validates it
up front, then every worker instantiates it as its own dataflow that **imports**
the published relations' shared arrangements — it replays their entire history,
then follows live updates incrementally, exactly as if its rules had been part
of the program from the start. Queries can join, aggregate, negate, and recurse
like any program.

```
GET    /query                        -> { "queries": [id, ...] }
POST   /query {"id","program"}       add a query
DELETE /query/<id>                   drop a query (its dataflow retires)
GET    /query/<id>/relations         list the query's relations
GET    /query/<id>/relations/<name>  the query's live rows
```
```bash
curl -s -X POST 127.0.0.1:7878/query -d '{
  "id": "fanout",
  "program": ".in\n.decl file_link(src: string, dst: string)\n.printsize\n.decl fanout(src: string, c: number)\n.rule\nfanout(S, count(D)) :- file_link(S, D).\n"
}'
curl -s 127.0.0.1:7878/query/fanout/relations/fanout
```

Invalid programs (parse/typing errors, unknown base relations, schema
mismatches) are rejected at `POST` time with the front-end's rendered report.
Publishing costs one extra whole-row arrangement per base relation, compacted
as the engine runs so trace memory stays bounded.

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

By default the query API serves only **terminal** relations — those not consumed
by another rule's body — so intermediate/scratch relations stay quiet (and aren't
materialized live). To expose a relation that *is* consumed by another rule,
declare it under `.out` instead of `.printsize`; `.out` force-serves it over the
query API:

```datalog
.out
.decl reach(from: string, to: string)   // served even though other rules use it
```

Columns are declared `number` (i64), `string`, or `float`. String literals
(`"function_item"`) are interned by the engine and matched against streamed/loaded
string values.

**Expressions.** Arithmetic chains evaluate **left-to-right** (no operator
precedence: `A - B * 2` is `(A - B) * 2`); parentheses group explicitly
(`A - (B * 2)`). Float literals are written with a decimal point (`1.5`), and a
typing pass resolves every expression's mode from the declared column types:
comparisons, arithmetic and `sum`/`min`/`max` over `float` columns evaluate as
floats. Mixing `float` with `number` in one expression is a load-time error —
there is no *implicit* conversion; `to_float(n)` and `round(f)`/`floor(f)` are
the explicit bridges:

```datalog
light(N, W) :- sample(N, W), W < 1.5.            // float compare
mid(N, (A + B) / 2.0) :- sample(N, A), sample(N, B). // parenthesised float arithmetic
usd(P, to_float(C) / 100.0) :- cost(P, C).       // number -> float
whole(P, round(to_float(C) / 100.0)) :- cost(P, C).  // ... and back
```

**Lattice merge (`merge(min)` / `merge(max)`).** An aggregate written in a rule
head (`p(K, min(V)) :- ...`) reads as rule-local, but the reduce actually runs
once where *every* rule's contributions meet — so it silently applies to the
whole relation. `merge(op)` says that directly, on the declaration: the relation
is a **function** from its leading columns (the key) to its last (a lattice
value), and every rule deriving into it contributes a candidate that gets folded:

```datalog
.out
.decl path(x: number, y: number, len: number) merge(min)

.rule
path(X, Y, L) :- edge(X, Y, L).                          // rules stay plain —
path(X, Z, L1 + L2) :- path(X, Y, L1), edge(Y, Z, L2).   // the decl does the fold
```

`path` keeps exactly one row per `(x, y)`. Rules need no aggregate of their own,
so a rule can be added without knowing about the fold and still participate
correctly. Only `min` and `max` are accepted: the fold has to be idempotent,
associative and commutative (a lattice join) to be monotone in the lattice
order, which is what lets it run *inside* the recursive fixpoint. `sum`/`count`/
`avg` are not idempotent and stay head aggregations. The value column must be
`number` or `float` — `string` columns are interned ids, so their ordering is
meaningless.

The same reasoning makes two rules of one relation asking for *different*
aggregates (`min` in one head, `max` in another) a load-time error: the fold
runs once, so one of them would be silently ignored. Declare it once with
`merge(op)`. See `examples/module_distance.dl` for a worked case — shortest
dependency chains between modules, where without the merge you would get a row
per distinct path length for every pair.

(The idea is borrowed from [egglog](https://github.com/egraphs-good/egglog)'s
`:merge`, which uses the same mechanism for both lattice analyses and, when the
merge is *union*, congruence closure.)

**Load-time validation.** Programs are checked when loaded, with the offending
rule in the message: atom arity against declarations, head/negation/comparison
variables bound by a positive body atom, no variable joined across columns of
different types, conversion builtins applied to the right kind. The engine also
verifies every streaming source's schema (arity + column types) against the
`.in` declaration it feeds — a `.decl` typo fails startup instead of feeding
silently-garbled rows.

Errors are rendered as labelled source snippets (the `syntax` crate — a
chumsky parser with ariadne reports, exercised by the whole example corpus
and by property tests over generated programs):

```text
Error: unknown column type `nmber` — the types are number, string and float
   ╭─[ rules.dl:2:23 ]
   │
 2 │ .decl e(x: number, y: nmber)
   │                       ┬
   │                       ╰── unknown column type `nmber` — …
───╯
```

### String builtins

String operators are exposed as **builtin functions** usable anywhere a factor
is (head expressions and comparison operands). They decode the interned ids back
to text, so they work on `string` columns and string literals:

- `split_nth(s, sep, n)` — the n-th `sep`-separated segment of `s` (a string).
  E.g. `crate_of(File, split_nth(File, "/", 0))` extracts the leading path
  segment.
- `replace(s, from, to)` — `s` with every `from` replaced by `to` (a string).
- `starts_with(s, prefix)`, `contains(s, needle)`, `str_before(a, b)`
  (lexicographic `a < b`) — these return `1`/`0`, so use them as a filter with
  `= 1`, e.g. `r(F) :- files(F, _), starts_with(F, "src/") = 1.`
- `to_lower(s)` / `to_upper(s)` — case folding, e.g.
  `contains(to_lower(F), "readme") = 1` for case-insensitive matching.
- `extract_number(s)` — the first integer in `s` as a *number* (digit runs may
  contain `,` separators: `"a backlog of 47,500 rows"` → 47500; NULL if no
  digits). The bridge from text to arithmetic.
- `date_epoch(s)` — Unix epoch seconds of an ISO-8601 timestamp
  (`"2026-07-20T00:00:00Z"`; date-only accepted, numeric offsets rejected;
  NULL if unparseable). The bridge from date columns to time arithmetic —
  pair it with the `clock` plugin's `now(iso, epoch)` heartbeat relation:
  `soon(N) :- deadline(N, D), now(_, E), date_epoch(D) < E + 604800.`
- `to_float(n)`, `round(f)`, `floor(f)` — explicit numeric conversions (see
  *Expressions* above); strictly typed, so `to_float` of a float is an error.
- `ln(f)`, `exp(f)`, `sqrt(f)`, `pow(f, f)` — float math. Would-be-NaN results
  (`ln`/`sqrt` of a negative) are NULL; infinities keep their IEEE-754 value.
  `ln(P / (1.0 - P))` is the log-odds transform.
- `abs(x)` — polymorphic: number → number, float → float (the typing pass
  resolves the mode). Replaces the two-symmetric-rules idiom for magnitudes.
- `similarity(a, b)` — how alike two strings are, 0..100 (Sørensen–Dice over
  character bigrams). Case-sensitive; compose with `to_lower` for fuzzy
  joins: `pair(A, B) :- x(A), y(B), similarity(to_lower(A), to_lower(B)) > 85.`

Builtins compose (`split_nth(split_nth(P, "/", 0), "_", 0)`), take full
expressions as arguments (`round(to_float(C) / 100.0)`), and propagate NULL.
Boolean builtins are written `f(..) = 1` because a bare `f(..)` in body position
parses as a relation atom.

### The clock plugin

Time is available as a relation: the `clock` plugin (no network, no files)
feeds a heartbeat `now(iso, epoch)` row, retracted and re-inserted every
`tick` seconds (default 60), so time-based rules re-evaluate incrementally —
things *enter* and *leave* derived relations as deadlines cross the window.
`fixed=<ISO>` freezes the clock for tests and reproducible runs.

```bash
dep2 run rules.dl --source 'files=fs:root=.' --source 'clock:tick=60'
```

## Limitations

- `ast_span` (byte offsets) churns on most edits — offsets after the edit shift,
  so it is *not* minimal-diff. That churn is deliberately isolated to the side
  table; the structural `ast_node` graph stays stable. Avoid joining `ast_span`
  in hot analyses unless you need positions.
- Change *detection* still rescans the directory tree on each event (the `fs`
  plugin) / re-reads changed files (`treesitter`); the re-parse itself is
  incremental. Fine for typical projects.
- **Recursive aggregation may not terminate, depending on the operator.**
  Recursive `sum`/`count` is desugared by a planner-level *stratum split*
  (`crates/strata/src/rewrite.rs`) into an un-aggregated recursive helper plus a
  downstream non-recursive aggregation — sound under the incremental (`isize`)
  semiring, and correct under streaming insert/delete (see the
  `batch_cc_/streaming_cc_/batch_mutual_min_` property tests). Both
  *self*-recursion and *mutual* recursion between aggregated heads are handled
  (the whole aggregated cycle is lifted out of the recursive stratum). That
  helper accumulates candidate values, so a split aggregate must range over a
  *finite* value domain to converge.
  Recursive `min`/`max` is deliberately **not** split — it folds inside the loop,
  so candidates never accumulate and it converges whenever the lattice has no
  infinite descending (resp. ascending) chain. Positive-weight shortest paths
  terminate even through a cycle (going around only lengthens the path, and the
  fold discards it); *negative* cycles do not, since the value decreases without
  bound. `merge(op)` relations use exactly this in-loop path.

## Workspace layout

All crates live under `crates/` (a flat workspace, `members = ["crates/*"]`):

```
crates/dep2/                  the CLI binary
crates/dep2-core/             HCL-free engine: string interning + streaming wiring
crates/dep2-plugin/           plugin traits (Plugin, StreamingDataProvider, ...)
crates/dep2-plugin-fs/        filesystem seed + watch
crates/dep2-plugin-treesitter/ wasm-grammar parsing + flatten
crates/dep2-plugin-csv/       CSV streaming (kept as a reference data source)
crates/syntax/                  the .dl parser (chumsky + ariadne reports)
crates/{parsing,strata,catalog,optimizing,planning,reading,executing,macros}/
                                the FlowLog incremental Datalog engine
examples/                       example .dl analysis programs
web/                            React SPA: live graph + data + rules views
packages/force-graph/           reusable R3F force-graph component (+ Storybook)
.mise/tasks/                    project tasks (graph, storybook, build-grammar, ...)
```
