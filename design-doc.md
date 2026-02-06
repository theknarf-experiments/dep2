# HCL as a Front-End DSL for Flowlog

## Overview

The goal of this design is to let users write **Flowlog** (a Datalog-based logic program) entirely in **HashiCorp Configuration Language (HCL)**, so they never have to directly write Datalog rules. In essence, we provide a familiar HCL configuration interface that internally translates into Flowlog rules. This approach leverages HCL's human-friendly syntax and declarative nature[\[1\]](https://github.com/hashicorp/hcl#:~:text=HCL%20attempts%20to%20strike%20a,use%20in%20more%20complex%20applications) to hide the complexity of Datalog. Users define **blocks** and **attributes** in HCL, and the system automatically compiles them into Datalog facts and rules for the Flowlog engine. This is analogous to other domain-specific languages that build on Datalog -- for example, Open Policy Agent's Rego language is *based on Datalog* under the hood[\[2\]](https://holos.run/blog/why-cue-for-configuration/#:~:text=Rego%20) -- thereby empowering users with logical inference capabilities without exposing low-level logic syntax.

**High-Level Intuition:** Each HCL block in the config will represent either a base fact or a derived rule in the logic program. HCL's inherent support for **references** (where an attribute's value can refer to another block's attribute) is used to express relationships between facts. By writing simple HCL stanzas, users implicitly define Datalog relations and rules. The motivation is to open up Datalog's powerful recursion and querying abilities to a wider audience (e.g. DevOps and domain experts) who are comfortable with HCL (as used in tools like Terraform) but unfamiliar with logic programming. HCL is designed for clarity and uses key-value pairs and nested blocks to structure data[\[1\]](https://github.com/hashicorp/hcl#:~:text=HCL%20attempts%20to%20strike%20a,use%20in%20more%20complex%20applications), making it a convenient front-end for specifying logical rules in a declarative style.

In summary, this design document formalizes how an HCL-based DSL can fully encode Flowlog programs. We describe the execution pipeline from HCL to Flowlog, define the semantics of each HCL construct in logical terms, explain how we detect extensional vs. intensional definitions (and identify recursion), and illustrate with examples. Finally, we discuss how this foundation can be extended with more advanced logic features.

## Execution Model

**Compilation Pipeline:** The system treats the HCL configuration as the *source code* and Flowlog's Datalog engine as the *runtime*. The process begins by parsing the HCL config using the standard HCL parser (with a schema we define for our DSL). This yields a structured representation of all **blocks**, their labels, and attributes. Next, we **analyze dependencies**: for each block, the compiler checks its attributes to see if they contain references to other blocks. Based on this, blocks are classified into *extensional database (EDB)* facts or *intensional database (IDB)* rules. An atom that is a base input (never appearing as a rule head) is an EDB fact, whereas a derived atom (appears in a rule head) is an IDB[\[3\]](https://arxiv.org/html/2511.00865v1#:~:text=of%20occurs%20in%20some%20The,of%20at%20least%20one%20rule). Blocks with no references become EDB facts (base facts), and blocks with references to others become IDB rules (derived via those dependencies). We build an internal **dependency graph** of these references: if block A's attributes reference block B, then A depends on B.

**Variables and Constants:** HCL `variable` blocks are treated as global constants or parameters. At compile time, each `variable` (with either a user-provided value or a default) is resolved to a concrete value. These values can be inlined as constants in the generated Datalog rules. In other words, `variable` blocks serve as **named constants** that can be used throughout the config for clarity. They do not produce new relations; they simply supply literal values. For example, a `variable "threshold" { default = 50 }` might be referenced in an HCL rule, and the compiler will substitute `50` wherever `var.threshold` appears.

**HCL to Datalog Translation:** After classifying blocks, the compiler generates Datalog/Flowlog code. Each HCL block corresponds either to a **fact declaration** or a **rule** in Datalog:

- For an EDB block (no references), we emit a fact in the form `Predicate(field1, field2, ...)` representing that block's predicate name and attribute values.
- For an IDB block (with references), we emit a Datalog rule. The **head** of the rule is the predicate corresponding to the block, and the **body** contains atoms for each referenced block. Essentially, the block's own attributes become either constants or variables unified with fields of referenced predicates (detailed in the next section).

After generating all facts and rules, the compiler inputs them to the Flowlog engine's front-end. (Flowlog's architecture cleanly separates the front-end parsing from the execution engine[\[4\]](https://souffle-lang.github.io/examples#:~:text=The%20,be%20accessed%20by%20other%20rules)[\[5\]](https://arxiv.org/html/2511.00865v1#:~:text=The%20straightforward%20way%20to%20implement,a%20fixpoint%20is%20reached); in our case, we bypass textual Datalog parsing by directly generating an internal representation.)

**Fixpoint Computation:** The Flowlog engine then executes the logic program. It starts with all the EDB facts (including those loaded from `data` sources, see below) and iteratively applies the IDB rules to derive new facts, repeating until no new facts appear -- i.e., until a **fixpoint** is reached[\[5\]](https://arxiv.org/html/2511.00865v1#:~:text=The%20straightforward%20way%20to%20implement,a%20fixpoint%20is%20reached). This is the standard semi-naïve evaluation of Datalog: rules are applied repeatedly, using newly derived facts each round, until closure. If the dependency graph has no cycles (no recursion), the evaluation will essentially follow a directed acyclic graph order (similar to Terraform's resource graph), deriving facts in a hierarchy. If there are recursive dependencies, the engine will iterate until fixpoint. In all cases, the Flowlog engine handles this transparently.

**Output Materialization:** Finally, after the fixpoint, the system produces outputs. HCL has a concept of **output** blocks (both at module-level and top-level) which we use to retrieve results. Top-level `output` blocks correspond to **terminal queries** in Datalog -- they do not feed into any further rules, but simply report results to the user. After the Flowlog engine finishes computation, it evaluates each top-level output query against the derived facts (which are now stable) and materializes the results. These could be printed to console, saved to a file, or passed to other systems as needed. The outputs thus allow end-users to see the outcome of the logic program in a friendly way (similar to how Terraform outputs expose information after an apply)[\[6\]](https://developer.hashicorp.com/terraform/language/values/outputs#:~:text=configurations.%20The%20,the%20following%20purposes%20in%20Terraform). In summary, the execution pipeline is:

**HCL Config** → Parse to AST → Analyze Blocks (EDB vs IDB) → **Compile to Flowlog Datalog** → Run fixpoint evaluation → **Collect Outputs**.

## Language Semantics

This section defines how each HCL construct in our DSL maps to Flowlog/Datalog semantics. We treat the HCL configuration language as a syntax for defining relations, rules, and data sources:

### `variable` Blocks → Constants

HCL `variable` blocks define named constants (often with a default value). In our DSL, a `variable` does **not** correspond to a predicate or rule; instead, it is a configuration-time constant that can be inlined into facts or rules. Each `variable "name"` introduces a symbol (or value) that can be used in attribute expressions elsewhere. At compile-time, references to `var.name` are replaced with the constant value. This is analogous to a Datalog program having a fixed constant or 0-ary function for a particular value. For example, if we have:

    variable "alert_threshold" {
      default = 50
    }

And elsewhere an HCL rule uses `var.alert_threshold`, the generated Datalog will directly use the number `50`. The purpose is to allow configurable parameters for the logic without hardcoding them. Users can thus adjust constants via variables (or override them when running the config, similar to Terraform), but they never see these as separate relations in Flowlog -- they remain global constants.

### Base Blocks (No References) → EDB Facts

Any top-level HCL block (excluding the special types like `variable`, `module`, etc.) that **does not contain references** in its attribute values is considered a base fact. We often use singular nouns as block types (e.g., `resource "type" "name"` style) to denote an object/fact. Such a block directly contributes an extensional fact to the logic program[\[3\]](https://arxiv.org/html/2511.00865v1#:~:text=of%20occurs%20in%20some%20The,of%20at%20least%20one%20rule).

**Predicate Name:** We use the block type as the predicate name in Datalog. For instance, a block `server "web1" { ip = "10.0.0.5", dc = "us-west" }` would define a predicate `server` in the logic.

**Attributes as Fields:** The block's attributes become the fields of the fact. Each attribute assignment `key = value` is treated as a predicate field taking that value. The ordering or naming of fields can follow a schema defined for that block type. In our example, `server("web1", "10.0.0.5", "us-west")` might be a fact (where we include the block label `"web1"` as an identifier along with the IP and datacenter). The inclusion of the label as a field is optional in the logical sense -- but to allow referencing specific instances, it can act as a primary key or identifier in the relation.

Crucially, this fact is part of the EDB and is never the head of any rule[\[3\]](https://arxiv.org/html/2511.00865v1#:~:text=of%20occurs%20in%20some%20The,of%20at%20least%20one%20rule). It's a given input to the program. End users just see it as defining a resource in HCL; under the hood it's a tuple in a table.

*Example:*

    resource "user" "alice" {
      role = "admin"
      active = true
    }

This might compile to a fact like: `user("alice", "admin", true).` which the Flowlog engine will treat as an extensional fact.

### Derived Blocks (With References) → IDB Rules

If an HCL block's attributes include references to other blocks, then it defines a derived relation. Such a block corresponds to a Datalog **rule**: the block's own predicate will be derived from the predicates it references. In Datalog terms, the block's type is the head predicate, and each referenced block becomes a body predicate. The compiler will generate a rule of the form:

    HeadPredicate(head_args) :- BodyPredicate1(body_args1), BodyPredicate2(body_args2), ... .

**Attribute Binding:** How do attributes map to rule arguments? For each attribute in the block: - If the attribute value is a literal (constant or a `var`), then in the rule it becomes a **filter** or constant equality in the body. Essentially, we ensure that field equals that constant. (In Datalog this could be an equality constraint or just directly inline if supported.) - If the attribute value is a reference to another block's attribute (like `other_type.other_name.field`), then we introduce that other block's predicate as a body atom. We unify the referenced field with the head's corresponding field. In practice the compiler creates a variable representing the value and uses it in both the head and the body atom.

In effect, the HCL references create **joins** between predicates. A block that references two different resources will result in a join between those two in the rule body. If it references the same resource type twice or two fields that must be equal, the compiler will generate appropriate equality constraints to ensure the values match.

**EDB vs IDB determination:** We determine that a block is IDB if *any* of its attributes contain a reference to another block (or to a value produced by another rule). These references are resolved as described above. The block itself then must appear as a head of a rule (since it's derived, not given initially)[\[3\]](https://arxiv.org/html/2511.00865v1#:~:text=of%20occurs%20in%20some%20The,of%20at%20least%20one%20rule). The engine will compute it during fixpoint evaluation.

*Example:* Suppose we want to derive a `monitor` resource that targets a server's IP. We write in HCL:

    resource "server" "web1" {
      ip = "10.0.0.5"
    }
    resource "monitor" "m1" {
      target_ip = server.web1.ip
    }

The `server.web1.ip` reference indicates `monitor.m1` is an IDB. The compiler generates a rule akin to:

    monitor("m1", TargetIP) :- server("web1", TargetIP, _).

Here, the `server` predicate in the body provides the `TargetIP` which is unified with the monitor's `target_ip` field. (The underscore `_` signifies other `server` fields we don't care about here, e.g., datacenter.) The semantics is: monitor *m1* exists for a given IP if a server *web1* with that IP exists. In the HCL, this was implicit by the reference; in the logic, it's an explicit rule.

If a block has multiple references, e.g.:

    resource "combo" "x" {
      val1 = resourceA.foo.value
      val2 = resourceB.bar.value
    }

then the rule's body will include both `resourceA(foo_id, Val1, ...)` and `resourceB(bar_id, Val2, ...)` atoms. The head will be `combo("x", Val1, Val2) :- ...` joining the two. If we needed `Val1` and `Val2` to satisfy some equality (rare in this pattern), the compiler would add a constraint or use the same variable name for both in the rule, effectively unifying them.

It's important to note that **references create a dependency graph**: the presence of a reference from combo.x to resourceA.foo and resourceB.bar means `combo.x` depends on those. This influences the execution order or the need for recursion (covered in the next section).

### `data` Blocks → External EDB Sources (Subscriptions)

HCL `data` blocks in our DSL declare **external data sources** whose contents populate EDB relations at runtime. This is similar to Terraform data sources (which fetch external info), but here it integrates with Flowlog's facts. A `data` block does not itself produce a rule; instead, it declares that an extensional relation's tuples will come from an outside system or stream. The schema of that relation is determined either by the data source type or configuration.

For example, one could have:

    data "logins" "events" {
      # (configuration to connect to an external log or stream of login events)
    }

This might represent a stream of login events with fields like username, timestamp, etc. The DSL will treat `logins.events` as an EDB predicate whose facts are continuously updated from the external source (e.g., each new login event inserts a fact). In Flowlog or Datalog engines, this corresponds to **subscription to external EDB updates** -- a concept supported by incremental or continuous Datalog systems[\[7\]](https://arxiv.org/html/2511.00865v1#:~:text=Datalog%20queries,2022%3B%20Shaikhha%20et%C2%A0al).

We assume the runtime has adapters to connect `data` sources (like databases, APIs, streams) and feed their data as facts into the logic engine. Each `data` block essentially declares a **named EDB** and possibly a filter or query to pull data. The important point is that from the logic perspective, `data.x.y` is just an EDB relation (like a table) that can be referenced by IDB rules, but is never derived by any rule (its population is external).

*Example:*

    data "logins" "events" { /* ... */ }

    resource "active_user" "from_logins" {
      name = data.logins.events.user
    }

If the external `logins.events` source provides a stream of login events with a `user` field, the `active_user.from_logins` block will create a rule:

    active_user("from_logins", Name) :- logins_events(Name, _).

This rule says any user appearing in the login events is considered an active_user. As new login facts come in, the Flowlog engine will derive new `active_user` facts. In an incremental setting, this can happen continuously. The `data` block thus bridges the gap between the HCL config and dynamic runtime data.

### Module `output` Blocks → Module-Scoped IDB Predicates

Within an HCL **module**, an `output` block defines a value or set of values that the module "exports" to the parent scope. In our DSL, we use module outputs to expose derived predicates from inside the module. A module's output is conceptually an IDB predicate that is **namespaced to that module** and available for the parent configuration to use.

Concretely, if a module file contains:

    output "result" {
      value = <expression referencing internal resources>
    }

This will compile to a predicate (let's call it) `result` in the module's logic, and when the module is instantiated, that becomes something like `module_instance.result` in the outer program. The module's internal rules should derive the value for `output.result`. In HCL, the output block typically has a `value = ...` expression. We allow that expression to reference module-local resources or data, so essentially the output block is a trivial rule that forwards or computes a value from inside the module. For example, in a module that computes some summary, `output "summary" { value = some_internal.pred.field }` will yield the final result from `some_internal` predicate.

For our DSL, think of a module's outputs as **named predicates** that are the *end product* of the module's internal Datalog. They are analogous to an IDB that the module computes and then marks for exposure. When the module is instantiated, the system will be able to refer to these via `module.<module_name>.<output_name>`. Technically, we implement this by **scoping** the output predicate to the module instance. Each module instance gets a unique prefix or identifier, and its outputs are prefixed by that when injecting into the parent's logic. This approach is exactly how Soufflé Datalog's component system handles module instantiation: upon instantiation, all internal relations are copied with a unique prefix so they won't conflict, and they can be referenced externally by prefixing with the instance name[\[4\]](https://souffle-lang.github.io/examples#:~:text=The%20,be%20accessed%20by%20other%20rules). In our case, only outputs (and maybe explicitly exported predicates) are made accessible, while purely internal predicates of the module remain hidden (to enforce encapsulation).

**Example:** Suppose we have a module that calculates the sum of two numbers (to illustrate in simple terms):

    // calc_module.hcl (module definition)
    variable "x" {}
    variable "y" {}

    resource "calc" "add" {
      result = var.x + var.y
    }

    output "sum" {
      value = calc.add.result
    }

Inside this module, `calc.add` is a resource that computes a result (an EDB here since it just uses variables), and `output "sum"` exposes that result. The module's `sum` output corresponds to an IDB predicate carrying the computed sum. If we instantiate this module:

    module "calc1" {
      source = "./calc_module.hcl"
      x = 5
      y = 7
    }
    output "final_sum" {
      value = module.calc1.sum
    }

The compiler will create a predicate for `calc1.sum`. Essentially, it inlines the module's logic with `x=5, y=7` and produces a fact for `calc1.calc.add` and hence `calc1.sum`. The top-level output `final_sum` then simply reads `module.calc1.sum` (which is the sum 12 in this case). In logic form, we'd have something like: `calc1_sum(12).` as a derived fact, and the `output final_sum` would retrieve that. This example shows how **module outputs become intermediate predicates** that parents can use or output. In Terraform terms, *child modules expose data via outputs to parent modules*[\[6\]](https://developer.hashicorp.com/terraform/language/values/outputs#:~:text=configurations.%20The%20,the%20following%20purposes%20in%20Terraform); here, that data can be not just single values but potentially entire relations computed by the module.

### Top-Level `output` Blocks → Readout-Only Queries

A top-level (root module) `output` block represents a **terminal query** of the logic program. These outputs do not feed into any other rule (nothing can reference a root output), so they are effectively projections or selections on the derived data. We treat each root-level output as something to evaluate and present once the Datalog fixpoint is reached.

Semantically, a root `output "X"` can be seen as asking: return the value of some expression or all tuples of some predicate after evaluation. In practice, the `output` block's `value` expression will often reference a resource or module output. The compiler will record that we need to retrieve that value. If the output is a simple reference to a predicate's field, it might compile to a query like "get all values of `<predicate>.<field>`" or a specific selection if the HCL expression is more specific.

For example, in the above module case, `output "final_sum" { value = module.calc1.sum }` simply means after computing, take the (scalar) value in `calc1.sum` and print it. If an output was something like:

    output "all_servers" {
      value = resource.servers[*].name  // (hypothetical syntax to collect all names)
    }

It would correspond to collecting all facts of a predicate (all server names). The DSL could allow certain aggregate or collection expressions in outputs to gather results.

The key is that top-level outputs **do not introduce new rules**; they operate on the already derived results. They are for the user's benefit to see results or for passing data out. In Datalog terms, they're analogous to issuing a query against the result of the program. Implementation-wise, after the Flowlog engine finishes, we execute these queries to fetch the needed values.

Thus, **root outputs are read-only views** into the logic outcomes. This is consistent with Terraform's model where root outputs just expose values (and in Terraform's case, print them or make them accessible to other Terraform configurations)[\[6\]](https://developer.hashicorp.com/terraform/language/values/outputs#:~:text=configurations.%20The%20,the%20following%20purposes%20in%20Terraform).

### `module` Blocks → Templating and Macro Expansion

HCL `module` blocks are used to **instantiate modules**, which serve as reusable logic templates. In our DSL design, modules provide a way to package a set of HCL definitions (including variables, resources, data sources, and outputs) and reuse them with different parameters. This is akin to macro expansion or templating in programming languages. Each `module` block in the configuration triggers the inclusion of a module's logic, with its own copy of rules and facts, scoped by the module's name (to avoid naming collisions).

When you write:

    module "instance1" {
      source = "./some_module.hcl"
      param1 = "foo"
    }

the compiler will load the module file `some_module.hcl`, substitute the inputs (`param1 = "foo"`, which likely correspond to a `variable "param1"` in the module), and then logically **inline** that module's contents into the overall program. All predicates defined in the module are namespaced (for example, if the module defines a predicate `P`, it might be renamed to `instance1.P` internally)[\[4\]](https://souffle-lang.github.io/examples#:~:text=The%20,be%20accessed%20by%20other%20rules). This ensures multiple instances of the same module can coexist without conflict, each operating on its own data. It mirrors how Soufflé Datalog's components are instantiated via `.init Name = Component` -- the component's rules are copied with the instance name as prefix[\[4\]](https://souffle-lang.github.io/examples#:~:text=The%20,be%20accessed%20by%20other%20rules). Our system does similarly: every module block leads to a copy of the module's AST, and we prepend the module's instance identifier to its predicates (or maintain a mapping from an internal module predicate to an instance-qualified predicate).

**Template Behavior:** Modules can be thought of as parameterized templates for Datalog code: - **Parameters**: Module input variables allow customization. These behave like constants within the module (as described under `variable` blocks). The `module ... { param = ... }` assignments set those constants for that instance. - **Internal Logic**: The module can contain any number of resource blocks and even nested module instantiations (modules within modules), building a local sub-graph of logic. - **Outputs**: Only the module's outputs are intended to be used by the outside world. Internal resources are encapsulated, unless an output or explicit design exposes them. This encapsulation is important for modular reasoning -- other parts of the config should rely only on the module's declared outputs, not its internal implementation details (just like a function's return values in programming).

By using modules, the DSL allows repetition and abstraction. Common patterns can be written once as a module and instantiated as needed. This avoids repeating similar sets of rules. It also helps manage complexity by giving a logical grouping of rules under a module namespace.

In summary, a `module` block causes a **macro expansion** of the module's contents. The result is as if the user had written those resource and output blocks in place, with every predicate name prefixed by `module.instance_name.` (or another scoping mechanism). The Flowlog engine doesn't natively know about "modules" -- it just sees the expanded flat program -- but from the user's perspective, modules provide a clean, high-level way to structure the config. The correctness of this expansion is ensured by how we handle name spacing and possibly by static checks to prevent unintended recursion across module boundaries, etc. (Though recursion *can* occur through modules if two modules' outputs reference each other via the parent, one must be careful -- see recursion section below.)

## Reference Detection and Recursion

One of the powerful aspects of Datalog is its ability to handle **recursive definitions**, and our HCL DSL supports this via reference cycles. The compiler's dependency analysis classifies each block as EDB or IDB by looking at references, but it also must detect when those references form cycles.

**Reference Graph:** We construct a graph where each node is a predicate (or a specific HCL block), and we draw a directed edge from A to B if A's definition (its block) references B. For example, if block X has an attribute = `Y.field`, we draw X → Y. This graph of dependencies can span across modules (we consider module instance predicates separately in this graph, effectively prefixed as above). Using this graph, we determine recursion: if there is a **cycle** (a closed loop of references), then the corresponding Datalog rules are recursive[\[8\]](https://arxiv.org/html/2511.00865v1#:~:text=Dependency%20Graph%20and%20Stratification,and%20in%20the%20topological%20order).

- If no cycles exist, the program is *stratified* into a partial order. We can evaluate non-recursive rules in a topologically sorted sequence (like a DAG). This is similar to Terraform's planning graph where resources are applied in order of dependencies -- except here it's logic derivation instead of imperative creation.
- If cycles exist, we have one or more **strongly connected components** in the graph that require iterative evaluation. Each strongly connected component of predicates corresponds to a set of mutually recursive rules (a *stratum* in Datalog terms)[\[8\]](https://arxiv.org/html/2511.00865v1#:~:text=Dependency%20Graph%20and%20Stratification,and%20in%20the%20topological%20order). These need to be evaluated together until they reach fixpoint.

**Handling Cycles:** Unlike some configuration languages which forbid cyclic dependencies (Terraform will error if you create a cycle between resources), our DSL **allows cycles intentionally**, because they represent legitimate recursive relations to be solved by the logic engine. When the compiler finds a cycle, it doesn't raise an error; instead, it flags those predicates as recursive IDBs. The Flowlog engine will naturally handle them by iterative fixpoint evaluation (semi-naïve evaluation ensures termination if there are a finite number of possible facts, which in pure Datalog without function symbols is the case)[\[5\]](https://arxiv.org/html/2511.00865v1#:~:text=The%20straightforward%20way%20to%20implement,a%20fixpoint%20is%20reached). The compiler may also assign *strata* to the rules (each cycle forms a stratum) to guide evaluation order if needed (ensuring any stratified negation is respected, though in this basic model we assume no negation yet).

**Example -- Mutual Recursion:** Consider two HCL blocks that reference each other:

    resource "A" "r" {
      link = B.r.link
    }
    resource "B" "r" {
      link = A.r.link
    }

Here, `A.r` references `B.r.link` and `B.r` references `A.r.link`. This is a direct cycle. The dependency graph has A → B and B → A, forming a cycle. The compiler will generate two rules:

    A("r", X) :- B("r", X).
    B("r", X) :- A("r", X).

These rules are recursive. During execution, starting with no facts for A or B, neither can derive a fact because they depend on each other -- so in this particular program, the result is an empty set (the least fixpoint is nothing, aside from the trivial null solution). But it *illustrates the recursion*: the engine will iteratively try to apply these rules. After the first round, no facts; it reaches a fixpoint (no new facts) and stops. The key is that the system detected the cycle and treated A and B as a strongly connected component evaluated together. A more meaningful recursive example would include a base case, e.g.:

    resource "A" "base" { val = "foo" }
    resource "B" "rule" { val = A.base.val }
    resource "A" "rule" { val = B.rule.val }

Here A.base is an EDB (with val = \"foo\"). B.rule references A.base (so B derives \"foo\"), and A.rule references B.rule (so A.rule derives \"foo\" as well). The dependency graph: A.base → (no deps); B.rule → A.base; A.rule → B.rule → A.base. This graph is actually acyclic (if we consider each specific node), because A.rule depends on B.rule, which depends on A.base; A.base had no dependency. There's no back-edge from A.rule to B or to itself. So it's not truly recursive -- it's a simple two-step derivation. Indeed, after one iteration: we get B.rule.val = \"foo\", then A.rule.val = \"foo\", and then stop. No cycle means it wasn't a recursive stratification, just a cascade.

For true recursion, a self-reference or mutual reference without an external base must occur. Another common pattern is **indirect recursion**, e.g. P depends on Q, Q on R, and R on P -- any loop counts. The compiler will use algorithms for detecting strongly connected components (Tarjan's or Kosaraju's algorithm on the dependency graph) to find such loops.

**Emergence of Recursion:** In general, **recursion emerges naturally** whenever there's a cycle of references. This could happen within a module or across modules: - Within a single predicate: e.g., a block of type X references another block of type X (perhaps with a different label), forming a cycle through the same predicate. Datalog can handle this (it becomes a recursive rule where the predicate appears in its own body via another instance or indirectly). - Mutual recursion across predicates: e.g., block of type X references Y, and block of type Y references X. Then X and Y predicates are mutually recursive. They will be in the same stratum. - Cross-module recursion: possible if module outputs and references are wired such that Module1.output references Module2.output and vice versa. The system will consider the fully expanded predicates; if those form a cycle, it's recursion. (Users need to be cautious to not create infinite recursion through modules, though the engine will simply find no fixpoint until an iteration yields no change -- which could be infinite if one keeps generating new facts, but with finite domains it will terminate).

We ensure the engine can handle recursion by relying on Flowlog's fixpoint engine. Flowlog (and Datalog engines in general) are designed for recursion, e.g., computing transitive closures, graph traversals, etc.[\[9\]](https://arxiv.org/html/2511.00865v1#:~:text=r1.%20reach%28x%29%20%3A)[\[10\]](https://arxiv.org/html/2511.00865v1#:~:text=The%20straightforward%20way%20to%20implement,a%20fixpoint%20is%20reached). The classic example is reachability: one rule defines a base reach, another defines reach recursively via itself. If a user were to encode that in HCL, they might unwittingly create a cycle that the system resolves by fixpoint. (We'll show such an example below.)

**Comparison to Terraform:** It's worth noting the philosophical shift: Terraform's HCL usage forbids dependency cycles as an error, because it cannot plan resource creation in cycles. In our logic DSL, cycles are *not* errors; they denote recursion which is a valid and useful construct (provided there is some base fact to ground the recursion, or else the result is just empty). This is a conscious design difference, leveraging Datalog's semantics for recursion. Users, of course, should supply at least one non-recursive input in a cycle to get a non-trivial result -- otherwise the rules define an empty or infinite self-loop. The system might warn if a cycle has no extensional base to ensure the recursion can produce something.

## Practical Examples

Let's walk through a few concrete HCL snippets and their equivalent Flowlog (Datalog) interpretations. These examples highlight base facts, simple derivations, recursion, data integration, and module usage.

### Example 1: Base Fact and Simple Rule

Consider a scenario with servers and a monitoring service. We want to monitor a server by IP. In HCL DSL:

    resource "server" "web1" {
      ip = "10.0.0.5"
      dc = "us-west"
    }
    resource "monitor" "m1" {
      target_ip = server.web1.ip
    }

- `server.web1` is a base resource with no references. It becomes an EDB fact: **server(\"web1\", \"10.0.0.5\", \"us-west\")** -- a tuple in the `server` relation.
- `monitor.m1` references `server.web1.ip`, so it becomes an IDB rule. The compiler will produce something like:

<!-- -->

    monitor("m1", TargetIP) :- server("web1", TargetIP, _).

In Flowlog/Datalog syntax, assuming the schema `server(name, ip, dc)` and `monitor(name, target_ip)`, the rule is exactly as above[\[9\]](https://arxiv.org/html/2511.00865v1#:~:text=r1.%20reach%28x%29%20%3A). This rule says *"monitor m1 exists for IP X if server web1 has IP X"*. Given the fact for server, the engine derives `monitor("m1", "10.0.0.5")` as a new fact. Essentially, the HCL reference `server.web1.ip` acted like a join condition linking the monitor to the server's IP.

**Explanation:** After one round of evaluation, the `monitor` fact is derived. There's no recursion here (acyclic dependency: monitor depends on server). The top-level config could output the monitor's target_ip if needed, or we simply know it's established. To the user, it was just assigning `server.web1.ip` to the monitor's attribute; to the logic engine, it was a rule performing a lookup.

### Example 2: Mutual Recursion (Cycle)

Now consider an artificial example to demonstrate recursion:

    resource "A" "r" {
      link = B.r.link
    }
    resource "B" "r" {
      link = A.r.link
    }

Here we have two resources A and B that reference each other's `link`. There are no base values given -- it's a self-contained cyclic definition. The translated Datalog would be:

    A("r", X) :- B("r", X).
    B("r", X) :- A("r", X).

This is a pair of mutually recursive rules. Logically, they say *"A.r's link X is true if B.r's link X is true"* and *vice versa*. This program doesn't have any starting fact, so the only solution is that no concrete X can satisfy this (except the trivial infinite loop which yields no extensional facts). The Flowlog engine will handle it by starting with an empty set for A and B and will not derive any new facts (fixpoint at empty). Although this particular cycle produces nothing, it **illustrates the detection of recursion**: the system identified a cycle A↔B, and would treat them in one stratum. If we had a base to kick it off, we could get a result. For instance:

    resource "A" "base" { link = "foo" }
    resource "B" "r"    { link = A.base.link }
    resource "A" "r"    { link = B.r.link }

Now the rules become:

    A("r", X) :- B("r", X).
    B("r", X) :- A("base", X).

We have a base fact `A("base", "foo")`. From it, the second rule derives `B("r", "foo")`. Then the first rule (with the cycle) derives `A("r", "foo")` from B. After that, no further new facts (we're at fixpoint). So we end up with A.base = foo (given), B.r = foo, A.r = foo. A and B (the recursive part) formed a cycle but because one side had a base input, the cycle produced one result. This is analogous to a simple reachability or closure logic: you need a base case to get the recursion going.

**Takeaway:** Cyclic references in HCL translate to recursive Datalog. The engine's fixpoint semantics will find a solution if one exists. Users can create recursive patterns (like linked lists, graph traversals, etc.) by using references that eventually loop back. The design ensures these are handled correctly rather than being errors.

### Example 3: Using `data` (External Input)

Imagine we want to flag any user who has logged in as "active". We have an external log of login events. In our HCL DSL:

    data "logins" "events" {
      # (configuration to connect to login events stream)
    }
    resource "active_user" "login" {
      name = data.logins.events.user
    }

Here, `data.logins.events` might provide a stream of events each with a `user` field. The `active_user.login` resource references that field, meaning it derives an `active_user` for each event's user.

Datalog-wise, suppose `logins_events(User)` is the predicate fed by the data source (each fact is a user who logged in, perhaps ignoring other details). The rule generated would be:

    active_user("login", Name) :- logins_events(Name).

If new login events come in over time, each with some username, the Flowlog engine will continuously assert new `logins_events(...)` facts (this is handled by the data subscription). The rule will then derive a new `active_user` for each such fact. If the engine is running incrementally, this could happen in real-time (each insertion triggers a new derivation). If running in batch, at the moment of fixpoint it will have all users that appeared in the input marked active.

For example, say the external source yields two facts: `logins_events("alice")` and `logins_events("bob")`. The rule gives us `active_user("login","alice")` and `active_user("login","bob")`. If we had an output to list active users, it would now include Alice and Bob.

This example demonstrates integrating external EDB with internal rules. The user didn't have to write any code to poll or handle updates -- they just declared a data source and a rule depending on it. The system (via Flowlog's incremental engine) ensures these are kept up-to-date[\[7\]](https://arxiv.org/html/2511.00865v1#:~:text=Datalog%20queries,2022%3B%20Shaikhha%20et%C2%A0al). In practice, one could extend this with conditions (say only mark active if login within last 1 hour -- which could be expressed with a condition or a timestamp comparison in HCL if supported, or a stratified rule).

### Example 4: Module with Outputs

For a more structured example, consider using a module to encapsulate logic. Suppose we have a module that determines VIP customers (those who have made purchases over a threshold):

**Module definition (**`vip_check.hcl`**):**

    variable "threshold" {}
    # This module expects an external data source of purchases:
    data "purchases" "records" {
      # ... e.g., all purchases with fields: user, amount
    }

    resource "vip" "rule" {
      user = data.purchases.records.user
      # Only consider purchase amounts over threshold:
      amount = data.purchases.records.amount
    }
    output "vip_list" {
      value = vip.rule.user
    }

In this module, we declare a `threshold` variable and a data source of purchase records (implying an EDB `purchases_records(user, amount, ...)`). Then `vip.rule` references the data fields. In reality we'd want to filter by threshold -- HCL might allow an expression like `data.purchases.records.amount > var.threshold` as a condition. If our DSL supports a basic conditional, we could do:

    resource "vip" "rule" {
      user   = data.purchases.records.user
      big_spender = data.purchases.records.amount > var.threshold
    }

and then perhaps only output if `big_spender` is true. But for simplicity, assume the filtering is implicit or just conceptual. The module's `output "vip_list"` exposes the `vip.rule.user` value -- effectively all users that meet the criteria.

**Module instantiation in main config:**

    module "vip_check_eu" {
      source    = "./vip_check.hcl"
      threshold = 1000
      # perhaps the data source is configured inside module or passed in, omitted for brevity
    }
    output "europe_vips" {
      value = module.vip_check_eu.vip_list
    }

We instantiate the module for the EU region with a threshold of 1000 (currency units). The module's internal logic will derive `vip.rule` facts for any purchase over 1000 in the data, and output their users. The top-level output then simply references the module's `vip_list` output.

**Under the hood:** The module's rules get expanded with prefix `vip_check_eu`. For instance, the `vip.rule` predicate might become `vip_check_eu.vip` in the global program, and `purchases.records` might also be namespaced if needed (or the data source could be declared outside module and passed in). The derived facts might look like `vip_check_eu_vip("alice", 1200).` (Alice spent 1200, over threshold), etc. The module's output `vip_list` being just the user field could correspond to a projection rule or simply be an alias for the `vip` predicate's user field. We could implement it as a separate predicate `vip_check_eu_vip_list(user)` or just reuse `vip_check_eu_vip(user,amount)` and let the output mechanism project the user. Either way, `module.vip_check_eu.vip_list` in HCL will fetch all the users identified as VIP in that module instance.

When the program runs, it will populate `purchases_records` from data, derive `vip_check_eu_vip` facts, then make those available. The `europe_vips` top-level output will collect those user names (say it prints them). The end user sees a nicely formatted output of VIP users in Europe, having never written a line of Datalog -- only configuration.

This module can be instantiated for other regions or datasets by passing different data sources or thresholds, demonstrating reuse. It also shows module outputs being used as a bridge between module logic and root logic (the root output depends on the module output)[\[6\]](https://developer.hashicorp.com/terraform/language/values/outputs#:~:text=configurations.%20The%20,the%20following%20purposes%20in%20Terraform).

## Extensibility

The HCL-for-Flowlog model described is a foundation that can be extended to cover more advanced Datalog features and integrate with various systems:

- **Aggregates:** Datalog supports aggregation (SUM, COUNT, etc.) in a stratified manner. We could introduce syntax in HCL for aggregates, for example a special type of block or attribute that performs an aggregation over a relation. Perhaps a block could declare an aggregate query like `count = count(resource.type[*])` or we add support for functions like `count()` in output expressions. This would let users compute summaries. Under the hood, these would map to Datalog aggregations (which require a separate rule in many engines). We'd ensure they are stratified (no recursion through an aggregate) to have well-defined semantics[\[11\]](https://arxiv.org/html/2511.00865v1#:~:text=Common%20Datalog%20Extensions,in%20a%20graph%20that%20are).

- **Stratified Negation:** In policy or networking logic, you often want to say "if not X, then Y". Datalog allows *negation* as long as it's stratified (no negative cycles). We can extend the DSL with a way to express negation -- possibly a function `absent(reference)` or a keyword. For instance, one might write something like:

<!-- -->

- resource "isolated" "rule" {
        host = data.network.host.id
        condition = depends_on.no_connections  # pseudo-syntax indicating no connection exists
      }

  This is speculative, but we'd design a clear way to mark that a rule should fire only if some other relation does *not* have a matching fact. The compiler would then generate a negated atom in Datalog (e.g., `:- not connection(host, _)`). We must ensure no recursion occurs through a negation (which the compiler can check via the dependency graph stratification). Stratified negation is a well-understood extension of Datalog[\[11\]](https://arxiv.org/html/2511.00865v1#:~:text=Common%20Datalog%20Extensions,in%20a%20graph%20that%20are), and our system can incorporate it by dividing the program into strata where negations from lower strata can be used in higher ones.

<!-- -->

- **Advanced Data Adapters:** The `data` block concept can be extended to support various external systems -- databases (SQL queries whose results populate EDBs), message queues (streams), files (CSV/JSON ingestion), etc. This requires writing connectors that translate external updates into insertions/deletions of facts in the Flowlog engine. The DSL would remain the same; only the configuration inside the `data` block might specify the connection details. For example, a `data "sql" "mydb"` could fetch rows from a database and treat them as facts. Because Flowlog's execution can be incremental, we could even push updates from the database (with triggers or change data capture) to the logic continuously. Custom data adapters can be added so that users can declaratively subscribe to new sources without coding new logic; just writing a new `data "<type>" "<name>"` with appropriate settings might load a plugin. This makes the system scalable in integrating with real-world data.

- **User-Defined Functions or Predicates:** We might allow calling external functions in HCL expressions (for example, a function to convert data formats, or a predicate that performs some calculation not easily done in pure Datalog). HCL expression syntax already supports certain built-ins and could be extended. The compiled Datalog could either incorporate these via built-in predicates or by extending the Flowlog engine with foreign function interfaces. For instance, an attribute `timestamp = time.now()` might call a function to get current time and unify that as a constant in a rule.

- **Complex Types and Adts:** Datalog (especially in systems like Soufflé) supports structured types (records/tuples, variants). The HCL DSL could expose this by allowing attributes that are objects or tuples. For example, one could assign a tuple value to an attribute, and the compiler would treat it as a record type in Datalog. This way, the DSL could handle more complex data without flatting everything into strings or numbers. The user would remain oblivious to the underlying type system; they'd just use HCL's natural syntax for lists, maps, etc., which can map to logical complex terms.

- **Integration with Flowlog's Optimizations:** Since the back-end is Flowlog (which in one context is a high-performance Datalog engine[\[12\]](https://www.flowlog-rs.com/#:~:text=A%20Datalog%20engine%20powered%20by,maintains%20the%20query%20results%20incrementally)[\[13\]](https://arxiv.org/html/2511.00865v1#:~:text=This%20paper%20bridges%20this%20gap,case%20joins)), we should ensure our compiled output can take advantage of its capabilities. Flowlog (the engine) separates the *logical optimization* from execution[\[14\]](https://arxiv.org/html/2511.00865v1#:~:text=In%20this%20section%2C%20we%20present,We%20incorporate%20some). As we generate the IR or rules, we could include annotations or allow user hints for performance (for example, indicating which attribute is a key for an index, which could be passed as a hint to the engine). While not a language feature per se, giving users some control (in HCL form) over performance-related configurations might be valuable for large-scale uses.

- **Error Handling and Validation:** HCL is known for good error messages and a clear structure. We can build on this to catch logic errors early. For instance, if a user writes a reference to a block that doesn't exist, or a type mismatch (treating a number as a block reference), the HCL parser or our compiler will flag it. We can also prevent certain unsafe patterns (like negation in recursion) by static analysis of the dependency graph, informing the user in config terms ("block X cannot reference Y with negation because it would cause a negative cycle"). This keeps the DSL user-friendly even as we add complex features.

- **Testing and Simulation:** As the DSL grows, we could allow a "dry run" or test mode where a user provides sample data (perhaps via `data` blocks pointing to test files) and writes assertions as special blocks (like expected outputs), then runs the engine to verify the logic. This would be akin to unit testing the logic at the config level.

In conclusion, using HCL as a front-end for Flowlog marries the accessibility of a widely-used configuration language with the power of Datalog's logical inference and fixpoint computation. We've defined how core constructs map to logical concepts, how the execution model works, and shown simple examples. With this design, end users can declaratively write complex logic (including recursive rules, queries over data streams, etc.) without ever writing or understanding Datalog -- the system will handle the translation and execution. This can be extended with more advanced logic features (aggregates, negation[\[11\]](https://arxiv.org/html/2511.00865v1#:~:text=Common%20Datalog%20Extensions,in%20a%20graph%20that%20are)) and integrations, all within the familiar HCL syntax. The result is a highly expressive yet user-friendly DSL for a variety of domains where Datalog/Flowlog is applicable (network policy, security rules, data analytics, etc.), lowering the barrier to entry for logic programming.

**Sources:** The concepts in this document draw on Datalog semantics and HCL usage conventions. HCL's design as a human-friendly configuration syntax[\[1\]](https://github.com/hashicorp/hcl#:~:text=HCL%20attempts%20to%20strike%20a,use%20in%20more%20complex%20applications) makes it suitable for expressing declarative logic. Datalog's EDB/IDB distinction and fixpoint evaluation are well-established[\[15\]](https://arxiv.org/html/2511.00865v1#:~:text=of%20occurs%20in%20some%20The,of%20at%20least%20one%20rule)[\[10\]](https://arxiv.org/html/2511.00865v1#:~:text=The%20straightforward%20way%20to%20implement,a%20fixpoint%20is%20reached). The module system is inspired by Terraform modules and Soufflé's component system for Datalog[\[4\]](https://souffle-lang.github.io/examples#:~:text=The%20,be%20accessed%20by%20other%20rules), enabling safe reuse of logic. The potential extensions like aggregates and negation align with common Datalog extensions for practical use[\[11\]](https://arxiv.org/html/2511.00865v1#:~:text=Common%20Datalog%20Extensions,in%20a%20graph%20that%20are). This synergy of technologies aims to deliver the approachability of HCL with the analytical power of Flowlog.

------------------------------------------------------------------------

[\[1\]](https://github.com/hashicorp/hcl#:~:text=HCL%20attempts%20to%20strike%20a,use%20in%20more%20complex%20applications) GitHub - hashicorp/hcl: HCL is the HashiCorp configuration language.

<https://github.com/hashicorp/hcl>

[\[2\]](https://holos.run/blog/why-cue-for-configuration/#:~:text=Rego%20) Why CUE for Configuration \| Holos

<https://holos.run/blog/why-cue-for-configuration/>

[\[3\]](https://arxiv.org/html/2511.00865v1#:~:text=of%20occurs%20in%20some%20The,of%20at%20least%20one%20rule) [\[5\]](https://arxiv.org/html/2511.00865v1#:~:text=The%20straightforward%20way%20to%20implement,a%20fixpoint%20is%20reached) [\[7\]](https://arxiv.org/html/2511.00865v1#:~:text=Datalog%20queries,2022%3B%20Shaikhha%20et%C2%A0al) [\[8\]](https://arxiv.org/html/2511.00865v1#:~:text=Dependency%20Graph%20and%20Stratification,and%20in%20the%20topological%20order) [\[9\]](https://arxiv.org/html/2511.00865v1#:~:text=r1.%20reach%28x%29%20%3A) [\[10\]](https://arxiv.org/html/2511.00865v1#:~:text=The%20straightforward%20way%20to%20implement,a%20fixpoint%20is%20reached) [\[11\]](https://arxiv.org/html/2511.00865v1#:~:text=Common%20Datalog%20Extensions,in%20a%20graph%20that%20are) [\[13\]](https://arxiv.org/html/2511.00865v1#:~:text=This%20paper%20bridges%20this%20gap,case%20joins) [\[14\]](https://arxiv.org/html/2511.00865v1#:~:text=In%20this%20section%2C%20we%20present,We%20incorporate%20some) [\[15\]](https://arxiv.org/html/2511.00865v1#:~:text=of%20occurs%20in%20some%20The,of%20at%20least%20one%20rule) FlowLog: Efficient and Extensible Datalog via Incrementality

<https://arxiv.org/html/2511.00865v1>

[\[4\]](https://souffle-lang.github.io/examples#:~:text=The%20,be%20accessed%20by%20other%20rules) Examples \| Soufflé • A Datalog Synthesis Tool for Static Analysis

<https://souffle-lang.github.io/examples>

[\[6\]](https://developer.hashicorp.com/terraform/language/values/outputs#:~:text=configurations.%20The%20,the%20following%20purposes%20in%20Terraform) Use outputs to expose module data \| Terraform \| HashiCorp Developer

<https://developer.hashicorp.com/terraform/language/values/outputs>

[\[12\]](https://www.flowlog-rs.com/#:~:text=A%20Datalog%20engine%20powered%20by,maintains%20the%20query%20results%20incrementally) FlowLog - Efficient and Extensible Datalog \| FlowLog

<https://www.flowlog-rs.com/>
