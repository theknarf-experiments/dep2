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

**Cyclic support does not survive.** The obvious objection is unfounded sets:
congruence derives `eq(s,t)` from leaders, which depend on equations, which
include `eq(s,t)`. Naive counting-based IVM gets exactly this case wrong. It
holds here because differential dataflow computes the least fixpoint for each
input configuration — differences are how it gets there, not what it means — and
dep2's recursive collector uses a reduce-based `threshold_rec` specifically so
facts that lose their only well-founded support are retracted around cycles.

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

**Paid:** no O(α) union — a merge costs label re-propagation across the affected
component, and splitting a large class can invalidate labels across all of it.
The equation graph is retained rather than collapsed into a forest, so space is
O(terms + equations) instead of O(e-nodes) with classes physically merged. A
class is not a contiguous structure; iterating one means an index lookup on
`leader`.

**Out of scope:** Datalog has no value invention, so rewrites cannot mint new
terms. This is congruence closure over a **fixed term universe**, not equality
saturation. That boundary is sharper than it sounds: it covers
unification-based program analysis — Steensgaard points-to, module and alias
resolution, the very case egglog opens with — where every term already exists in
the source. It does not cover optimization or synthesis, where rewriting is
supposed to create terms. egglog gets that from `:default` = make-set; a dataflow
version would need deterministic id minting, e.g. content-hashing a term to its
id, which is the obvious next thing to try.

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

### Where it differs from a union-find

Exactly one place, and it is a consequence of the trade rather than an
implementation gap. With a **cyclic node table** — a term that is its own
descendant — the only thing justifying a congruence can be that congruence
itself. Take `4 = op(4, 5)` and `1 = op(4, 5)`: the two look congruent, but the
moment they merge, term 4's canonical form changes (its child `4` now
canonicalizes to `1`), so the form that justified the merge no longer exists.
The merge holds only itself up.

A union-find keeps it — `union` is destructive and never revisits why. This
encoding drops it, because differential dataflow retracts facts with no
well-founded support, and that property is precisely what makes retraction work
at all. You cannot have both.

The boundary is narrow: well-formed term DAGs (children built before parents)
cannot express this, and a cycle *created by an asserted equation* — `a = f(a)`,
which puts a node in the class of its own child — is fine, because the merge
rests on a base fact that keeps its support. Both cases are pinned by
`cyclic_terms_differ_from_union_find` and
`equation_induced_cycles_match_union_find`.

## Measured cost

Cost is driven by the **diameter of the equation graph**, not by class size.
Diffs emitted for a class of N terms, building and then retracting one equation:

| equation graph | build | retract one equation |
|---|---|---|
| chain of N (diameter N) | N² | N²/2 |
| star of N (diameter 2) | 3N | **2** |

Measured at N = 100, 400, 1600 (`examples/egraph/congruence.dl`, `--print`,
counting `leader` diffs). The chain at N=400 costs 160k diffs to build and 80k to
retract; the star at N=1600 costs 4,799 to build and **2** to retract — removing
one term from a 1600-member class is constant work.

That is label propagation's known weakness and its known strength: a label needs
one round per hop to cross the component, so a long chain re-propagates
quadratically while a shallow graph barely moves. Union-find is near-linear on
both, so the chain case is a genuine regression against a classical e-graph, and
the mitigation is to keep the equation graph shallow (transitive shortcuts, or
pointer doubling to converge in O(log N) rounds).

For the intended use — unification-based analysis over a source tree, where
equations come from resolution facts rather than long transitive chains — the
star column is the representative one.

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

1. **Closing the chain gap.** Retraction is constant on shallow equation graphs
   and quadratic on long chains (measured above). Pointer doubling would give
   O(log N) rounds at the cost of materializing more pairs; whether that beats
   the current fold on realistic inputs is untested.
2. **Value invention.** Content-hashing terms to ids would let rewrites construct
   terms and turn this into real equality saturation. The risk is collisions and
   an unbounded universe; worth prototyping.
3. **A complexity bound.** Galil–Italiano give O(log n) for arbitrary deunion on
   the bare union-find. Lifting that to congruence closure appears to be open;
   this construction sidesteps it by not being a union-find at all, but its own
   bound is unknown.
4. **Does the leader fold need to be `min`?** Any lattice join works. A
   representative chosen by *stability* rather than by value — one that changes
   less when a class splits — might cut re-propagation sharply.
5. **Is the well-founded answer the better one?** On cyclic node tables this
   encoding refuses self-supporting merges where a union-find accepts them. For
   an analysis reading equations off source code, refusing looks right — a
   conclusion resting only on itself is not evidence. For equality saturation
   over deliberately cyclic e-graphs it is probably wrong. The two use cases may
   simply want different fixpoints.
