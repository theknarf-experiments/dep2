# A factored e-graph: congruence closure that retracts

*Working note. Prototype: `examples/egraph/congruence.dl`. Property tests:
`batch_egraph_matches_union_find` and `streaming_egraph_retraction_equals_rebuild`
in `crates/executing/tests/properties.rs`.*

## The problem

E-graphs are insert-only, and not by oversight. The core operation, `union(a, b)`,
is destructive: it points one class at another and, with path compression,
rewrites the parent pointers it walks. Compression is what makes find nearly
constant-time, and it is also precisely what discards the record of *why* two
things were merged. Congruence makes this worse — merging `a` and `b` can force
`f(a)` and `f(b)` to merge, which can force more merges, and none of that
cascade is recorded either. So deleting one asserted equation means rebuilding
from scratch.

Every implementation reflects this. Souffle's `eqrel` states the assumption
outright — Datalog relations grow monotonically, "hence no deletion operations
are required." egg, egglog, and eqlog have no retraction. Cranelift's ægraphs go
further and drop congruence closure altogether in exchange for acyclicity and a
single-pass rewrite; they deliberately keep no node→user index, which is exactly
the index any incremental scheme would need, so they cannot be made incremental
at all. SMT solvers do support undo, but only **LIFO**: Z3 and cvc5 keep a trail
of update records and a timestamped congruence table, restored by popping in
reverse order. That is backtracking, not retraction of an arbitrary fact.

The theory community solved the *bare union-find* version of this. Galil and
Italiano (1991) give arbitrary, non-LIFO deunion in O(log n) worst case and O(n)
space, which is tight for separable pointer algorithms. But as far as this survey
found, that result has never been lifted to congruence closure with a hash-cons,
and no e-graph library implements anything like it.

## The idea

Factor the structure, the way `avg` factors into `sum` and `count`.

An average cannot be maintained incrementally because two averages do not
combine — knowing `avg(A)` and `avg(B)` does not give you `avg(A ∪ B)`. Sum and
count each *do* combine (both are monoid homomorphisms), and the average is
recovered at the end by dividing. The fix is not a cleverer average; it is
storing something that retains enough information, and recomputing the lossy
part on demand.

A union-find cannot be retracted for the same shape of reason: merging forgets.
So do not store the merge. Store the asserted equations, and derive the class
structure from them:

| | lossy form | factored form | recombine |
|---|---|---|---|
| average | `avg(S)` | `sum(S)`, `count(S)` | `sum / count` |
| e-graph | union-find + hash-cons, mutated in place | `eq_edge` (asserted + derived equations), `leader` (a min-fold over them), `form_rep` (hash-cons over canonicalized nodes) | `class(t) = leader(t)`; congruence = join on canonical form |

The pieces:

- **`eq_edge`** — the equation graph: what was asserted, symmetrized, plus what
  congruence derives.
- **`leader : term → representative`** — the class representative, computed as
  the *minimum label over the connected component* of the equation graph. This is
  the piece that replaces the union-find. It is a lattice fold (`merge(min)`),
  not a mutable structure.
- **`cnode`** — each e-node with its children replaced by their representatives.
  A view, recomputed as leaders change.
- **`form_rep : canonical form → representative term`** — the hash-cons, again a
  min-fold.

The `leader` fold also carries a **pointer-jumping** rule,
`leader(X,L) :- leader(X,M), leader(M,L)`, which is logically redundant — in a
monotone reading it derives nothing the propagation rule will not. It earns its
place twice over anyway; see *Measured cost* and *Which fixpoint* below.

Congruence is then one rule: a node is equal to the representative of its own
canonical form. Note this links each node to *one* representative rather than
pairing congruent nodes with each other, so it is **linear in nodes**, not
quadratic in class size — avoiding the materialized-equivalence-relation blow-up
that made hand-rolled equality in Datalog slow (the cclyzer++ problem egglog
describes, where the "join modulo equivalence" was an order of magnitude slower
than any other rule in the analysis).

The whole structure is about fifteen rules; see `examples/egraph/congruence.dl`.

## Why it retracts

Nothing is mutated. The class structure is a pure function of the asserted
equations, so deleting one simply re-derives smaller classes — and any
congruence that equation had cascaded into loses its support and disappears with
it. The split happens by itself.

Two things make this work rather than merely sound plausible:

**The engine supplies the dependency index.** The thing an incremental e-graph
would need parent pointers for — "which derived facts rest on this equation" —
is what a dataflow engine already maintains. This is the index ægraphs
deliberately omit, and the one a proof forest only approximates (proof forests
keep a *single* justification per merge, since shortest-explanation is NP-hard,
so inverting one gives an incomplete dependency set).

**Support that traces back to a deleted equation goes with it.** The obvious
objection is unfounded sets: congruence derives `eq(s,t)` from leaders, which
depend on equations, which include `eq(s,t)`. Naive counting-based IVM gets
exactly this case wrong, keeping facts alive on their own derivations. It holds
here because differential dataflow computes the fixpoint afresh for each input
configuration — differences are how it gets there, not what it means — and
dep2's recursive collector uses a reduce-based `threshold_rec` so facts around a
cycle are re-derived rather than assumed.

This is not the same as saying circular support can never hold anything up. On a
cyclic *term table* it can, and whether it does depends on the propagation
strategy — see *Which fixpoint*. What retraction needs is weaker and is what
holds: no derived fact outlives the asserted equations it traces back to.

## What it costs

Honest accounting, because the trade is real.

**Kept:** congruence closure (which ægraphs give up); compact representation —
each term stored once, equality as a derived link, never the O(n²) equivalence
relation; constant-time representative lookup once converged; e-class analyses,
which are just more `merge(op)` folds and compose freely; e-matching as a
relational query, which is egglog's insight and comes for free here since
everything is already relational.

**Gained:** arbitrary, non-LIFO retraction of any asserted equation, with the
congruence cascade correctly undone; incremental maintenance under streaming
input; provenance, since the engine knows what rests on what.

**Paid:** no O(α) union — a merge costs label re-propagation over the affected
component, which pointer jumping brings to O(N) for a retraction and
O(N log N) to build a chain, against union-find's near-linear. The equation
graph is retained rather than collapsed into a forest, so space is
O(terms + equations) instead of O(e-nodes) with classes physically merged. A
class is not a contiguous structure; iterating one means an index lookup on
`leader`.

**Value invention** looked out of scope at first — Datalog cannot mint values,
so rewriting could not create the term it rewrites *to*, which bounds the
structure to unification analysis rather than equality saturation. It turned out
to be reachable, and cheaply: make term ids **structural strings** built with
`concat`, so the id of a term is its syntax and a rewrite constructs `shl(a,1)`
by name. No hashing, hence no collisions — two terms share an id exactly when
they are the same term. See *Equality saturation* below.

## Validation

Two property tests, both against a textbook union-find congruence closure
recomputed from scratch as the oracle, passing at 400 generated cases:

- `batch_egraph_matches_union_find` — over random well-formed term DAGs and
  random equation sets, the factored encoding computes *exactly* the same
  classes.
- `streaming_egraph_retraction_equals_rebuild` — insert the equations, delete an
  arbitrary subset, and the result equals what the oracle computes over the
  survivors alone. This is the claim a union-find cannot satisfy.

Worked examples behave as expected: asserting `a = b` merges `f(a)` with `f(b)`,
and retracting it splits them again; a cascade through `f`, `g`, `h` collapses
and re-splits at every level; and an equality with two independent
justifications correctly survives losing one.

Finding this out required fixing a genuine engine bug first — the materialized
state applied output diffs as insert/remove rather than as counts, so a
retraction arriving before its matching addition was dropped and the later
addition resurrected the row permanently. Recursive label propagation over
deleted input ended up with nodes carrying two labels, one stale. Fixed in
`engine: keep net counts in served state`; the dataflow had been right all along.

### Which fixpoint

This recursion is **not monotone**: lowering a leader *retracts* the old `cnode`
row rather than adding to it. So "the least fixpoint" does not by itself pin
down an answer, and on a cyclic term table there is genuinely more than one
stable state.

The case that exposes it: `4 = op(4, 5)` and `1 = op(4, 5)`. The two look
congruent, but the instant they merge, term 4's canonical form changes — its
child `4` now canonicalizes to `1` — so the form that justified the merge is
gone, replaced by one that justifies it again. The support is circular.

Propagating one hop at a time settles on the state where the merge is refused.
Adding the pointer-jumping rule settles on the state where it holds, which is
exactly what a destructive union-find computes. **The redundant rule selects the
fixpoint.**

With it, the encoding matches the union-find oracle on every input tested,
cyclic tables included, in batch (800 cases) and after arbitrary retraction (250
cases). An earlier draft of this note listed the cyclic divergence as an
inherent price of retractability; that was wrong, and the correction came from
an optimization rather than a semantic argument. What remains true is narrower
and more interesting: the answer is strategy-dependent, and the strategy that is
faster is also the one that agrees with the classical structure. Both variants
are pinned by `pointer_jumping_selects_the_union_find_fixpoint`.

## Does it do useful work?

`examples/egraph/steensgaard.dl` is a Steensgaard points-to analysis built on
the structure — the case egglog opens with. cclyzer++ implemented Steensgaard in
Datalog, found that a hand-rolled equivalence relation forced a "join modulo
equivalence" an order of magnitude slower than every other rule, and shipped two
soundness bugs in the encoding meant to avoid it. egglog's answer was a built-in
union-find: fast, and insert-only.

Steensgaard is a good fit because `pt` ("what this location points to") is an
ordinary unary function symbol, so congruence *is* the analysis: each statement
asserts one equation, and unifying two locations unifies their pointees
transitively for free.

    x = &y   ->  pt(x) = y          x = *y   ->  pt(x) = pt(pt(y))
    x = y    ->  pt(x) = pt(y)      *x = y   ->  pt(pt(x)) = pt(y)

On `a = &x; b = &y; c = a; d = *c` it reports `a~c` and `d~x`, which is right.
Adding `b = a` unifies x with y and collapses every variable into one alias set —
Steensgaard's characteristic imprecision, arriving through congruence rather
than through any rule that mentions it. **Retracting that one statement splits
the classes back to `a~c`, `d~x`.** Removing `c = a` as well leaves no aliases.

That is a unification-based program analysis that answers correctly *while the
source is being edited*, which is the thing a union-find implementation cannot
offer. Pinned by `steensgaard_points_to_survives_editing_the_program`.

### What an edit costs on a real analysis

Synthetic programs, measured end to end through the CSV sources: build the
analysis, then delete one `x = y` statement and let it re-converge.

| variables | statements | full build | one-line edit | ratio |
|---|---|---|---|---|
| 400 | 776 | 66,611 diffs | 334 diffs | 199× |
| 800 | 1,553 | 1,308,361 diffs | 3,218 diffs | 407× |

An edit costs a few hundred to a few thousand diffs against a build of tens of
thousands to over a million — **200–400× cheaper than recomputing, and the
ratio improves with size**, which is the whole point of the structure.

The build column looks alarmingly superlinear until you look at what it is
producing. These generated programs are degenerate for Steensgaard: at 800
variables the analysis collapses all 1,600 locations into a *single* class,
which implies 1,279,200 alias pairs. The build cost is tracking the size of its
own output almost exactly, not overhead in the encoding. Enumerating `may_alias`
is quadratic in class size no matter how the equivalence is stored; a caller who
only wants "do `a` and `b` alias?" should read `points_to` and compare, which is
linear.

Pointer jumping is neutral here (66,611 vs 68,925 without it) — it is worth
keeping for the chain case and for the fixpoint it selects, and it costs nothing
on this shape.

This example pre-generates the `pt` tower to a fixed depth with an arithmetic id
scheme, so load nesting is bounded. That is a property of this encoding, not of
the structure: `saturation.dl` mints term ids on demand instead.

## Equality saturation

`examples/egraph/saturation.dl` runs actual rewrite rules that CREATE terms:

    R1   X * 2        =  X << 1     mints the shl term
    R2   (X << 1) / 2 =  X          fires only modulo equality

Ids are structural strings, so R1's head constructs one:
`node(concat(concat("shl(", X), ",1)"), "shl", X, "1")`. This needs `merge(min)`
over a *string* column, which is why the engine now orders string min/max by
decoded **text** rather than by interned id — id order varies between runs and
across the parse pool's threads, so it could not have served as a representative.
(That also fixes a latent bug: `min` over a string column previously returned a
nondeterministic answer. `sum`/`avg` over string columns are now rejected
outright, being arithmetic on interned ids.)

R2 is the part that makes it a real e-graph rather than a rewriter. Nothing in
the input contains `(X << 1) / 2` — the division's child is `mul(a,2)`. R2 fires
because it looks for a `shl` node **in the dividend's class** rather than at the
dividend itself, which is exactly e-matching modulo equality, and here it is
just a join through `leader`.

On `div(mul(a,2),2)` plus `mul(b,2)` it proves `a = div(mul(a,2),2)` along with
both strength reductions, having invented `shl(a,1)` and `shl(b,1)`. Deleting
the division from the input retracts the proof and leaves the two reductions
standing. Pinned by `equality_saturation_invents_terms_and_still_retracts`.

One convention matters: **leaves are nullary symbols named after themselves**.
Give every literal the operator `lit` and congruence will cheerfully prove
`1 = 2`, which is correct behaviour for the rule and a terrible encoding. The
first run of this example did exactly that.

What remains genuinely out of reach is *termination*: nothing here bounds a
ruleset that grows terms without limit (associativity, commutativity). Classical
saturation handles that with iteration limits and fuel; this has no equivalent,
so a non-terminating ruleset simply does not converge.

## Measured cost

Cost is driven by the **diameter of the equation graph**, not by class size.
Diffs emitted for a class of N terms, building and then retracting one equation:

| equation graph | one hop per round | with pointer jumping |
|---|---|---|
| chain of N — build | N² | ~2 N log N |
| chain of N — retract | N²/2 | **~3N** |
| star of N — build | 3N | 3N |
| star of N — retract | 2 | **2** |

Measured at N = 100…1600 (`examples/egraph/congruence.dl`, `--print`, counting
`leader` diffs). Concretely, retracting one equation from a 400-term chain costs
80,000 diffs one hop at a time and 1,196 with jumping — a 67× reduction that
widens with N, since it is quadratic against linear. On a 1600-term star,
retraction is 2 diffs either way.

Linear retraction is close to the floor: splitting a chain genuinely changes N/2
leaders, so ~N diffs is the least any correct implementation could emit. The
remaining factor of ~3 is intermediate churn.

That leaves union-find ahead only on *building* a long chain (near-linear versus
N log N), and behind on everything this structure exists for.

## Relation to prior art

Closest published work is Motik, Nenov, Piro and Horrocks, *Combining Rewriting
and Incremental Materialisation Maintenance for Datalog Programs with Equality*
(IJCAI 2015, implemented in RDFox). They address the same problem — retracting
an equality invalidates a rewriting, and previously-collapsed facts must be
restored — with arbitrary (non-LIFO) deletion sets. Their mechanism is different:
backward chaining to find surviving proofs, forward chaining to restore, with
per-fact bookkeeping. No complexity bound is given; the evaluation is empirical
and degrades toward full rematerialization as the deletion set grows. This note's
approach replaces proof search with dataflow view maintenance, which is why it is
about fifteen rules rather than an algorithm.

Otherwise: egglog contributes `:merge` (the same mechanism serves congruence when
the merge is union and lattice analyses when it is a join) but is monotone.
Colored e-graphs layer *coarsenings* for hypothetical reasoning — you can ask
"what if additionally `a = b`", never un-assert. "Incremental Equality
Saturation" (EGRAPHS 2025) reuses one e-graph across a *sequence* of inputs via
version tags; not deletion. The semi-persistent e-graph work presented at EGRAPHS
2026 is the nearest live effort and is push/pop, i.e. LIFO.

No prior work found combines e-graphs with differential/streaming incremental
view maintenance, and no e-graph library supports retracting an equation.

## Open questions

1. **~~Closing the chain gap.~~** Answered: pointer jumping takes chain
   retraction from O(N²) to O(N) and build from O(N²) to O(N log N), with no
   cost on shallow graphs. What remains is whether the last constant factor of
   ~3 over the output-change floor can be removed.
2. **~~Value invention.~~** Answered, and more cheaply than expected: structural
   string ids need no hashing and so cannot collide. What is still missing is a
   termination story — fuel, iteration limits, or a depth bound — for rulesets
   that grow terms without limit.
3. **A complexity bound.** Galil–Italiano give O(log n) for arbitrary deunion on
   the bare union-find. Lifting that to congruence closure appears to be open;
   this construction sidesteps it by not being a union-find at all, but its own
   bound is unknown.
4. **Does the leader fold need to be `min`?** Any lattice join works. A
   representative chosen by *stability* rather than by value — one that changes
   less when a class splits — might cut re-propagation sharply.
5. **Is the union-find fixpoint the right one to have chosen?** Pointer
   jumping lands on it, so the encoding now agrees with classical e-graphs
   everywhere tested. But the *other* stable state — refusing a congruence that
   rests only on itself — is arguably the better answer for an analysis reading
   equations off source code, where a conclusion supporting itself is not
   evidence. Both are reachable; picking one via an optimization rule is not a
   principled way to choose, and the encoding currently offers no way to ask for
   the other.
