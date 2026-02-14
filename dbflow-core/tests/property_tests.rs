use std::collections::HashSet;

use dbflow_core::compiler::{
    compile, emit_datalog, to_datalog_var, write_facts, CompileError, FetchedDataBlock,
    ScalarFnKind, StringTable,
};
use dbflow_core::compiler::compile::apply_scalar_fn;
use dbflow_core::hcl_types::{
    HclAggregateOp, HclArithmeticOp, HclComparisonOp, HclExpr, HclOutput, HclProgram,
    HclResource, HclValue, Reference,
};
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
        any::<i64>().prop_map(HclValue::Integer),
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
                let vals: Vec<i64> = line
                    .split('\t')
                    .map(|v| v.parse::<i64>().unwrap())
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
        let mut id = 0i64;
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

// ---------------------------------------------------------------------------
// H. InvalidEdbExpr — EDB blocks with non-literal expressions
// ---------------------------------------------------------------------------

/// Helper: build an EDB resource with one good literal attribute and one bad
/// attribute containing the given expression. No references, so it stays EDB.
fn arb_edb_with_bad_expr(bad_expr: HclExpr) -> HclProgram {
    let mut attrs = IndexMap::new();
    attrs.insert("good".into(), HclExpr::Literal(HclValue::Integer(1i64)));
    attrs.insert("bad".into(), bad_expr);
    HclProgram {
        variables: Default::default(),
        resources: vec![HclResource {
            type_name: "edbtype".into(),
            label: "l0".into(),
            attributes: attrs,
        }],
        outputs: vec![],
        modules: vec![],
        data_blocks: vec![],
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn invalid_edb_comparison_rejected(
        lhs in any::<i64>(),
        rhs in any::<i64>(),
    ) {
        let prog = arb_edb_with_bad_expr(HclExpr::Comparison {
            lhs: Box::new(HclExpr::Literal(HclValue::Integer(lhs))),
            operator: HclComparisonOp::Greater,
            rhs: Box::new(HclExpr::Literal(HclValue::Integer(rhs))),
        });
        let result = compile(prog, None, &[], &[]);
        prop_assert!(matches!(result, Err(CompileError::InvalidEdbExpr { .. })),
            "Expected InvalidEdbExpr, got {:?}", result.err());
    }

    #[test]
    fn invalid_edb_aggregate_rejected(val in any::<i64>()) {
        let prog = arb_edb_with_bad_expr(HclExpr::Aggregate {
            operator: HclAggregateOp::Sum,
            argument: Box::new(HclExpr::Literal(HclValue::Integer(val))),
        });
        let result = compile(prog, None, &[], &[]);
        prop_assert!(matches!(result, Err(CompileError::InvalidEdbExpr { .. })),
            "Expected InvalidEdbExpr, got {:?}", result.err());
    }

    #[test]
    fn invalid_edb_arithmetic_rejected(
        lhs in any::<i64>(),
        rhs in any::<i64>(),
    ) {
        let prog = arb_edb_with_bad_expr(HclExpr::ArithmeticOp {
            lhs: Box::new(HclExpr::Literal(HclValue::Integer(lhs))),
            operator: HclArithmeticOp::Plus,
            rhs: Box::new(HclExpr::Literal(HclValue::Integer(rhs))),
        });
        let result = compile(prog, None, &[], &[]);
        prop_assert!(matches!(result, Err(CompileError::InvalidEdbExpr { .. })),
            "Expected InvalidEdbExpr, got {:?}", result.err());
    }

    #[test]
    fn invalid_edb_function_rejected(val in any::<i64>()) {
        let prog = arb_edb_with_bad_expr(HclExpr::FunctionCall {
            name: "abs".into(),
            args: vec![HclExpr::Literal(HclValue::Integer(val))],
        });
        let result = compile(prog, None, &[], &[]);
        prop_assert!(matches!(result, Err(CompileError::InvalidEdbExpr { .. })),
            "Expected InvalidEdbExpr, got {:?}", result.err());
    }
}

// ---------------------------------------------------------------------------
// I. UnknownReference — dangling references in outputs and IDB rules
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn output_unknown_resource_type(
        edb_type in arb_identifier(),
        attr in arb_identifier(),
        val in arb_hcl_value(),
    ) {
        let mut edb_attrs = IndexMap::new();
        edb_attrs.insert(attr.clone(), HclExpr::Literal(val));

        let prog = HclProgram {
            variables: Default::default(),
            resources: vec![HclResource {
                type_name: edb_type.clone(),
                label: "l0".into(),
                attributes: edb_attrs,
            }],
            outputs: vec![HclOutput {
                name: "out".into(),
                value: HclExpr::Reference(Reference {
                    block_type: format!("{}_nonexistent", edb_type),
                    block_label: "l0".into(),
                    field: attr,
                }),
            }],
            modules: vec![],
            data_blocks: vec![],
        };

        let result = compile(prog, None, &[], &[]);
        prop_assert!(matches!(result, Err(CompileError::UnknownReference { .. })),
            "Expected UnknownReference, got {:?}", result.err());
    }

    #[test]
    fn output_unknown_field(
        edb_type in arb_identifier(),
        attr in arb_identifier(),
        val in arb_hcl_value(),
    ) {
        let mut edb_attrs = IndexMap::new();
        edb_attrs.insert(attr.clone(), HclExpr::Literal(val));

        let prog = HclProgram {
            variables: Default::default(),
            resources: vec![HclResource {
                type_name: edb_type.clone(),
                label: "l0".into(),
                attributes: edb_attrs,
            }],
            outputs: vec![HclOutput {
                name: "out".into(),
                value: HclExpr::Reference(Reference {
                    block_type: edb_type,
                    block_label: "l0".into(),
                    field: format!("{}_nonexistent", attr),
                }),
            }],
            modules: vec![],
            data_blocks: vec![],
        };

        let result = compile(prog, None, &[], &[]);
        prop_assert!(matches!(result, Err(CompileError::UnknownReference { .. })),
            "Expected UnknownReference, got {:?}", result.err());
    }

    #[test]
    fn idb_unknown_reference_type(
        edb_type in arb_identifier(),
        idb_type in arb_identifier(),
        attr in arb_identifier(),
        val in arb_hcl_value(),
    ) {
        // Ensure distinct type names.
        let idb_type = if idb_type == edb_type {
            format!("{}x", idb_type)
        } else {
            idb_type
        };

        let mut edb_attrs = IndexMap::new();
        edb_attrs.insert(attr.clone(), HclExpr::Literal(val));

        let mut idb_attrs = IndexMap::new();
        idb_attrs.insert(
            attr.clone(),
            HclExpr::Reference(Reference {
                block_type: format!("{}_bad", edb_type),
                block_label: "l0".into(),
                field: attr,
            }),
        );

        let prog = HclProgram {
            variables: Default::default(),
            resources: vec![
                HclResource {
                    type_name: edb_type,
                    label: "l0".into(),
                    attributes: edb_attrs,
                },
                HclResource {
                    type_name: idb_type,
                    label: "derived".into(),
                    attributes: idb_attrs,
                },
            ],
            outputs: vec![],
            modules: vec![],
            data_blocks: vec![],
        };

        let result = compile(prog, None, &[], &[]);
        prop_assert!(matches!(result, Err(CompileError::UnknownReference { .. })),
            "Expected UnknownReference, got {:?}", result.err());
    }
}

// ---------------------------------------------------------------------------
// J. MissingAttribute — schema mismatch between same-type resources
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn missing_attribute_rejected(
        type_name in arb_identifier(),
        attr_a in arb_identifier(),
        attr_b in arb_identifier(),
        val_a1 in arb_hcl_value(),
        val_a2 in arb_hcl_value(),
        val_b in arb_hcl_value(),
    ) {
        // Ensure attr_a and attr_b are distinct.
        let attr_b = if attr_b == attr_a {
            format!("{}x", attr_b)
        } else {
            attr_b
        };

        // First resource has attrs {a, b}.
        let mut attrs1 = IndexMap::new();
        attrs1.insert(attr_a.clone(), HclExpr::Literal(val_a1));
        attrs1.insert(attr_b.clone(), HclExpr::Literal(val_b));

        // Second resource has only attr {a} — missing attr_b.
        let mut attrs2 = IndexMap::new();
        attrs2.insert(attr_a.clone(), HclExpr::Literal(val_a2));

        let prog = HclProgram {
            variables: Default::default(),
            resources: vec![
                HclResource {
                    type_name: type_name.clone(),
                    label: "r1".into(),
                    attributes: attrs1,
                },
                HclResource {
                    type_name,
                    label: "r2".into(),
                    attributes: attrs2,
                },
            ],
            outputs: vec![],
            modules: vec![],
            data_blocks: vec![],
        };

        let result = compile(prog, None, &[], &[]);
        prop_assert!(matches!(result, Err(CompileError::MissingAttribute { .. })),
            "Expected MissingAttribute, got {:?}", result.err());
    }
}

// ---------------------------------------------------------------------------
// K. InvalidExprContext — invalid expressions in output blocks
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn invalid_expr_in_output_rejected(
        choice in 0u8..5,
        val in any::<i64>(),
        attr in arb_identifier(),
        hcl_val in arb_hcl_value(),
    ) {
        let bad_expr = match choice {
            0 => HclExpr::Comparison {
                lhs: Box::new(HclExpr::Literal(HclValue::Integer(val))),
                operator: HclComparisonOp::Greater,
                rhs: Box::new(HclExpr::Literal(HclValue::Integer(0))),
            },
            1 => HclExpr::Aggregate {
                operator: HclAggregateOp::Sum,
                argument: Box::new(HclExpr::Literal(HclValue::Integer(val))),
            },
            2 => HclExpr::ArithmeticOp {
                lhs: Box::new(HclExpr::Literal(HclValue::Integer(val))),
                operator: HclArithmeticOp::Plus,
                rhs: Box::new(HclExpr::Literal(HclValue::Integer(1i64))),
            },
            3 => HclExpr::NegatedReference(Reference {
                block_type: "nonexistent".into(),
                block_label: "l0".into(),
                field: "f".into(),
            }),
            _ => HclExpr::FunctionCall {
                name: "abs".into(),
                args: vec![HclExpr::Literal(HclValue::Integer(val))],
            },
        };

        // Include a valid EDB resource so compilation proceeds past resource handling.
        let mut edb_attrs = IndexMap::new();
        edb_attrs.insert(attr, HclExpr::Literal(hcl_val));

        let prog = HclProgram {
            variables: Default::default(),
            resources: vec![HclResource {
                type_name: "edbtype".into(),
                label: "l0".into(),
                attributes: edb_attrs,
            }],
            outputs: vec![HclOutput {
                name: "out".into(),
                value: bad_expr,
            }],
            modules: vec![],
            data_blocks: vec![],
        };

        let result = compile(prog, None, &[], &[]);
        prop_assert!(matches!(result, Err(CompileError::InvalidExprContext { .. })),
            "Expected InvalidExprContext, got {:?}", result.err());
    }
}

// ---------------------------------------------------------------------------
// L. Edge cases
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn unresolved_varref_in_edb_rejected(var_name in arb_identifier()) {
        // VarRef doesn't count as a reference, so block is classified as EDB.
        // EDB path hits the fallthrough for non-literal expressions.
        let mut attrs = IndexMap::new();
        attrs.insert("good".into(), HclExpr::Literal(HclValue::Integer(1)));
        attrs.insert("bad".into(), HclExpr::VarRef(var_name));

        let prog = HclProgram {
            variables: Default::default(), // no variable defined
            resources: vec![HclResource {
                type_name: "edbtype".into(),
                label: "l0".into(),
                attributes: attrs,
            }],
            outputs: vec![],
            modules: vec![],
            data_blocks: vec![],
        };

        let result = compile(prog, None, &[], &[]);
        // VarRef in EDB is caught as InvalidEdbExpr (non-literal value).
        prop_assert!(matches!(result, Err(CompileError::InvalidEdbExpr { .. })),
            "Expected InvalidEdbExpr for unresolved VarRef, got {:?}", result.err());
    }

    #[test]
    fn edb_bool_values_encoded_as_integers(b in any::<bool>()) {
        let mut attrs = IndexMap::new();
        attrs.insert("flag".into(), HclExpr::Literal(HclValue::Bool(b)));

        let prog = HclProgram {
            variables: Default::default(),
            resources: vec![HclResource {
                type_name: "booltype".into(),
                label: "l0".into(),
                attributes: attrs,
            }],
            outputs: vec![],
            modules: vec![],
            data_blocks: vec![],
        };

        let result = compile(prog, None, &[], &[]).unwrap();
        let facts = &result.edb_facts["booltype"];
        prop_assert_eq!(facts.len(), 1);
        // Tuple is [label_id, flag_value].
        let flag_val = facts[0][1];
        let expected = if b { 1 } else { 0 };
        prop_assert_eq!(flag_val, expected,
            "Bool {} should encode as {}, got {}", b, expected, flag_val);
    }

    #[test]
    fn many_attributes_compile(n_attrs in 10usize..=20) {
        let mut attrs = IndexMap::new();
        for i in 0..n_attrs {
            attrs.insert(format!("a{}", i), HclExpr::Literal(HclValue::Integer(i as i64)));
        }

        let prog = HclProgram {
            variables: Default::default(),
            resources: vec![HclResource {
                type_name: "wide".into(),
                label: "l0".into(),
                attributes: attrs,
            }],
            outputs: vec![],
            modules: vec![],
            data_blocks: vec![],
        };

        let result = compile(prog, None, &[], &[]);
        prop_assert!(result.is_ok(), "Expected Ok for many-attribute EDB, got {:?}", result.err());
        let result = result.unwrap();
        let facts = &result.edb_facts["wide"];
        prop_assert_eq!(facts.len(), 1);
        // Tuple arity = 1 (label) + n_attrs.
        prop_assert_eq!(facts[0].len(), 1 + n_attrs);
    }
}

#[test]
fn empty_program_compiles() {
    let prog = HclProgram {
        variables: Default::default(),
        resources: vec![],
        outputs: vec![],
        modules: vec![],
        data_blocks: vec![],
    };

    let result = compile(prog, None, &[], &[]);
    assert!(result.is_ok(), "Empty program should compile, got {:?}", result.err());
    let result = result.unwrap();
    assert!(result.edb_facts.is_empty());
    assert!(result.outputs.is_empty());
    assert!(result.program.rules().is_empty());
}

// ---------------------------------------------------------------------------
// M. InvalidArithmeticExpr — string literal in comparison
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn string_in_comparison_rejected(
        edb_type in arb_identifier(),
        idb_type in arb_identifier(),
        attr in arb_identifier(),
        val in arb_hcl_value(),
        bad_str in "[a-z]{1,10}",
    ) {
        let idb_type = if idb_type == edb_type {
            format!("{}x", idb_type)
        } else {
            idb_type
        };

        let mut edb_attrs = IndexMap::new();
        edb_attrs.insert(attr.clone(), HclExpr::Literal(val));

        // IDB resource with _filter containing a string literal in comparison.
        let mut idb_attrs = IndexMap::new();
        idb_attrs.insert(
            attr.clone(),
            HclExpr::Reference(Reference {
                block_type: edb_type.clone(),
                block_label: "l0".into(),
                field: attr.clone(),
            }),
        );
        idb_attrs.insert(
            "_filter".into(),
            HclExpr::Comparison {
                lhs: Box::new(HclExpr::Reference(Reference {
                    block_type: edb_type.clone(),
                    block_label: "l0".into(),
                    field: attr,
                })),
                operator: HclComparisonOp::Greater,
                rhs: Box::new(HclExpr::Literal(HclValue::String(bad_str))),
            },
        );

        let prog = HclProgram {
            variables: Default::default(),
            resources: vec![
                HclResource {
                    type_name: edb_type,
                    label: "l0".into(),
                    attributes: edb_attrs,
                },
                HclResource {
                    type_name: idb_type,
                    label: "derived".into(),
                    attributes: idb_attrs,
                },
            ],
            outputs: vec![],
            modules: vec![],
            data_blocks: vec![],
        };

        let result = compile(prog, None, &[], &[]);
        prop_assert!(matches!(result, Err(CompileError::InvalidArithmeticExpr(_))),
            "Expected InvalidArithmeticExpr, got {:?}", result.err());
    }
}

// ---------------------------------------------------------------------------
// N. Stratified negation validation
// ---------------------------------------------------------------------------

#[test]
fn negation_in_mutual_recursion_rejected() {
    // A negates B (via negation), B references A → recursive SCC with negation edge.
    let mut a_attrs = IndexMap::new();
    a_attrs.insert(
        "val".into(),
        HclExpr::Literal(HclValue::String("base".into())),
    );
    a_attrs.insert(
        "not_b".into(),
        HclExpr::NegatedReference(Reference {
            block_type: "b".into(),
            block_label: "r".into(),
            field: "val".into(),
        }),
    );

    let mut b_attrs = IndexMap::new();
    b_attrs.insert(
        "val".into(),
        HclExpr::Reference(Reference {
            block_type: "a".into(),
            block_label: "r".into(),
            field: "val".into(),
        }),
    );

    let prog = HclProgram {
        variables: Default::default(),
        resources: vec![
            HclResource {
                type_name: "a".into(),
                label: "r".into(),
                attributes: a_attrs,
            },
            HclResource {
                type_name: "b".into(),
                label: "r".into(),
                attributes: b_attrs,
            },
        ],
        outputs: vec![],
        modules: vec![],
        data_blocks: vec![],
    };

    let result = compile(prog, None, &[], &[]);
    assert!(
        matches!(result, Err(CompileError::NegationInRecursion { .. })),
        "Expected NegationInRecursion, got {:?}",
        result.err()
    );
}

#[test]
fn negation_in_self_loop_rejected() {
    // A block that references itself and negates itself.
    let mut attrs = IndexMap::new();
    attrs.insert(
        "val".into(),
        HclExpr::Reference(Reference {
            block_type: "loop".into(),
            block_label: "r".into(),
            field: "val".into(),
        }),
    );
    attrs.insert(
        "not_self".into(),
        HclExpr::NegatedReference(Reference {
            block_type: "loop".into(),
            block_label: "r".into(),
            field: "val".into(),
        }),
    );

    let prog = HclProgram {
        variables: Default::default(),
        resources: vec![HclResource {
            type_name: "loop".into(),
            label: "r".into(),
            attributes: attrs,
        }],
        outputs: vec![],
        modules: vec![],
        data_blocks: vec![],
    };

    let result = compile(prog, None, &[], &[]);
    assert!(
        matches!(result, Err(CompileError::NegationInRecursion { .. })),
        "Expected NegationInRecursion, got {:?}",
        result.err()
    );
}

#[test]
fn negation_acyclic_ok() {
    // Negation in an acyclic graph should compile fine.
    let mut edb_attrs = IndexMap::new();
    edb_attrs.insert(
        "val".into(),
        HclExpr::Literal(HclValue::String("hello".into())),
    );

    let mut blocked_attrs = IndexMap::new();
    blocked_attrs.insert(
        "val".into(),
        HclExpr::Literal(HclValue::String("bad".into())),
    );

    let mut idb_attrs = IndexMap::new();
    idb_attrs.insert(
        "val".into(),
        HclExpr::Reference(Reference {
            block_type: "source".into(),
            block_label: "s".into(),
            field: "val".into(),
        }),
    );
    idb_attrs.insert(
        "not_blocked".into(),
        HclExpr::NegatedReference(Reference {
            block_type: "blocked".into(),
            block_label: "b".into(),
            field: "val".into(),
        }),
    );

    let prog = HclProgram {
        variables: Default::default(),
        resources: vec![
            HclResource {
                type_name: "source".into(),
                label: "s".into(),
                attributes: edb_attrs,
            },
            HclResource {
                type_name: "blocked".into(),
                label: "b".into(),
                attributes: blocked_attrs,
            },
            HclResource {
                type_name: "allowed".into(),
                label: "rule".into(),
                attributes: idb_attrs,
            },
        ],
        outputs: vec![],
        modules: vec![],
        data_blocks: vec![],
    };

    let result = compile(prog, None, &[], &[]);
    assert!(
        result.is_ok(),
        "Acyclic negation should compile, got {:?}",
        result.err()
    );
}

// ---------------------------------------------------------------------------
// O. Data block roundtrip via FetchedDataBlock
// ---------------------------------------------------------------------------

#[test]
fn large_integer_roundtrip() {
    // Pass a FetchedDataBlock with i64::MAX — should succeed now with i64 storage.
    let prog = HclProgram {
        variables: Default::default(),
        resources: vec![],
        outputs: vec![],
        modules: vec![],
        data_blocks: vec![],
    };

    let data_blocks = vec![FetchedDataBlock {
        provider_type: "csv".into(),
        label: "bigvals".into(),
        schema: dbflow_plugin::DataSchema {
            columns: vec![dbflow_plugin::ColumnDef {
                name: "big".into(),
                data_type: dbflow_plugin::DataType::Integer,
            }],
        },
        rows: vec![vec![dbflow_plugin::DataValue::Integer(i64::MAX)]],
    }];

    let result = compile(prog, None, &data_blocks, &[]);
    assert!(result.is_ok(), "Large i64 value should compile, got {:?}", result.err());
    let result = result.unwrap();
    let facts = &result.edb_facts["_data_csv_bigvals"];
    assert_eq!(facts[0][0], i64::MAX);
}

#[test]
fn data_value_i64_roundtrip() {
    // Pass a FetchedDataBlock with normal integer and string values; verify compilation
    // succeeds and facts are correctly encoded.
    let prog = HclProgram {
        variables: Default::default(),
        resources: vec![],
        outputs: vec![],
        modules: vec![],
        data_blocks: vec![],
    };

    let data_blocks = vec![FetchedDataBlock {
        provider_type: "csv".into(),
        label: "items".into(),
        schema: dbflow_plugin::DataSchema {
            columns: vec![
                dbflow_plugin::ColumnDef {
                    name: "name".into(),
                    data_type: dbflow_plugin::DataType::String,
                },
                dbflow_plugin::ColumnDef {
                    name: "qty".into(),
                    data_type: dbflow_plugin::DataType::Integer,
                },
            ],
        },
        rows: vec![
            vec![
                dbflow_plugin::DataValue::String("apple".into()),
                dbflow_plugin::DataValue::Integer(42),
            ],
            vec![
                dbflow_plugin::DataValue::String("banana".into()),
                dbflow_plugin::DataValue::Integer(-7),
            ],
        ],
    }];

    let result = compile(prog, None, &data_blocks, &[]).unwrap();
    let facts = &result.edb_facts["_data_csv_items"];
    assert_eq!(facts.len(), 2);

    // Integer column values should be raw i64.
    assert_eq!(facts[0][1], 42i64);
    assert_eq!(facts[1][1], -7i64);

    // String column values should be interned IDs that decode correctly.
    let name0 = result.string_table.decode(facts[0][0]);
    let name1 = result.string_table.decode(facts[1][0]);
    assert_eq!(name0, Some("apple"));
    assert_eq!(name1, Some("banana"));
}

// ---------------------------------------------------------------------------
// Scalar function: sign() property tests
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn prop_sign_positive(v in 1i64..=i64::MAX) {
        assert_eq!(apply_scalar_fn(&ScalarFnKind::Sign, v), 1);
    }

    #[test]
    fn prop_sign_negative(v in i64::MIN..=-1i64) {
        assert_eq!(apply_scalar_fn(&ScalarFnKind::Sign, v), -1);
    }
}

#[test]
fn sign_zero() {
    assert_eq!(apply_scalar_fn(&ScalarFnKind::Sign, 0), 0);
}

#[test]
fn sign_matches_signum() {
    for v in [-1000, -1, 0, 1, 1000, i64::MIN, i64::MAX] {
        assert_eq!(apply_scalar_fn(&ScalarFnKind::Sign, v), v.signum());
    }
}

// ---------------------------------------------------------------------------
// Duplicate output validation
// ---------------------------------------------------------------------------

#[test]
fn duplicate_output_name_rejected() {
    let prog = HclProgram {
        variables: Default::default(),
        resources: vec![],
        outputs: vec![
            HclOutput {
                name: "dup".into(),
                value: HclExpr::Literal(HclValue::String("a".into())),
            },
            HclOutput {
                name: "dup".into(),
                value: HclExpr::Literal(HclValue::String("b".into())),
            },
        ],
        modules: vec![],
        data_blocks: vec![],
    };
    let result = compile(prog, None, &[], &[]);
    match result {
        Err(CompileError::DuplicateOutput { name }) => {
            assert_eq!(name, "dup");
        }
        Err(other) => panic!("Expected DuplicateOutput error, got: {}", other),
        Ok(_) => panic!("Expected compilation to fail for duplicate outputs"),
    }
}

#[test]
fn unique_output_names_accepted() {
    let prog = HclProgram {
        variables: Default::default(),
        resources: vec![],
        outputs: vec![
            HclOutput {
                name: "out1".into(),
                value: HclExpr::Literal(HclValue::String("a".into())),
            },
            HclOutput {
                name: "out2".into(),
                value: HclExpr::Literal(HclValue::String("b".into())),
            },
        ],
        modules: vec![],
        data_blocks: vec![],
    };
    let result = compile(prog, None, &[], &[]);
    assert!(result.is_ok(), "Expected unique outputs to compile successfully");
}

// ---------------------------------------------------------------------------
// Scalar function: abs() property tests
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn prop_abs_nonnegative(v in any::<i64>()) {
        // abs() always returns a non-negative value (except i64::MIN which overflows).
        if v != i64::MIN {
            prop_assert!(apply_scalar_fn(&ScalarFnKind::Abs, v) >= 0);
        }
    }

    #[test]
    fn prop_abs_matches_stdlib(v in any::<i64>()) {
        if v != i64::MIN {
            prop_assert_eq!(apply_scalar_fn(&ScalarFnKind::Abs, v), v.abs());
        }
    }

    #[test]
    fn prop_neg_involution(v in any::<i64>()) {
        // neg(neg(x)) == x for all x (except overflow edge cases).
        if v != i64::MIN {
            let once = apply_scalar_fn(&ScalarFnKind::Neg, v);
            let twice = apply_scalar_fn(&ScalarFnKind::Neg, once);
            prop_assert_eq!(twice, v);
        }
    }
}

// ---------------------------------------------------------------------------
// Scalar function: float functions (floor, ceil, round, sqrt) property tests
// ---------------------------------------------------------------------------

/// Encode an f64 as i64 for use with apply_scalar_fn.
fn encode_f64(f: f64) -> i64 {
    let bits = f.to_bits() as i64;
    if bits == parsing::decl::NULL_SENTINEL {
        parsing::decl::NULL_SENTINEL + 1
    } else {
        bits
    }
}

/// Decode an i64 back to f64 (reverse of encode_f64).
fn decode_f64(v: i64) -> f64 {
    f64::from_bits(v as u64)
}

proptest! {
    #[test]
    fn prop_floor_matches_stdlib(f in proptest::num::f64::NORMAL) {
        let input = encode_f64(f);
        let result = decode_f64(apply_scalar_fn(&ScalarFnKind::Floor, input));
        prop_assert_eq!(result, f.floor(),
            "floor({}) expected {}, got {}", f, f.floor(), result);
    }

    #[test]
    fn prop_ceil_matches_stdlib(f in proptest::num::f64::NORMAL) {
        let input = encode_f64(f);
        let result = decode_f64(apply_scalar_fn(&ScalarFnKind::Ceil, input));
        prop_assert_eq!(result, f.ceil(),
            "ceil({}) expected {}, got {}", f, f.ceil(), result);
    }

    #[test]
    fn prop_round_matches_stdlib(f in proptest::num::f64::NORMAL) {
        let input = encode_f64(f);
        let result = decode_f64(apply_scalar_fn(&ScalarFnKind::Round, input));
        prop_assert_eq!(result, f.round(),
            "round({}) expected {}, got {}", f, f.round(), result);
    }

    #[test]
    fn prop_sqrt_nonneg_input(f in 0.0f64..1e15) {
        let input = encode_f64(f);
        let result = decode_f64(apply_scalar_fn(&ScalarFnKind::Sqrt, input));
        prop_assert_eq!(result, f.sqrt(),
            "sqrt({}) expected {}, got {}", f, f.sqrt(), result);
    }

    #[test]
    fn prop_floor_le_original(f in proptest::num::f64::NORMAL) {
        let input = encode_f64(f);
        let result = decode_f64(apply_scalar_fn(&ScalarFnKind::Floor, input));
        prop_assert!(result <= f,
            "floor({}) = {} should be <= original", f, result);
    }

    #[test]
    fn prop_ceil_ge_original(f in proptest::num::f64::NORMAL) {
        let input = encode_f64(f);
        let result = decode_f64(apply_scalar_fn(&ScalarFnKind::Ceil, input));
        prop_assert!(result >= f,
            "ceil({}) = {} should be >= original", f, result);
    }
}

#[test]
fn floor_known_values() {
    for (f, expected) in [(3.7, 3.0), (3.0, 3.0), (-1.5, -2.0), (0.0, 0.0)] {
        let result = decode_f64(apply_scalar_fn(&ScalarFnKind::Floor, encode_f64(f)));
        assert_eq!(result, expected, "floor({}) expected {}, got {}", f, expected, result);
    }
}

#[test]
fn ceil_known_values() {
    for (f, expected) in [(3.2, 4.0), (3.0, 3.0), (-1.5, -1.0), (0.0, 0.0)] {
        let result = decode_f64(apply_scalar_fn(&ScalarFnKind::Ceil, encode_f64(f)));
        assert_eq!(result, expected, "ceil({}) expected {}, got {}", f, expected, result);
    }
}

#[test]
fn round_known_values() {
    for (f, expected) in [(3.5, 4.0), (3.4, 3.0), (-1.5, -2.0), (0.0, 0.0)] {
        let result = decode_f64(apply_scalar_fn(&ScalarFnKind::Round, encode_f64(f)));
        assert_eq!(result, expected, "round({}) expected {}, got {}", f, expected, result);
    }
}

#[test]
fn sqrt_known_values() {
    for (f, expected) in [(9.0, 3.0), (4.0, 2.0), (1.0, 1.0), (0.0, 0.0)] {
        let result = decode_f64(apply_scalar_fn(&ScalarFnKind::Sqrt, encode_f64(f)));
        assert_eq!(result, expected, "sqrt({}) expected {}, got {}", f, expected, result);
    }
}

#[test]
fn sqrt_negative_is_nan() {
    let result = decode_f64(apply_scalar_fn(&ScalarFnKind::Sqrt, encode_f64(-1.0)));
    assert!(result.is_nan(), "sqrt(-1) should be NaN, got {}", result);
}

// ---------------------------------------------------------------------------
// ScalarFnKind::is_float_function tests
// ---------------------------------------------------------------------------

#[test]
fn is_float_function_correct() {
    assert!(!ScalarFnKind::Neg.is_float_function());
    assert!(!ScalarFnKind::Abs.is_float_function());
    assert!(!ScalarFnKind::Sign.is_float_function());
    assert!(ScalarFnKind::Floor.is_float_function());
    assert!(ScalarFnKind::Ceil.is_float_function());
    assert!(ScalarFnKind::Round.is_float_function());
    assert!(ScalarFnKind::Sqrt.is_float_function());
}
