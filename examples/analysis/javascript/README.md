# JS/TS scope and points-to analysis

`mise run graph-calls [directory]` runs `examples/calls.dl`. With no directory it
analyzes this repository (including `web` and `packages`). The same task now
also includes the [Rust adapter](../rust/README.md). `mise run graph` retains
its existing import-graph behavior.

The graph uses **definition identities**, `file#structural-AST-path`, as node IDs.
Labels are display names only. A module has its own node for top-level calls;
anonymous functions have their own nodes too. Click an edge for its call-site
location, or a node to open its source file. These IDs survive changes outside
their structural path, but are not persistent symbol IDs across arbitrary edits.

The reusable entry is `examples/analysis/javascript.dl`:

- `javascript/ast.dl` normalizes JS/TS AST nodes and grammar fields.
- `javascript/scopes.dl` assigns lexical scopes and declaration identities.
  Name lookup stops at the nearest scope that declares the name, even if that
  declaration has no known function value. `var` binds to the enclosing
  function/module; `let`/`const` bind to blocks. Parameters, catch bindings,
  named function expressions, and destructuring introduce bindings.
- `javascript/modules.dl` connects ESM bindings to exported values. It handles
  relative imports, aliases, default and namespace imports, named re-exports,
  and star re-exports. Paths are normalized component by component. Existing
  exact files take precedence, then `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs`,
  then directory `index` files. A missing `.js` file can resolve to `.ts`.
- `javascript/jsx.dl` models JSX function components and explicit/spread props.
  DOM tags and closing tags do not create invocation edges.
- `javascript/values.dl` solves inclusion constraints over function and object
  allocation sites. Assignments, arguments, returns, and property reads/writes
  propagate possible values. Call targets feed argument/return propagation,
  so higher-order and recursive calls participate in the same fixed point.

This is a **may-call analysis**, flow insensitive and context insensitive. All
assignments contribute possible values; assignments do not kill prior values.
A function called with two different callbacks can invoke either. Objects from
the same allocation expression share an abstract location across invocations.
Re-export stars conservatively union candidate values, with explicit exports
taking precedence; ambiguous star exports are not diagnosed. Object fields
remain separate by allocation site and property name. This avoids
the additional conflation of a unification-based points-to analysis.

Supported value forms include function declarations/expressions/arrows, object
methods, literal properties, aliases, destructuring declarations/parameters,
array holes, array iteration with `for-of`,
static property access, object spread, conditional/logical expressions, TS `as`
and non-null wrappers, function/class construction, static and instance methods,
and receiver `this` (lexically captured by arrows). The analysis does not run
TypeScript's type checker or use annotations to invent receiver types.

The source plugin supplies `ast_field(file, parent, field, node)` using the
actual tree-sitter field names, e.g. `function`, `arguments`, `object`, `property`,
`name`, and `value`. Like the other AST side tables, it is only constructed if
a rule consumes it, and its rows retract on file edits/deletions.

## Inspecting results

The Data view / query API exposes:

| Relation | Meaning |
| --- | --- |
| `function_node(id, name, file, line)` | Function definitions and module roots |
| `call_target(site, target)` | A call site's possible definition targets |
| `call_at(file, caller, callee, line)` | Graph edges with source locations |
| `binding(scope, name, declaration)` | Lexical declarations |
| `resolves(reference, declaration)` | Name resolution, before value analysis |
| `points_to(value, allocation)` | Possible function/object values |
| `unresolved_call(file, site, caller, line)` | No known target at this call site |
| `unresolved_import(file, spec, line)` | Module outside the relative resolver |

Unknown targets are not replaced with guessed name matches. However, an empty
`unresolved_call` relation does **not** prove completeness: a partially resolved
call may also have unknown targets. These results are not a soundness guarantee
for arbitrary JavaScript, and missing edges must not be used to delete code.

## Boundaries of this first implementation

Package exports, tsconfig aliases, CommonJS, dynamic imports, external library
callback models, JSX children/class components, decorators, dynamic/computed
property keys,
prototype mutation/inheritance, getters/setters, class field initializers,
`call`/`apply`/`bind`, rest/spread arguments, promises, generators, and exceptions
need additional models. Spread arguments are not positionally misassigned;
array spread is not treated as one ordinary array slot. Type-only imports do not
supply runtime values. Temporal dead zones, evaluation order, and branch
feasibility are outside the flow-insensitive model. Ordinary function default
parameters currently share the function's binding scope. Destructuring assignment
to existing variables, escaped property-name literals, and custom iterators
need additional models.

Build grammars with `mise run setup`, then run:

```sh
cargo test -p dep2 --test javascript_calls
cargo test -p dep2 --test examples
```

The source fixtures annotate expected targets, including explicit unresolved
sites. The integration test runs real JS/TS grammars and the streaming engine,
checks exact target sets and caller attribution (including the absence of false
edges), changes object
values and shadowing declarations, and verifies retraction and restoration.

On macOS these live-edit tests need access to filesystem watcher events; a
sandbox that blocks FSEvents cannot run the retraction checks.
