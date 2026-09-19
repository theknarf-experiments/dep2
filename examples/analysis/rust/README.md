# Rust call analysis

`mise run graph-calls [directory]` now analyzes Rust and JS/TS together. The
language adapters produce the same graph relations, using definition identities
(`file#AST-path`) rather than names. A Rust function named `run` never joins a
JS function or another Rust method simply because their labels match.

The Rust rules are reusable through `examples/analysis/rust.dl`:

- `ast.dl` normalizes Rust nodes, grammar fields, positions, and argument slots.
- `scopes.dl` separates value/type namespaces, hoisted items, parameters and
  sequential local declarations. A `let` binding becomes visible after its
  statement; its initializer sees the preceding binding. Closures capture the
  preceding lexical environment. Nested `fn` items cannot capture outer locals.
  Modules do not implicitly inherit outer-module names. Loop, `if let`, and
  match-arm patterns have their own visibility regions.
- `modules.dl` follows declared `mod` trees, including inline modules, `foo.rs`,
  `foo/mod.rs`, and children of non-root module files. `crate`, `self`, `super`,
  brace imports, aliases and named re-exports resolve within that tree.
- `cargo.dl` reads ordinary inline Cargo path dependencies, including renamed
  dependencies, and connects them to their `src/lib.rs` targets. Resolution uses
  the declared path and the nearest manifest, not a global package-name match.
- `values.dl` propagates possible functions, objects and reference locations
  through assignments, calls, arguments, returns, closures, references,
  dereferences, fields, tuple destructuring and array iteration. Struct fields
  retain their allocation identity. Type annotations, constructed values,
  aliases and `Self` resolve inherent methods to the implementing declaration.

The binding rules follow the [Rust Reference's scope rules](https://doc.rust-lang.org/reference/names/scopes.html).
Module layout follows the [module documentation](https://doc.rust-lang.org/reference/items/modules.html).
The implementation is deliberately narrower than the full
[method lookup algorithm](https://doc.rust-lang.org/reference/expressions/method-call-expr.html).

## Results and precision

`function_node`, `call_target`, `calls`, `call_at`, `binding`, `resolves`, and
`points_to` are shared with JS/TS. Additional relations expose Rust semantics:

| Relation | Meaning |
| --- | --- |
| `rust_binding(scope, namespace, name, declaration, visible_after)` | Namespace and byte offset where a binding becomes visible |
| `value_type(value, type_definition)` | Possible declared receiver types |
| `reference_target(reference, location, field)` | Referenced variable storage, or an allocation's field; empty field means variable storage |
| `analysis_gap(file, site, kind, line)` | Unexpanded macros, unresolved module declarations, and unsupported annotated receiver types |

`unresolved_call` exposes sites without a modeled callable target. Struct
construction without an explicit function body is a known allocation, not an
unresolved call. `unresolved_import` reports unresolved imported names; for
Rust its `spec` column contains the local import name/alias.

This is a may-call analysis, flow insensitive within a binding and context
insensitive across invocations. Reassigning one binding unions possible values;
separate `let` declarations retain separate identities. Calls passing distinct
callbacks to the same function can therefore contribute multiple targets.
References retain storage identities so writes through `&mut`/`*` can update
possible callback values. Borrow checking, move validity and path feasibility
are not performed. An empty unresolved-call relation does not establish complete
coverage; a partly resolved site can still have unmodeled targets.

## Current boundaries

This is not a replacement for rustc/rust-analyzer type inference. Trait dispatch,
trait defaults, operator overloads, custom `Deref`, closure-call trait machinery,
macro expansion, conditional compilation, and generic specialization need further
models. A `dyn Trait` or unresolved generic receiver does not get an inherent
method edge just because an observed argument has a concrete type. Such calls
remain unresolved and appear in `analysis_gap`.

Generic arguments are erased for supported concrete local types. Type aliases
preserve the underlying type. The resolver does not enforce visibility or
reproduce autoref/autoderef candidate ordering. Import aliases conservatively
reserve both namespaces when the target is unavailable. Glob imports, extern
crate declarations, `#[path]`, custom library targets, Cargo workspace dependency
inheritance, registry dependencies and target-specific dependency tables are
not resolved by this first adapter. Orphan source files are treated as separate
roots; callers should scan the crate/workspace root to include manifests and
module declarations.

Basic `if let` and match patterns are supported; let chains, guards that introduce
new bindings, slice/rest patterns and enum payload construction need further
models. Iterator-library calls and async/generator scheduling are not expanded.
AST paths can change after structural edits; they are not durable symbol IDs.

## Verification

Build the grammars using `mise run setup`, then run:

```sh
cargo test -p dep2 --test rust_calls --test javascript_calls --test examples
```

The tests run real grammars and the streaming engine. Fixtures assert exact
call-target sets and caller attribution. They cover sequential shadowing,
receiver separation, references and field writes, callbacks, closures,
destructuring, module paths, renamed Cargo dependencies, and trait-dispatch
boundaries. Edits to constructor arguments and import bindings must retract
and restore the relevant edges. The shared test harness also preserves the
JS/TS live-edit regression checks. macOS tests require access to FSEvents.
