# A factored e-graph: congruence closure that retracts

E-graphs are insert-only. This note describes a way to get congruence closure
that supports **arbitrary, non-LIFO retraction** of any asserted equation, by
replacing the union-find with pieces that are each a monotone view over the
asserted facts, and letting a differential dataflow engine maintain them.

It works, and it is about fifteen rules: `examples/egraph/congruence.dl`. It
matches a textbook union-find oracle on every input tested, in batch and after
arbitrary deletion, and retracting an equation from a 400-term chain costs 1,196
diffs against 80,000 for the naive fold. On a Steensgaard points-to analysis, a
one-line source edit costs 200–400× less than recomputing.

**Where it pays is narrower than expected.** Two applications were tested
against a real codebase with ground truth, and both came back negative — one
because identity there is declared directionally (reachability suffices), the
other because the corpus has no nested structure for equality to propagate
through. Congruence closure earns its place when identity is *both* discovered
symmetrically *and* propagates through genuinely shared, nested structure.
Unification analysis is that; flat record types and forwarding chains are not,
and both are more typical of what dep2 reads.

---

## The problem

The core operation, `union(a, b)`, is destructive: it points one class at
another and, with path compression, rewrites the parent pointers it walks.
Compression is what makes find nearly constant-time, and it is precisely what
discards the record of *why* two things were merged. Congruence makes it worse —
merging `a` and `b` can force `f(a)` and `f(b)` to merge, which forces more
merges, and none of that cascade is recorded either. Deleting one asserted
equation therefore means rebuilding from scratch.

Every implementation reflects this. Souffle's `eqrel` states the assumption
outright: Datalog relations grow monotonically, "hence no deletion operations
are required." egg, egglog and eqlog have no retraction. Cranelift's ægraphs go
further and drop congruence closure altogether in exchange for acyclicity and a
single-pass rewrite; they deliberately keep no node→user index, which is exactly
the index any incremental scheme needs, so they cannot be made incremental at
all. SMT solvers do support undo, but only **LIFO**: Z3 and cvc5 keep a trail of
update records and a timestamped congruence table, restored by popping in
reverse. That is backtracking, not retraction of an arbitrary fact.

The theory community solved the *bare union-find* version: Galil and Italiano
(1991) give arbitrary, non-LIFO deunion in O(log n) worst case and O(n) space,
tight for separable pointer algorithms. As far as this survey found, that result
has never been lifted to congruence closure with a hash-cons.

## The construction

Factor the structure, the way `avg` factors into `sum` and `count`.

An average cannot be maintained incrementally because two averages do not
combine — knowing `avg(A)` and `avg(B)` does not give `avg(A ∪ B)`. Sum and
count each do combine, and the average is recovered at the end by dividing. The
fix is not a cleverer average; it is storing something lossless and recomputing
the lossy part on demand.

A union-find cannot be retracted for the same shape of reason: merging forgets.
So do not store the merge.

| | lossy form | factored form | recombine |
|---|---|---|---|
| average | `avg(S)` | `sum(S)`, `count(S)` | `sum / count` |
| e-graph | union-find + hash-cons, mutated in place | `eq_edge`, `leader`, `form_rep` | `class(t) = leader(t)`; congruence = join on canonical form |

- **`eq_edge`** — the equation graph: what was asserted, symmetrized, plus what
  congruence derives.
- **`leader : term → representative`** — the class representative, the *minimum
  label over the connected component* of the equation graph. This replaces the
  union-find, and it is a lattice fold (`merge(min)`), not a mutable structure.
- **`cnode`** — each e-node with its children replaced by their representatives.
  A view, recomputed as leaders change.
- **`form_rep : canonical form → representative term`** — the hash-cons, again a
  min-fold.

Congruence is one rule: a node is equal to the representative of its own
canonical form. This links each node to *one* representative rather than pairing
congruent nodes with each other, so it is **linear in nodes**, not quadratic in
class size — avoiding the materialized-equivalence blow-up that made hand-rolled
equality in Datalog slow. (egglog reports that in cclyzer++ the resulting "join
modulo equivalence" was an order of magnitude slower than any other rule in the
analysis.)

The fold also carries a **pointer-jumping** rule,
`leader(X,L) :- leader(X,M), leader(M,L)`, which is logically redundant — in a
monotone reading it derives nothing the propagation rule will not. It pays for
itself twice: it makes retraction linear instead of quadratic, and it decides
which fixpoint the recursion settles on. Both are below.

## Why it retracts

Nothing is mutated. The class structure is a pure function of the asserted
equations, so deleting one re-derives smaller classes, and any congruence that
equation had cascaded into loses its support and disappears with it.

Two things make that hold rather than merely sound plausible.

**The engine supplies the dependency index.** What an incremental e-graph would
need parent pointers for — "which derived facts rest on this equation" — is what
a dataflow engine already maintains. This is the index ægraphs deliberately
omit, and the one a proof forest only approximates: proof forests keep a
*single* justification per merge, since shortest-explanation is NP-hard, so
inverting one gives an incomplete dependency set.

**No derived fact outlives the equations it traces back to.** The obvious
objection is unfounded sets: congruence derives `eq(s,t)` from leaders, which
depend on equations, which include `eq(s,t)`. Naive counting-based IVM gets this
wrong, keeping facts alive on their own derivations. It holds here because
differential dataflow computes the fixpoint afresh for each input configuration —
differences are how it gets there, not what it means — and dep2's recursive
collector uses a reduce-based `threshold_rec`, so facts around a cycle are
re-derived rather than assumed.

### Which fixpoint

That is not the same as saying circular support can never hold anything up. This
recursion is **not monotone**: lowering a leader *retracts* the old `cnode` row
rather than adding to it. So "the least fixpoint" does not by itself pin down an
answer, and on a cyclic term table there is genuinely more than one stable state.

The case that exposes it: `4 = op(4, 5)` and `1 = op(4, 5)`. The two look
congruent, but the instant they merge, term 4's canonical form changes — its
child `4` now canonicalizes to `1` — so the form that justified the merge is
gone, replaced by one that justifies it again. The support is circular.

Propagating one hop at a time settles on refusing the merge. Adding the
pointer-jumping rule settles on the state a destructive union-find computes.
**The redundant rule selects the fixpoint.** Both variants are pinned by
`pointer_jumping_selects_the_union_find_fixpoint`.

### Validation

Two property tests against a textbook union-find congruence closure recomputed
from scratch as the oracle:

- `batch_egraph_matches_union_find` — over random well-formed term DAGs and
  random equation sets, the encoding computes exactly the same classes.
- `streaming_egraph_retraction_equals_rebuild` — insert the equations, delete an
  arbitrary subset, and the result equals what the oracle computes over the
  survivors alone. This is the claim a union-find cannot satisfy.

Both pass at 400 generated cases; with pointer jumping the agreement extends to
cyclic term tables, at 800 cases in batch and 250 under retraction. Worked
examples behave as expected: asserting `a = b` merges `f(a)` with `f(b)` and
retracting it splits them again; a cascade through `f`, `g`, `h` collapses and
re-splits at every level; an equality with two independent justifications
survives losing one.

## What it costs

**Kept:** congruence closure (which ægraphs give up); compact representation —
each term stored once, equality a derived link, never the O(n²) equivalence
relation; constant-time representative lookup once converged; e-class analyses,
which are just more `merge(op)` folds and compose freely; e-matching as a
relational query, free here since everything is already relational.

**Gained:** arbitrary, non-LIFO retraction with the congruence cascade correctly
undone; incremental maintenance under streaming input; provenance, since the
engine knows what rests on what.

**Paid:** no O(α) union. A class is not a contiguous structure, so iterating one
means an index lookup on `leader`, and the equation graph is retained rather
than collapsed into a forest — space is O(terms + equations).

### Propagation cost

Driven by the **diameter of the equation graph**, not by class size. Diffs
emitted for a class of N terms, building and then retracting one equation,
measured at N = 100…1600 by counting `leader` diffs:

| equation graph | one hop per round | with pointer jumping |
|---|---|---|
| chain of N — build | N² | ~2 N log N |
| chain of N — retract | N²/2 | **~3N** |
| star of N — build | 3N | 3N |
| star of N — retract | 2 | **2** |

Retracting one equation from a 400-term chain costs 80,000 diffs one hop at a
time and 1,196 with jumping — a 67× reduction that widens with N, being
quadratic against linear. On a 1600-term star, retraction is 2 diffs either way.

Linear retraction is close to the floor: splitting a chain genuinely changes N/2
leaders, so ~N diffs is the least any correct implementation could emit; the
remaining factor of ~3 is intermediate churn. That leaves union-find ahead only
on *building* a long chain, and behind on everything this structure exists for.

### Termination

Rewriting that creates terms needs a bound, and which quantity to bound was not
what I expected.

**Creation is idempotent, so breadth does not diverge.** Term ids are structural
strings (below), so building `shl(a,1)` twice yields the same id. Commutativity
therefore closes on its own: `add(a,b) = add(b,a)` creates the swapped term, and
re-applying re-creates `add(a,b)`, which already exists — 5 terms, fixpoint
reached, no bound needed. The blowup classical equality saturation fears most is
not a termination problem here. It still costs terms, but a bounded number.

**Depth does diverge, and wants a guard.** `X = add(X, 0)` deepens its input on
every firing and never closes; the engine's divergence detector eventually
reports an epoch that has not completed. The control is a `merge(max)` fold over
the children plus a guard on the term-creating rule:

```datalog
.decl depth(t: string, d: number) merge(max)
depth(T, 0) :- node(T, _, "_", "_").
depth(T, D + 1) :- node(T, _, A, _), A != "_", depth(A, D).
node(...) :- node(X, _, _, _), depth(X, D), D < 8.
```

At `depth < 2` the runaway tower stops at exactly `a`, `add(a,0)`,
`add(add(a,0),0)`. Pinned by `a_depth_guard_bounds_a_term_growing_rewrite`.

### Id size, which is the real limit

Datalog cannot mint values, so a rewrite could not construct the term it rewrites
*to*. The way out is cheap: make term ids **structural strings** built with
`concat`, so a term's id is its syntax and a rule constructs `shl(a,1)` by name.
No hashing, hence no collisions — two terms share an id exactly when they are the
same term.

The cost is that **a structural id cannot share**. An id spells out its whole
term, so a subterm used twice is written twice. A balanced duplicating rule
measured 6, 16, 36, 76, 156 characters at depths 1 to 5 — exactly
`L(k) = 2·L(k-1) + 4`, doubling per level. Sharing is precisely what an e-graph
exists to provide, and a string id gives it up: a heavily-shared DAG gets an id
exponential in its node count. The depth guard caps this as a side effect, which
is enough for shallow rewriting and is not a solution. Hashing to fixed width
would restore sharing at the cost of collision detection — two distinct
`(op, a, b)` triples landing on one id is a one-rule check, so it can be loud
rather than silently unsound.

## What it can do

### Unification analysis that survives editing

`examples/egraph/steensgaard.dl` is a Steensgaard points-to analysis — the case
egglog opens with, and the structure's strongest. It fits because `pt` ("what
this location points to") is an ordinary unary function symbol, so congruence
*is* the analysis: each statement asserts one equation, and unifying two
locations unifies their pointees transitively for free.

    x = &y   ->  pt(x) = y          x = *y   ->  pt(x) = pt(pt(y))
    x = y    ->  pt(x) = pt(y)      *x = y   ->  pt(pt(x)) = pt(y)

On `a = &x; b = &y; c = a; d = *c` it reports `a~c` and `d~x`. Adding `b = a`
unifies x with y and collapses every variable into one alias set — Steensgaard's
characteristic imprecision, arriving through congruence rather than any rule that
mentions it. **Retracting that one statement splits the classes back.** Pinned by
`steensgaard_points_to_survives_editing_the_program`.

What an edit costs, measured end to end through the CSV sources — build, then
delete one `x = y` statement and let it re-converge:

| variables | statements | full build | one-line edit | ratio |
|---|---|---|---|---|
| 400 | 776 | 66,611 diffs | 334 diffs | 199× |
| 800 | 1,553 | 1,308,361 diffs | 3,218 diffs | 407× |

**200–400× cheaper than recomputing, and the ratio improves with size**, which
is the whole point. The build column looks superlinear until you check what it
produces: these generated programs are degenerate for Steensgaard, collapsing all
1,600 locations into a single class at 800 variables, which implies 1,279,200
alias pairs. Build cost tracks output size almost exactly, not encoding overhead.
Enumerating alias pairs is quadratic in class size however the equivalence is
stored; a caller wanting a single alias query should read `points_to` and
compare. Pointer jumping is neutral on this shape (66,611 vs 68,925 without it).

This example pre-generates the `pt` tower to a fixed depth, so load nesting is
bounded — a property of the encoding, not the structure.

### Equality saturation with term-creating rewrites

`examples/egraph/saturation.dl` runs rules that create terms:

    R1   X * 2        =  X << 1     mints the shl term
    R2   (X << 1) / 2 =  X          fires only modulo equality

R2 is what makes this an e-graph rather than a rewriter. Nothing in the input
contains `(X << 1) / 2` — the division's child is `mul(a,2)`. R2 fires because it
looks for a `shl` node **in the dividend's class** rather than at the dividend
itself, which is e-matching modulo equality, and here it is a join through
`leader`.

On `div(mul(a,2),2)` plus `mul(b,2)` it proves `a = div(mul(a,2),2)` along with
both strength reductions, having invented `shl(a,1)` and `shl(b,1)`. Deleting the
division retracts the proof and leaves the reductions standing. Pinned by
`equality_saturation_invents_terms_and_still_retracts`.

One convention matters: **leaves are nullary symbols named after themselves**.
Give every literal the operator `lit` and congruence will prove `1 = 2` — correct
behaviour for the rule, and a terrible encoding.

## Where it does not pay

Two applications tested against nettbil, where there is ground truth. Both
negative, for opposite reasons.

### Structural type identity

Meant to be decisive, because unlike re-exports the identity is *discovered
symmetrically*: two object types are equal when their fields are pointwise equal,
which is the congruence rule verbatim. `examples/egraph/typedup.dl` builds each
declaration into a field-name-sorted cons spine, so unordered field sets get a
canonical term, then runs the e-graph over it.

The mechanism works. On a designed sample, `{v: Car}` and `{v: Auto}` are
different terms that become equal only after `Car` and `Auto` are found equal —
`f(a) = f(b)` from `a = b`, propagating upward.

On real code it finds nothing extra: 375 declarations, 68 duplicate pairs, and
**zero** found by congruence. Every duplicate was already the same structural
term, so hashconsing alone had it. The data says why — of 3,729 field
occurrences, roughly 2,553 are primitives, 1,000 composite (`Car[]`, `A | B`,
inline objects), and only **176 name another declared type**. There is almost
nothing to propagate through, and the duplicates that exist are byte-identical
API records pasted into three apps.

Two suppressors are the encoding's rather than the corpus's: composite
annotations are treated as opaque, and a type reference is tied to a declaration
only when its name is globally unique. Widening either enlarges the surface but
does not change the shape of the answer, since the duplicates found are identical
copies that hashconsing catches by construction.

The analysis earns its keep anyway: `AddressSuggestion` is a `type` in one app
and an `interface` in another with identical fields, which this unifies and a
text diff would not.

### Re-exports

`import_graph` draws a file-to-file edge for a re-export but cannot say that
importing `Button` from a barrel is really a dependency on
`components/radio/index.tsx`. That looked like a congruence problem — the same
entity under different names.

The analysis works: 272 specifiers resolved without ambiguity, 581 forwarded
symbols, **1,190 hidden dependencies**, about 16% more edges than the 7,445 the
file graph draws. Five results sampled at random were verified by hand. It now
lives on main as `examples/reexports.dl`.

But it uses none of this. Re-export chains are **directed** — F forwards from D,
never the reverse — so symbol provenance is plain reachability, computed by three
recursive rules. The condition that would have changed the answer is a re-export
cycle, which makes the relation genuinely symmetric; `reexport_cycle` checks for
exactly that and is empty.

### What the two add up to

Re-exports failed because identity there is declared directionally, so
reachability suffices. Type duplication failed for the opposite reason: identity
is discovered symmetrically and the machinery fires correctly, but the corpus has
no nesting to propagate through, and structural hashconsing already closes every
case.

Congruence closure pays when identity is **both** discovered symmetrically **and**
propagates through genuinely shared, nested structure. Unification analysis is
that — a pointer's target class feeds its dereference's class, layer after layer —
which is why `steensgaard.dl` remains the strongest case.

## Relation to prior art

Closest published work is Motik, Nenov, Piro and Horrocks, *Combining Rewriting
and Incremental Materialisation Maintenance for Datalog Programs with Equality*
(IJCAI 2015, in RDFox). They address the same problem — retracting an equality
invalidates a rewriting, and previously-collapsed facts must be restored — with
arbitrary, non-LIFO deletion sets. Their mechanism is different: backward
chaining to find surviving proofs, forward chaining to restore, with per-fact
bookkeeping. No complexity bound is given; the evaluation is empirical and
degrades toward full rematerialization as the deletion set grows. This
construction replaces proof search with dataflow view maintenance, which is why
it is fifteen rules rather than an algorithm.

Otherwise: egglog contributes `:merge` — the same mechanism serves congruence
when the merge is union and lattice analyses when it is a join — but is monotone.
Its `delete` action removes a function-table row, not an equality, and cannot
undo the congruences that row caused. Colored e-graphs layer *coarsenings* for
hypothetical reasoning: you can ask "what if additionally `a = b`", never
un-assert. "Incremental Equality Saturation" (EGRAPHS 2025) reuses one e-graph
across a *sequence* of inputs via version tags; not deletion. The semi-persistent
e-graph work at EGRAPHS 2026 is the nearest live effort and is push/pop, i.e.
LIFO.

No prior work found combines e-graphs with differential/streaming incremental
view maintenance, and no e-graph library supports retracting an equation.

## Bugs this surfaced

Building on an engine is a good way to test it. Three defects were found and
fixed on main, none of them in the e-graph work itself:

- **Served state dropped retractions.** Output diffs were applied as
  insert/remove rather than as counts, so a `-1` arriving for a row not currently
  present was discarded and the later `+1` resurrected it permanently. Recursive
  label propagation over deleted input left nodes carrying two labels, one stale,
  and it never converged. The dataflow had been right all along.
- **String `min`/`max` compared interned ids**, which are assigned in arrival
  order and differ between runs and across the parse pool's threads — a silently
  nondeterministic answer. They now compare decoded text, which is what lets a
  string column serve as a class representative. `sum`/`avg` over string columns
  are now rejected outright.
- **Fat mode used the wrong arity budget for key/value arrangements.** A kv's
  value was compared against the row budget, so a join output of arity (1,5) was
  wide enough to build and had no generated join arm; the next join over it
  panicked every worker.

## Open questions

1. **Sharing.** Structural ids cannot share subterms, so a duplicating rewrite
   grows them exponentially. Hashing to fixed width with collision detection is
   the next thing to try, and wants measuring against the depth guard it would
   replace.
2. **A complexity bound.** Galil–Italiano give O(log n) for arbitrary deunion on
   the bare union-find. Lifting that to congruence closure appears open; this
   construction sidesteps it by not being a union-find, but its own bound is
   unknown. The measured behaviour is O(diameter) rounds over O(edges).
3. **Does the leader fold need to be `min`?** Any lattice join works. A
   representative chosen by *stability* rather than value — one that changes less
   when a class splits — might cut the remaining ~3× over the floor.
4. **Is the union-find fixpoint the right one?** Pointer jumping lands on it, so
   the encoding agrees with classical e-graphs everywhere tested. But the other
   stable state — refusing a congruence that rests only on itself — is arguably
   better for an analysis reading equations off source code, where a conclusion
   supporting itself is not evidence. Picking one via an optimization rule is not
   a principled way to choose, and there is currently no way to ask for the other.
5. **Extraction.** Reading an optimized term back out is the one substantial
   piece of egglog's model not built here, and it is expressible as another
   `merge(min)` cost fold. It points toward optimization, though, where batch
   compile-time tools are strong and retraction buys nothing.
