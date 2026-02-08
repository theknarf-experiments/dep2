use std::collections::HashSet;

use dbflow_core::compiler::{compile, emit_datalog, to_datalog_var, write_facts, StringTable};
use dbflow_core::hcl_types::{HclExpr, HclOutput, HclProgram, HclResource, HclValue, Reference};
use dbflow_core::reference::{analyze_dependencies, resolve_variables, BlockKind};
use indexmap::IndexMap;
use parsing::head::HeadArg;
use parsing::rule::{AtomArg, Const, Predicate};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

/// Valid HCL / Datalog identifier: starts with lowercase letter, then lowercase
/// letters, digits, or underscores. Length 1–8.
fn arb_identifier() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,7}"
}

/// Arbitrary HclValue.
fn arb_hcl_value() -> impl Strategy<Value = HclValue> {
    prop_oneof![
        any::<i32>().prop_map(HclValue::Integer),
        "[a-zA-Z0-9_ ]{0,20}".prop_map(HclValue::String),
        any::<bool>().prop_map(HclValue::Bool),
    ]
}

/// An EDB resource with only literal attributes. 1–4 attributes.
fn arb_edb_resource(type_name: String) -> impl Strategy<Value = HclResource> {
    // Generate 1–4 unique attribute name/value pairs.
    proptest::collection::vec((arb_identifier(), arb_hcl_value()), 1..=4).prop_map(move |pairs| {
        let mut attributes = IndexMap::new();
        for (name, value) in pairs {
            attributes.insert(name, HclExpr::Literal(value));
        }
        HclResource {
            type_name: type_name.clone(),
            label: String::new(), // filled in by program generator
            attributes,
        }
    })
}

/// An EDB-only program with 1–5 resources. Each resource gets a unique
/// (type_name, label) pair by appending the index.
fn arb_edb_program() -> impl Strategy<Value = HclProgram> {
    // Generate 1-5 type names, then build resources.
    // Each resource gets a unique type_name (by appending index) so the
    // compiler's schema-union logic doesn't require matching attribute sets.
    proptest::collection::vec(arb_identifier(), 1..=5)
        .prop_flat_map(|type_names| {
            let strategies: Vec<_> = type_names
                .into_iter()
                .enumerate()
                .map(|(i, tn)| {
                    let unique_type = format!("{}{}", tn, i);
                    arb_edb_resource(unique_type).prop_map(move |mut r| {
                        r.label = format!("l{}", i);
                        r
                    })
                })
                .collect();
            strategies
        })
        .prop_map(|resources| HclProgram {
            variables: Default::default(),
            resources,
            outputs: vec![],
            modules: vec![],
            data_blocks: vec![],
        })
}

/// A "mixed" program: first some EDB base blocks, then IDB blocks that
/// reference the EDB blocks' attributes.
fn arb_mixed_program() -> impl Strategy<Value = HclProgram> {
    // 1-3 EDB resources with a shared type, then 1-2 IDB resources referencing them.
    let edb_type = arb_identifier();
    let idb_type = arb_identifier();

    (edb_type, idb_type, arb_hcl_value(), arb_hcl_value()).prop_flat_map(
        |(edb_tn, idb_tn, v1, _v2)| {
            // Guard: EDB and IDB type names must differ.
            let idb_tn = if idb_tn == edb_tn {
                format!("{}x", idb_tn)
            } else {
                idb_tn
            };

            let attr_name = arb_identifier();
            attr_name.prop_map(move |attr| {
                let mut edb_attrs = IndexMap::new();
                edb_attrs.insert(attr.clone(), HclExpr::Literal(v1.clone()));

                let edb = HclResource {
                    type_name: edb_tn.clone(),
                    label: "base".into(),
                    attributes: edb_attrs,
                };

                let mut idb_attrs = IndexMap::new();
                idb_attrs.insert(
                    attr.clone(),
                    HclExpr::Reference(Reference {
                        block_type: edb_tn.clone(),
                        block_label: "base".into(),
                        field: attr.clone(),
                    }),
                );

                let idb = HclResource {
                    type_name: idb_tn.clone(),
                    label: "derived".into(),
                    attributes: idb_attrs,
                };

                HclProgram {
                    variables: Default::default(),
                    resources: vec![edb, idb],
                    outputs: vec![],
                    modules: vec![],
                    data_blocks: vec![],
                }
            })
        },
    )
}

/// A mixed program (EDB + IDB) with an output block referencing the IDB resource.
fn arb_mixed_program_with_output() -> impl Strategy<Value = HclProgram> {
    let edb_type = arb_identifier();
    let idb_type = arb_identifier();

    (edb_type, idb_type, arb_hcl_value(), arb_hcl_value()).prop_flat_map(
        |(edb_tn, idb_tn, v1, _v2)| {
            let idb_tn = if idb_tn == edb_tn {
                format!("{}x", idb_tn)
            } else {
                idb_tn
            };

            let attr_name = arb_identifier();
            attr_name.prop_map(move |attr| {
                let mut edb_attrs = IndexMap::new();
                edb_attrs.insert(attr.clone(), HclExpr::Literal(v1.clone()));

                let edb = HclResource {
                    type_name: edb_tn.clone(),
                    label: "base".into(),
                    attributes: edb_attrs,
                };

                let mut idb_attrs = IndexMap::new();
                idb_attrs.insert(
                    attr.clone(),
                    HclExpr::Reference(Reference {
                        block_type: edb_tn.clone(),
                        block_label: "base".into(),
                        field: attr.clone(),
                    }),
                );

                let idb = HclResource {
                    type_name: idb_tn.clone(),
                    label: "derived".into(),
                    attributes: idb_attrs,
                };

                let output = HclOutput {
                    name: "out".into(),
                    value: HclExpr::Reference(Reference {
                        block_type: idb_tn.clone(),
                        block_label: "derived".into(),
                        field: attr.clone(),
                    }),
                };

                HclProgram {
                    variables: Default::default(),
                    resources: vec![edb, idb],
                    outputs: vec![output],
                    modules: vec![],
                    data_blocks: vec![],
                }
            })
        },
    )
}

// ---------------------------------------------------------------------------
// A. StringTable round-trip
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn string_table_intern_then_decode(s in "[a-zA-Z0-9_. ]{0,50}") {
        let mut st = StringTable::default();
        let id = st.intern(&s);
        prop_assert_eq!(st.decode(id), Some(s.as_str()));
    }

    #[test]
    fn string_table_intern_idempotent(s in "[a-zA-Z0-9_. ]{0,50}") {
        let mut st = StringTable::default();
        let id1 = st.intern(&s);
        let id2 = st.intern(&s);
        prop_assert_eq!(id1, id2);
    }

    #[test]
    fn string_table_distinct_strings_distinct_ids(
        a in "[a-z]{1,10}",
        b in "[a-z]{1,10}",
    ) {
        prop_assume!(a != b);
        let mut st = StringTable::default();
        let id_a = st.intern(&a);
        let id_b = st.intern(&b);
        prop_assert_ne!(id_a, id_b);
    }
}

// ---------------------------------------------------------------------------
// B. to_datalog_var transformation
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn datalog_var_non_empty(s in "[a-z][a-z0-9_]{0,15}") {
        let result = to_datalog_var(&s);
        prop_assert!(!result.is_empty());
    }

    #[test]
    fn datalog_var_starts_uppercase(s in "[a-z][a-z0-9_]{0,15}") {
        let result = to_datalog_var(&s);
        prop_assert!(result.chars().next().unwrap().is_uppercase());
    }

    #[test]
    fn datalog_var_no_underscores(s in "[a-z][a-z0-9_]{0,15}") {
        let result = to_datalog_var(&s);
        prop_assert!(!result.contains('_'));
    }

    #[test]
    fn datalog_var_idempotent(s in "[a-z][a-z0-9_]{0,15}") {
        let once = to_datalog_var(&s);
        let twice = to_datalog_var(&once);
        // CamelCase with no underscores → applying again should be identity
        // (each "part" split on _ is already capitalized and there are no _).
        prop_assert_eq!(&once, &twice);
    }
}

// ---------------------------------------------------------------------------
// C. EDB / IDB classification
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn edb_only_program_all_edb(prog in arb_edb_program()) {
        let analysis = analyze_dependencies(&prog);
        for resource in &prog.resources {
            let id = (resource.type_name.clone(), resource.label.clone());
            let kind = analysis.block_kinds.get(&id).unwrap();
            prop_assert_eq!(kind, &BlockKind::Edb,
                "Expected EDB for {}.{}", resource.type_name, resource.label);
        }
    }

    #[test]
    fn mixed_program_classification(prog in arb_mixed_program()) {
        let analysis = analyze_dependencies(&prog);
        for resource in &prog.resources {
            let id = (resource.type_name.clone(), resource.label.clone());
            let kind = analysis.block_kinds.get(&id).unwrap();
            let has_refs = resource.attributes.values().any(|e| matches!(e, HclExpr::Reference(_)));
            if has_refs {
                prop_assert_eq!(kind, &BlockKind::Idb,
                    "Expected IDB for {}.{}", resource.type_name, resource.label);
            } else {
                prop_assert_eq!(kind, &BlockKind::Edb,
                    "Expected EDB for {}.{}", resource.type_name, resource.label);
            }
        }
    }

    #[test]
    fn topo_order_contains_all_blocks(prog in arb_edb_program()) {
        let analysis = analyze_dependencies(&prog);
        let topo_set: HashSet<_> = analysis.topo_order.iter().collect();
        for resource in &prog.resources {
            let id = (resource.type_name.clone(), resource.label.clone());
            prop_assert!(topo_set.contains(&id),
                "Block {}.{} missing from topo_order", resource.type_name, resource.label);
        }
        // Exactly one entry per resource.
        prop_assert_eq!(analysis.topo_order.len(), prog.resources.len());
    }
}

// ---------------------------------------------------------------------------
// D. Variable resolution
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn resolve_variables_replaces_varrefs(
        var_name in arb_identifier(),
        val in arb_hcl_value(),
    ) {
        let mut prog = HclProgram {
            variables: [(var_name.clone(), val.clone())].into_iter().collect(),
            resources: vec![HclResource {
                type_name: "test".into(),
                label: "t0".into(),
                attributes: {
                    let mut m = IndexMap::new();
                    m.insert("attr".into(), HclExpr::VarRef(var_name.clone()));
                    m
                },
            }],
            outputs: vec![],
            modules: vec![],
            data_blocks: vec![],
        };

        resolve_variables(&mut prog);

        let expr = prog.resources[0].attributes.get("attr").unwrap();
        match expr {
            HclExpr::Literal(_) => { /* ok — was resolved */ }
            other => prop_assert!(false,
                "Expected Literal after resolution, got {:?}", other),
        }
    }

    #[test]
    fn resolve_variables_preserves_literals(val in arb_hcl_value()) {
        let mut prog = HclProgram {
            variables: Default::default(),
            resources: vec![HclResource {
                type_name: "test".into(),
                label: "t0".into(),
                attributes: {
                    let mut m = IndexMap::new();
                    m.insert("attr".into(), HclExpr::Literal(val.clone()));
                    m
                },
            }],
            outputs: vec![],
            modules: vec![],
            data_blocks: vec![],
        };

        resolve_variables(&mut prog);

        let expr = prog.resources[0].attributes.get("attr").unwrap();
        match expr {
            HclExpr::Literal(_) => { /* still a literal — correct */ }
            other => prop_assert!(false,
                "Expected Literal to be preserved, got {:?}", other),
        }
    }
}

// ---------------------------------------------------------------------------
// E. Compile structural invariants (EDB-only programs)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn compile_edb_fact_count(prog in arb_edb_program()) {
        let n_resources = prog.resources.len();
        let result = compile(prog, None, &[], &[]);
        // EDB-only programs should compile successfully.
        let result = result.unwrap();
        let total_facts: usize = result.edb_facts.values().map(|v| v.len()).sum();
        prop_assert_eq!(total_facts, n_resources);
    }

    #[test]
    fn compile_edb_tuple_arity(prog in arb_edb_program()) {
        let expected: Vec<(String, String, usize)> = prog.resources.iter().map(|r| {
            (r.type_name.clone(), r.label.clone(), r.attributes.len())
        }).collect();

        let result = compile(prog, None, &[], &[]).unwrap();

        for (type_name, _label, n_attrs) in &expected {
            if let Some(facts) = result.edb_facts.get(type_name) {
                for tuple in facts {
                    // arity = 1 (label) + number of attributes in the schema for this type
                    // The schema is the union of all attributes across blocks of this type,
                    // so tuple arity >= 1 + n_attrs.
                    prop_assert!(tuple.len() >= 1 + n_attrs,
                        "Tuple arity {} < 1 + {} for type {}", tuple.len(), n_attrs, type_name);
                }
            }
        }
    }

    #[test]
    fn compile_edb_first_element_is_label(prog in arb_edb_program()) {
        // Collect labels per type so we can verify the decoded first element.
        let labels: Vec<(String, String)> = prog.resources.iter().map(|r| {
            (r.type_name.clone(), r.label.clone())
        }).collect();

        let result = compile(prog, None, &[], &[]).unwrap();

        // For each resource, find its fact and check the first element.
        for (type_name, label) in &labels {
            if let Some(facts) = result.edb_facts.get(type_name) {
                // Find the fact whose first element decodes to this label.
                let found = facts.iter().any(|tuple| {
                    result.string_table.decode(tuple[0]) == Some(label.as_str())
                });
                prop_assert!(found,
                    "No fact for {}.{} with label as first element", type_name, label);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// F. write_facts round-trip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn write_facts_roundtrip(prog in arb_edb_program()) {
        let result = compile(prog, None, &[], &[]).unwrap();

        let dir = tempfile::tempdir().unwrap();
        write_facts(&result.edb_facts, dir.path()).unwrap();

        // Read back each .facts file and compare.
        for (rel_name, facts) in &result.edb_facts {
            let path = dir.path().join(format!("{}.facts", rel_name));
            let content = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = content.lines().collect();
            prop_assert_eq!(lines.len(), facts.len(),
                "Line count mismatch for {}", rel_name);

            for (line, tuple) in lines.iter().zip(facts.iter()) {
                let vals: Vec<i32> = line
                    .split('\t')
                    .map(|v| v.parse::<i32>().unwrap())
                    .collect();
                prop_assert_eq!(&vals, tuple,
                    "Tuple mismatch in {}.facts", rel_name);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// G. String interning invariants (no Const::Text in compiled rules)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// After compilation, no rule body contains Const::Text — all constants must be Const::Integer.
    #[test]
    fn no_const_text_in_rules(prog in arb_mixed_program()) {
        let result = compile(prog, None, &[], &[]).unwrap();

        for rule in result.program.rules() {
            for pred in rule.rhs() {
                if let Predicate::AtomPredicate(atom) = pred {
                    for arg in atom.arguments() {
                        if let AtomArg::Const(c) = arg {
                            prop_assert!(
                                matches!(c, Const::Integer(_)),
                                "Found Const::Text in rule body: {} — rule: {}",
                                c, rule
                            );
                        }
                    }
                }
            }
        }
    }

    /// IDB rule heads use HeadArg::Var("HclLabel") for the label position,
    /// bound via a helper EDB atom in the body.
    #[test]
    fn idb_head_label_bound_via_edb(prog in arb_mixed_program()) {
        let result = compile(prog, None, &[], &[]).unwrap();

        for rule in result.program.rules() {
            let head_args = rule.head().head_arguments();
            prop_assert!(!head_args.is_empty(), "Rule head has no arguments: {}", rule);

            // First head arg should be HeadArg::Var("HclLabel").
            if let HeadArg::Var(s) = &head_args[0] {
                prop_assert_eq!(s, "HclLabel",
                    "Expected HclLabel as first head arg, got '{}' in rule: {}", s, rule);
            } else {
                prop_assert!(false, "First head arg is not HeadArg::Var: {}", rule);
            }

            // Body should contain a label-binding EDB atom (_hcl_lbl_*).
            let has_label_edb = rule.rhs().iter().any(|pred| {
                if let Predicate::AtomPredicate(atom) = pred {
                    atom.name().starts_with("_hcl_lbl_")
                } else {
                    false
                }
            });
            prop_assert!(has_label_edb,
                "No _hcl_lbl_ EDB atom in rule body: {}", rule);
        }
    }

    /// The interned label constant in an IDB rule body atom matches a label
    /// in the corresponding EDB facts, ensuring joins will succeed at runtime.
    #[test]
    fn idb_body_label_matches_edb_fact(prog in arb_mixed_program()) {
        let result = compile(prog, None, &[], &[]).unwrap();

        for rule in result.program.rules() {
            for pred in rule.rhs() {
                if let Predicate::AtomPredicate(atom) = pred {
                    let args = atom.arguments();
                    prop_assert!(!args.is_empty(),
                        "Body atom has no arguments: {}", rule);

                    if let AtomArg::Const(Const::Integer(label_id)) = &args[0] {
                        let rel_name = atom.name();
                        if let Some(facts) = result.edb_facts.get(rel_name) {
                            let found = facts.iter().any(|tuple| tuple[0] == *label_id);
                            prop_assert!(found,
                                "Body atom label id {} not found in {}.facts — rule: {}",
                                label_id, rel_name, rule);
                        }
                        // If no EDB facts for this relation, it's an IDB-to-IDB join — skip.
                    }
                }
            }
        }
    }

    /// Output rules referencing a resource use the correct interned label.
    #[test]
    fn output_rule_label_matches_source(prog in arb_mixed_program_with_output()) {
        let result = compile(prog, None, &[], &[]).unwrap();

        // Find the output rule (head name starts with "hcl_output_").
        let output_rule = result.program.rules().iter()
            .find(|r| r.head().name().starts_with("hcl_output_"));
        prop_assert!(output_rule.is_some(), "No output rule found");
        let output_rule = output_rule.unwrap();

        // The body should have at least one atom predicate.
        let body_atom = output_rule.rhs().iter().find_map(|p| {
            if let Predicate::AtomPredicate(atom) = p { Some(atom) } else { None }
        });
        prop_assert!(body_atom.is_some(), "Output rule has no body atom: {}", output_rule);
        let body_atom = body_atom.unwrap();

        // First argument should be Const::Integer (the interned label).
        let first_arg = &body_atom.arguments()[0];
        if let AtomArg::Const(Const::Integer(id)) = first_arg {
            let decoded = result.string_table.decode(*id);
            prop_assert_eq!(decoded, Some("derived"),
                "Output rule body label decodes to {:?}, expected \"derived\" — rule: {}",
                decoded, output_rule);
        } else {
            prop_assert!(false,
                "Output rule body first arg is not Const::Integer: {:?} — rule: {}",
                first_arg, output_rule);
        }
    }

    /// emit_datalog() includes a comment line for every interned string.
    #[test]
    fn emit_datalog_contains_string_table(prog in arb_mixed_program()) {
        let result = compile(prog, None, &[], &[]).unwrap();
        let dl = emit_datalog(&result);

        // Iterate all interned strings via decode(0), decode(1), ...
        let mut id = 0i32;
        while let Some(s) = result.string_table.decode(id) {
            let expected = format!("// {} = \"{}\"", id, s);
            prop_assert!(dl.contains(&expected),
                "Missing string table entry in emit_datalog: '{}'\nOutput:\n{}", expected, dl);
            id += 1;
        }
        // There should be at least one interned string (labels are always interned).
        prop_assert!(id > 0, "No strings were interned");
    }

    /// Rule lines in emitted Datalog contain no quoted strings as atom arguments.
    #[test]
    fn emit_datalog_no_quoted_strings_in_rules(prog in arb_mixed_program()) {
        let result = compile(prog, None, &[], &[]).unwrap();
        let dl = emit_datalog(&result);

        for line in dl.lines() {
            if line.contains(":-") {
                // This is a rule line. It should not contain quoted strings
                // in argument positions (e.g., pred("foo", X)).
                prop_assert!(!line.contains("(\""),
                    "Rule line contains quoted string argument: {}", line);
                prop_assert!(!line.contains(", \""),
                    "Rule line contains quoted string argument: {}", line);
            }
        }
    }
}
