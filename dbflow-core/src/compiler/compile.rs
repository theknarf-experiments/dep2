use std::collections::{HashMap, HashSet};
use std::path::Path;

use indexmap::IndexMap;
use parsing::decl::{Attribute, DataType, RelDecl};
use parsing::head::{Head, HeadArg};
use parsing::parser::Program;
use parsing::rule::{Atom, AtomArg, Const, FLRule, Predicate};

use super::error::CompileError;
use super::rule::{make_rel_decl, make_rule};
use super::types::{
    convert_data_type, data_value_to_i64, value_to_i64, CompileResult, FetchedDataBlock,
    OutputInfo, ScalarFnKind, StreamingDataBlock, StreamingFnEdb, StringTable,
};
use crate::hcl_types::{HclExpr, HclOutput, HclProgram, HclValue};
use crate::module_loader::expand_modules;
use crate::reference::{analyze_dependencies, resolve_variables, BlockKind};

/// Compile an `HclProgram` into a FlowLog `Program`.
///
/// `base_path` is the directory used to resolve relative module `source` paths.
/// Pass `None` if module blocks are not expected (e.g., in unit tests).
///
/// `data_blocks` contains pre-fetched data from `data` blocks in the HCL source.
/// `streaming_data_blocks` contains schema info for streaming data blocks (no rows yet).
pub fn compile(
    mut hcl_program: HclProgram,
    base_path: Option<&Path>,
    data_blocks: &[FetchedDataBlock],
    streaming_data_blocks: &[StreamingDataBlock],
) -> Result<CompileResult, CompileError> {
    // Expand module blocks (if any).
    if !hcl_program.modules.is_empty() {
        let bp = base_path.ok_or(CompileError::MissingBasePath)?;
        expand_modules(&mut hcl_program, bp).map_err(CompileError::Module)?;
    }

    // Resolve variable references.
    resolve_variables(&mut hcl_program);

    // Analyze dependencies to classify blocks.
    let analysis = analyze_dependencies(&hcl_program);

    let mut string_table = StringTable::default();
    let mut edbs = Vec::new();
    let mut idbs = Vec::new();
    let mut rules = Vec::new();
    let mut edb_facts: HashMap<String, Vec<Vec<i64>>> = HashMap::new();
    let mut streaming_fn_edbs: Vec<StreamingFnEdb> = Vec::new();

    // Process data blocks → EDB relations.
    let mut data_schemas: HashMap<(String, String), Vec<String>> = HashMap::new();
    for data in data_blocks {
        let rel_name = format!("_data_{}_{}", data.provider_type, data.label);

        let col_names: Vec<String> = data.schema.columns.iter().map(|c| c.name.clone()).collect();

        // Declare EDB relation (no label column — unlike resources).
        let attributes: Vec<Attribute> = data
            .schema
            .columns
            .iter()
            .map(|c| Attribute::new(&c.name, convert_data_type(&c.data_type)))
            .collect();
        let decl = RelDecl::new(&rel_name, attributes, None);
        edbs.push(decl);

        // Convert rows to fact tuples.
        let mut facts = Vec::new();
        for row in &data.rows {
            let mut tuple = Vec::new();
            for val in row {
                tuple.push(data_value_to_i64(val, &mut string_table));
            }
            facts.push(tuple);
        }
        edb_facts.insert(rel_name, facts);

        data_schemas.insert((data.provider_type.clone(), data.label.clone()), col_names);
    }

    // Process streaming data blocks → EDB relation declarations only (no facts).
    let mut streaming_edbs = Vec::new();
    for sdb in streaming_data_blocks {
        let rel_name = format!("_data_{}_{}", sdb.provider_type, sdb.label);

        let col_names: Vec<String> = sdb.schema.columns.iter().map(|c| c.name.clone()).collect();

        // Declare EDB relation (same as batch, but no facts).
        let attributes: Vec<Attribute> = sdb
            .schema
            .columns
            .iter()
            .map(|c| Attribute::new(&c.name, convert_data_type(&c.data_type)))
            .collect();
        let decl = RelDecl::new(&rel_name, attributes, None);
        edbs.push(decl);

        // Write an empty facts file so FlowLog doesn't panic on missing file.
        edb_facts.insert(rel_name.clone(), Vec::new());

        streaming_edbs.push(rel_name);

        data_schemas.insert((sdb.provider_type.clone(), sdb.label.clone()), col_names);
    }

    // Build a lookup from (type_name, label) to the resource for reference resolution.
    let resource_map: HashMap<(&str, &str), &crate::hcl_types::HclResource> = hcl_program
        .resources
        .iter()
        .map(|r| ((r.type_name.as_str(), r.label.as_str()), r))
        .collect();

    // Build schema map: type_name → ordered attribute names (across all blocks of that type).
    // All blocks of the same type must share a schema.
    // Negated attributes and Comparison attributes are excluded — they are filters, not values.
    let mut schema_map: IndexMap<String, Vec<String>> = IndexMap::new();
    for resource in &hcl_program.resources {
        let entry = schema_map.entry(resource.type_name.clone()).or_default();
        for (attr_name, expr) in &resource.attributes {
            if matches!(
                expr,
                HclExpr::NegatedReference(_) | HclExpr::Comparison { .. }
            ) {
                continue; // negated refs and comparisons don't contribute schema columns
            }
            if !entry.contains(attr_name) {
                entry.push(attr_name.clone());
            }
        }
    }

    // Validate: EDB blocks must not contain comparison attributes (which would be
    // silently excluded from the schema). Reject them early with a clear error.
    for resource in &hcl_program.resources {
        let block_id = (resource.type_name.clone(), resource.label.clone());
        if let Some(BlockKind::Edb) = analysis.block_kinds.get(&block_id) {
            for (attr_name, expr) in &resource.attributes {
                if matches!(expr, HclExpr::Comparison { .. }) {
                    return Err(CompileError::InvalidEdbExpr {
                        type_name: resource.type_name.clone(),
                        label: resource.label.clone(),
                        detail: format!(
                            "cannot use comparison in attribute '{}' (comparisons are only valid in IDB rules)",
                            attr_name
                        ),
                    });
                }
            }
        }
    }

    // Track which type_names have already been declared.
    let mut declared_edb: HashSet<String> = HashSet::new();
    let mut declared_idb: HashSet<String> = HashSet::new();

    // Process blocks in topological order.
    for block_id in &analysis.topo_order {
        let (type_name, label) = block_id;
        let resource = resource_map
            .get(&(type_name.as_str(), label.as_str()))
            .ok_or_else(|| {
                CompileError::Internal(format!("block ({}, {}) not found", type_name, label))
            })?;
        let kind = analysis.block_kinds.get(block_id).ok_or_else(|| {
            CompileError::Internal(format!(
                "block ({}, {}) not classified",
                type_name, label
            ))
        })?;

        let attr_names = schema_map
            .get(type_name)
            .ok_or_else(|| {
                CompileError::Internal(format!("no schema for type {}", type_name))
            })?;

        match kind {
            BlockKind::Edb => {
                // Declare the EDB relation (once per type).
                if declared_edb.insert(type_name.clone()) {
                    let decl = make_rel_decl(type_name, attr_names, &resource.attributes);
                    edbs.push(decl);
                }

                // Generate a fact tuple.
                let mut tuple = Vec::new();
                // First field is the label (interned).
                tuple.push(string_table.intern(label));
                // Then each attribute in schema order.
                for attr_name in attr_names {
                    let val = resource.attributes.get(attr_name).ok_or_else(|| {
                        CompileError::MissingAttribute {
                            type_name: type_name.clone(),
                            label: label.clone(),
                            attribute: attr_name.clone(),
                        }
                    })?;
                    match val {
                        HclExpr::Literal(v) => {
                            tuple.push(value_to_i64(v, &mut string_table));
                        }
                        HclExpr::Aggregate { .. } => {
                            return Err(CompileError::InvalidEdbExpr {
                                type_name: type_name.clone(),
                                label: label.clone(),
                                detail: "cannot use aggregate functions".to_string(),
                            });
                        }
                        HclExpr::ArithmeticOp { .. } => {
                            return Err(CompileError::InvalidEdbExpr {
                                type_name: type_name.clone(),
                                label: label.clone(),
                                detail: "cannot use arithmetic expressions".to_string(),
                            });
                        }
                        _ => {
                            return Err(CompileError::InvalidEdbExpr {
                                type_name: type_name.clone(),
                                label: label.clone(),
                                detail: format!("has non-literal value for '{}'", attr_name),
                            });
                        }
                    }
                }

                edb_facts.entry(type_name.clone()).or_default().push(tuple);
            }
            BlockKind::Idb => {
                // Declare the IDB relation (once per type).
                if declared_idb.insert(type_name.clone()) {
                    let decl = make_rel_decl(type_name, attr_names, &resource.attributes);
                    idbs.push(decl);
                }

                // Generate a rule: head :- body, with a label-binding EDB.
                let (rule, label_edb_name, label_edb_decl, label_fact, extra) = make_rule(
                    resource,
                    attr_names,
                    &resource_map,
                    &schema_map,
                    &data_schemas,
                    &mut string_table,
                )?;
                rules.push(rule);
                edbs.push(label_edb_decl);
                edb_facts
                    .entry(label_edb_name)
                    .or_default()
                    .push(label_fact);

                // Multi-aggregate decomposition or scalar function expansion.
                if let Some(extra_decls) = extra {
                    idbs.extend(extra_decls.idbs);
                    rules.extend(extra_decls.rules);

                    // Handle function EDB lookup tables.
                    for (fn_decl, fn_info) in extra_decls.fn_edbs {
                        edbs.push(fn_decl);

                        // Check if the source is a streaming EDB.
                        let is_streaming_source =
                            streaming_edbs.contains(&fn_info.source_data_edb);

                        if is_streaming_source {
                            // For streaming sources, add the fn EDB as a streaming EDB
                            // and record metadata for the engine's encoding thread.
                            streaming_edbs.push(fn_info.edb_name.clone());
                            edb_facts.insert(fn_info.edb_name.clone(), Vec::new());
                            streaming_fn_edbs.push(super::types::StreamingFnEdb {
                                fn_edb_name: fn_info.edb_name,
                                source_edb_name: fn_info.source_data_edb,
                                input_col_idx: fn_info.input_col_idx,
                                function: fn_info.function,
                            });
                        } else {
                            // For batch sources, precompute function values from existing facts.
                            let source_facts = edb_facts
                                .get(&fn_info.source_data_edb)
                                .cloned()
                                .unwrap_or_default();
                            let mut fn_facts = Vec::new();
                            for fact_row in &source_facts {
                                if fn_info.input_col_idx < fact_row.len() {
                                    let input_val = fact_row[fn_info.input_col_idx];
                                    let output_val =
                                        apply_scalar_fn(&fn_info.function, input_val);
                                    fn_facts.push(vec![input_val, output_val]);
                                }
                            }
                            edb_facts.insert(fn_info.edb_name, fn_facts);
                        }
                    }
                }
            }
        }
    }

    // Compile output blocks into IDB relations.
    let mut output_infos = Vec::new();
    for output in &hcl_program.outputs {
        let (output_info, new_decl, new_rules, new_facts) =
            compile_output(output, &schema_map, &data_schemas, &mut string_table)?;
        if !new_facts.is_empty() {
            // Literal outputs become EDB facts.
            if let Some(decl) = new_decl {
                edbs.push(decl);
            }
        } else if let Some(decl) = new_decl {
            idbs.push(decl);
        }
        rules.extend(new_rules);
        for (rel, facts) in new_facts {
            edb_facts.entry(rel).or_default().extend(facts);
        }
        output_infos.push(output_info);
    }

    let program = Program::new(edbs, idbs, rules);

    Ok(CompileResult {
        program,
        string_table,
        analysis,
        edb_facts,
        outputs: output_infos,
        streaming_edbs,
        streaming_fn_edbs,
    })
}

/// Compile a single output block into an IDB relation.
///
/// Returns `(OutputInfo, optional RelDecl, Vec<FLRule>, HashMap of EDB facts)`.
fn compile_output(
    output: &HclOutput,
    schema_map: &IndexMap<String, Vec<String>>,
    data_schemas: &HashMap<(String, String), Vec<String>>,
    string_table: &mut StringTable,
) -> Result<
    (
        OutputInfo,
        Option<RelDecl>,
        Vec<FLRule>,
        HashMap<String, Vec<Vec<i64>>>,
    ),
    CompileError,
> {
    let rel_name = format!("hcl_output_{}", output.name);

    match &output.value {
        HclExpr::Reference(r) => {
            // Output references a field from a resource block.
            // Generate: hcl_output_{name}(Value) :- {block_type}("{block_label}", ..., Value, ...).
            let ref_schema = schema_map.get(&r.block_type).ok_or_else(|| {
                CompileError::UnknownReference {
                    context: format!("output '{}'", output.name),
                    reference: format!("type '{}'", r.block_type),
                }
            })?;

            // Find the position of the referenced field in the schema.
            let field_idx = ref_schema
                .iter()
                .position(|a| a == &r.field)
                .ok_or_else(|| CompileError::UnknownReference {
                    context: format!("output '{}'", output.name),
                    reference: format!("field '{}.{}.{}'", r.block_type, r.block_label, r.field),
                })?;

            // Infer data type from the field name position — we use String as default
            // since we can't easily access the sample resource here. The referenced
            // relation's declaration already has the type info, so this is for OutputInfo only.
            let data_type = DataType::String;

            // Declare the output IDB: hcl_output_{name}(value: <type>)
            let decl = RelDecl::new(
                &rel_name,
                vec![Attribute::new("value", data_type.clone())],
                None,
            );

            // Build the rule: hcl_output_{name}(Value) :- block_type("label", ..., Value, ...).
            let var_name = "Value";
            let head = Head::new(rel_name.clone(), vec![HeadArg::Var(var_name.to_string())]);

            // Build body atom: block_type("label", _, ..., Value, _, ...)
            let mut atom_args = Vec::new();
            // Label position (first argument).
            let label_id = string_table.intern(&r.block_label);
            atom_args.push(AtomArg::Const(Const::Integer(label_id)));
            // Schema attributes — put the variable at the matching field, placeholder elsewhere.
            for (i, _attr_name) in ref_schema.iter().enumerate() {
                if i == field_idx {
                    atom_args.push(AtomArg::Var(var_name.to_string()));
                } else {
                    atom_args.push(AtomArg::Placeholder);
                }
            }

            let atom = Atom::from_str(&r.block_type, atom_args);
            let rule = FLRule::new(head, vec![Predicate::AtomPredicate(atom)], false, false);

            let output_info = OutputInfo {
                name: output.name.clone(),
                relation_name: rel_name,
                column_types: vec![data_type],
            };

            Ok((output_info, Some(decl), vec![rule], HashMap::new()))
        }
        HclExpr::Literal(val) => {
            // Literal output — generate an EDB fact for hcl_output_{name}.
            let data_type = match val {
                HclValue::Integer(_) => DataType::Integer,
                HclValue::Float(_) => DataType::Float,
                HclValue::String(_) => DataType::String,
                HclValue::Bool(_) => DataType::Integer,
            };

            let decl = RelDecl::new(
                &rel_name,
                vec![Attribute::new("value", data_type.clone())],
                None,
            );

            let fact_val = value_to_i64(val, string_table);
            let mut facts = HashMap::new();
            facts.insert(rel_name.clone(), vec![vec![fact_val]]);

            let output_info = OutputInfo {
                name: output.name.clone(),
                relation_name: rel_name.clone(),
                column_types: vec![data_type],
            };

            // Literal outputs become EDB facts. The caller adds the decl to edbs
            // when facts are non-empty.
            Ok((output_info, Some(decl), vec![], facts))
        }
        HclExpr::NegatedReference(_) => Err(CompileError::InvalidExprContext {
            context: format!("output '{}'", output.name),
            expr_kind: "a negated reference".to_string(),
        }),
        HclExpr::DataReference(dr) => {
            // Output references a data block field.
            let data_key = (dr.provider_type.clone(), dr.label.clone());
            let data_rel_name = format!("_data_{}_{}", dr.provider_type, dr.label);
            let data_col_names = data_schemas.get(&data_key).ok_or_else(|| {
                CompileError::UnknownReference {
                    context: format!("output '{}'", output.name),
                    reference: format!("data block data.{}.{}", dr.provider_type, dr.label),
                }
            })?;

            let field_idx = data_col_names
                .iter()
                .position(|c| c == &dr.field)
                .ok_or_else(|| CompileError::UnknownReference {
                    context: format!("output '{}'", output.name),
                    reference: format!(
                        "field 'data.{}.{}.{}'",
                        dr.provider_type, dr.label, dr.field
                    ),
                })?;

            let data_type = DataType::String;

            let decl = RelDecl::new(
                &rel_name,
                vec![Attribute::new("value", data_type.clone())],
                None,
            );

            let var_name = "Value";
            let head = Head::new(rel_name.clone(), vec![HeadArg::Var(var_name.to_string())]);

            // Build body atom: _data_{type}_{label}(_, ..., Value, ...)
            // Data blocks have no label column — fields start at index 0.
            let mut atom_args = Vec::new();
            for (i, _) in data_col_names.iter().enumerate() {
                if i == field_idx {
                    atom_args.push(AtomArg::Var(var_name.to_string()));
                } else {
                    atom_args.push(AtomArg::Placeholder);
                }
            }

            let atom = Atom::from_str(&data_rel_name, atom_args);
            let rule = FLRule::new(head, vec![Predicate::AtomPredicate(atom)], false, false);

            let output_info = OutputInfo {
                name: output.name.clone(),
                relation_name: rel_name,
                column_types: vec![data_type],
            };

            Ok((output_info, Some(decl), vec![rule], HashMap::new()))
        }
        HclExpr::VarRef(name) => Err(CompileError::UnresolvedVariable {
            context: format!("output '{}'", output.name),
            var_name: name.clone(),
        }),
        HclExpr::Comparison { .. } => Err(CompileError::InvalidExprContext {
            context: format!("output '{}'", output.name),
            expr_kind: "a comparison expression".to_string(),
        }),
        HclExpr::Aggregate { .. } => Err(CompileError::InvalidExprContext {
            context: format!("output '{}'", output.name),
            expr_kind: "an aggregate expression".to_string(),
        }),
        HclExpr::ArithmeticOp { .. } => Err(CompileError::InvalidExprContext {
            context: format!("output '{}'", output.name),
            expr_kind: "an arithmetic expression".to_string(),
        }),
        HclExpr::FunctionCall { .. } => Err(CompileError::InvalidExprContext {
            context: format!("output '{}'", output.name),
            expr_kind: "a function call".to_string(),
        }),
    }
}

/// Apply a scalar function to an i64 value.
pub fn apply_scalar_fn(kind: &ScalarFnKind, input: i64) -> i64 {
    match kind {
        ScalarFnKind::Neg => -input,
        ScalarFnKind::Abs => input.abs(),
    }
}
