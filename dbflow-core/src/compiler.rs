use std::collections::{HashMap, HashSet};
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use indexmap::IndexMap;
use parsing::decl::{Attribute, DataType, RelDecl};
use parsing::head::{Head, HeadArg};
use parsing::parser::Program;
use parsing::rule::{Atom, AtomArg, Const, FLRule, Predicate};

use crate::hcl_types::{HclExpr, HclOutput, HclProgram, HclResource, HclValue};
use crate::module_loader::expand_modules;
use crate::reference::{analyze_dependencies, resolve_variables, BlockKind, DependencyAnalysis};

/// Bidirectional string interning table.
/// Maps string values to unique i32 identifiers for FlowLog execution.
#[derive(Debug, Default, Clone)]
pub struct StringTable {
    str_to_id: HashMap<String, i32>,
    id_to_str: Vec<String>,
}

impl StringTable {
    pub fn intern(&mut self, s: &str) -> i32 {
        if let Some(&id) = self.str_to_id.get(s) {
            return id;
        }
        let id = self.id_to_str.len() as i32;
        self.id_to_str.push(s.to_string());
        self.str_to_id.insert(s.to_string(), id);
        id
    }

    pub fn decode(&self, id: i32) -> Option<&str> {
        self.id_to_str.get(id as usize).map(|s| s.as_str())
    }
}

/// Thread-safe wrapper around `StringTable` for runtime use (e.g., streaming).
pub struct RuntimeStringTable {
    inner: Mutex<StringTable>,
}

impl RuntimeStringTable {
    pub fn from(st: StringTable) -> Self {
        Self {
            inner: Mutex::new(st),
        }
    }

    pub fn intern(&self, s: &str) -> i32 {
        self.inner.lock().unwrap().intern(s)
    }

    pub fn decode(&self, id: i32) -> Option<String> {
        self.inner.lock().unwrap().decode(id).map(|s| s.to_string())
    }
}

/// Metadata about a compiled output block.
pub struct OutputInfo {
    /// User-visible name (e.g., "all_monitors").
    pub name: String,
    /// FlowLog relation name (e.g., "hcl_output_all_monitors").
    pub relation_name: String,
    /// Column types for decoding output values.
    pub column_types: Vec<DataType>,
}

/// Result of compiling an HCL program.
pub struct CompileResult {
    pub program: Program,
    pub string_table: StringTable,
    pub analysis: DependencyAnalysis,
    /// For each EDB relation name, the list of fact tuples (as i32 vectors).
    pub edb_facts: HashMap<String, Vec<Vec<i32>>>,
    /// Metadata about output blocks for post-execution display.
    pub outputs: Vec<OutputInfo>,
    /// Names of EDB relations that will be populated at runtime via streaming.
    pub streaming_edbs: Vec<String>,
}

/// Fetched data from a data block, ready for compilation into EDB facts.
pub struct FetchedDataBlock {
    pub provider_type: String,
    pub label: String,
    pub schema: dbflow_plugin::DataSchema,
    pub rows: Vec<Vec<dbflow_plugin::DataValue>>,
}

/// A streaming data block: schema is known, but rows arrive at runtime.
pub struct StreamingDataBlock {
    pub provider_type: String,
    pub label: String,
    pub schema: dbflow_plugin::DataSchema,
}

/// Convert a plugin `DataType` to a FlowLog `DataType`.
fn convert_data_type(dt: &dbflow_plugin::DataType) -> DataType {
    match dt {
        dbflow_plugin::DataType::String => DataType::String,
        dbflow_plugin::DataType::Integer => DataType::Integer,
    }
}

/// Convert a plugin `DataValue` to an i32 for fact encoding.
fn data_value_to_i32(val: &dbflow_plugin::DataValue, st: &mut StringTable) -> Result<i32, String> {
    match val {
        dbflow_plugin::DataValue::String(s) => Ok(st.intern(s)),
        dbflow_plugin::DataValue::Integer(i) => {
            if *i < i32::MIN as i64 || *i > i32::MAX as i64 {
                Err(format!("integer value {} out of i32 range", i))
            } else {
                Ok(*i as i32)
            }
        }
        dbflow_plugin::DataValue::Bool(b) => Ok(if *b { 1 } else { 0 }),
        dbflow_plugin::DataValue::Null => Ok(st.intern("__null__")),
    }
}

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
) -> Result<CompileResult, String> {
    // Expand module blocks (if any).
    if !hcl_program.modules.is_empty() {
        let bp = base_path.ok_or_else(|| {
            "module blocks require a base path for source resolution".to_string()
        })?;
        expand_modules(&mut hcl_program, bp)?;
    }

    // Resolve variable references.
    resolve_variables(&mut hcl_program);

    // Analyze dependencies to classify blocks.
    let analysis = analyze_dependencies(&hcl_program);

    let mut string_table = StringTable::default();
    let mut edbs = Vec::new();
    let mut idbs = Vec::new();
    let mut rules = Vec::new();
    let mut edb_facts: HashMap<String, Vec<Vec<i32>>> = HashMap::new();

    // Process data blocks → EDB relations.
    let mut data_schemas: HashMap<(String, String), Vec<String>> = HashMap::new();
    for data in data_blocks {
        let rel_name = format!("_data_{}_{}", data.provider_type, data.label);

        let col_names: Vec<String> = data
            .schema
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect();

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
                tuple.push(data_value_to_i32(val, &mut string_table)?);
            }
            facts.push(tuple);
        }
        edb_facts.insert(rel_name, facts);

        data_schemas.insert(
            (data.provider_type.clone(), data.label.clone()),
            col_names,
        );
    }

    // Process streaming data blocks → EDB relation declarations only (no facts).
    let mut streaming_edbs = Vec::new();
    for sdb in streaming_data_blocks {
        let rel_name = format!("_data_{}_{}", sdb.provider_type, sdb.label);

        let col_names: Vec<String> = sdb
            .schema
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect();

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

        data_schemas.insert(
            (sdb.provider_type.clone(), sdb.label.clone()),
            col_names,
        );
    }

    // Build a lookup from (type_name, label) to the resource for reference resolution.
    let resource_map: HashMap<(&str, &str), &HclResource> = hcl_program
        .resources
        .iter()
        .map(|r| ((r.type_name.as_str(), r.label.as_str()), r))
        .collect();

    // Build schema map: type_name → ordered attribute names (across all blocks of that type).
    // All blocks of the same type must share a schema.
    // Negated attributes are excluded — they are filters, not values.
    let mut schema_map: IndexMap<String, Vec<String>> = IndexMap::new();
    for resource in &hcl_program.resources {
        let entry = schema_map
            .entry(resource.type_name.clone())
            .or_default();
        for (attr_name, expr) in &resource.attributes {
            if matches!(expr, HclExpr::NegatedReference(_)) {
                continue; // negated refs don't contribute schema columns
            }
            if !entry.contains(attr_name) {
                entry.push(attr_name.clone());
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
            .ok_or_else(|| format!("internal error: block ({}, {}) not found", type_name, label))?;
        let kind = analysis
            .block_kinds
            .get(block_id)
            .ok_or_else(|| format!("internal error: block ({}, {}) not classified", type_name, label))?;

        let attr_names = schema_map
            .get(type_name)
            .ok_or_else(|| format!("internal error: no schema for type {}", type_name))?;

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
                    let val = resource
                        .attributes
                        .get(attr_name)
                        .ok_or_else(|| {
                            format!(
                                "resource {}.{} missing attribute '{}'",
                                type_name, label, attr_name
                            )
                        })?;
                    match val {
                        HclExpr::Literal(v) => {
                            tuple.push(value_to_i32(v, &mut string_table));
                        }
                        _ => {
                            return Err(format!(
                                "EDB block {}.{} has non-literal value for '{}'",
                                type_name, label, attr_name
                            ));
                        }
                    }
                }

                edb_facts
                    .entry(type_name.clone())
                    .or_default()
                    .push(tuple);
            }
            BlockKind::Idb => {
                // Declare the IDB relation (once per type).
                if declared_idb.insert(type_name.clone()) {
                    let decl = make_rel_decl(type_name, attr_names, &resource.attributes);
                    idbs.push(decl);
                }

                // Generate a rule: head :- body, with a label-binding EDB.
                let (rule, label_edb_name, label_edb_decl, label_fact) = make_rule(
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
            }
        }
    }

    // Compile output blocks into IDB relations.
    let mut output_infos = Vec::new();
    for output in &hcl_program.outputs {
        let (output_info, new_decl, new_rules, new_facts) = compile_output(
            output,
            &schema_map,
            &data_schemas,
            &mut string_table,
        )?;
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
) -> Result<(OutputInfo, Option<RelDecl>, Vec<FLRule>, HashMap<String, Vec<Vec<i32>>>), String> {
    let rel_name = format!("hcl_output_{}", output.name);

    match &output.value {
        HclExpr::Reference(r) => {
            // Output references a field from a resource block.
            // Generate: hcl_output_{name}(Value) :- {block_type}("{block_label}", ..., Value, ...).
            let ref_schema = schema_map.get(&r.block_type).ok_or_else(|| {
                format!(
                    "output '{}' references unknown type '{}'",
                    output.name, r.block_type
                )
            })?;

            // Find the position of the referenced field in the schema.
            let field_idx = ref_schema.iter().position(|a| a == &r.field).ok_or_else(|| {
                format!(
                    "output '{}' references unknown field '{}.{}.{}'",
                    output.name, r.block_type, r.block_label, r.field
                )
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
                HclValue::String(_) => DataType::String,
                HclValue::Bool(_) => DataType::Integer,
            };

            let decl = RelDecl::new(
                &rel_name,
                vec![Attribute::new("value", data_type.clone())],
                None,
            );

            let fact_val = value_to_i32(val, string_table);
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
        HclExpr::NegatedReference(_) => {
            Err(format!(
                "output '{}' cannot use a negated reference as its value",
                output.name
            ))
        }
        HclExpr::DataReference(dr) => {
            // Output references a data block field.
            let data_key = (dr.provider_type.clone(), dr.label.clone());
            let data_rel_name = format!("_data_{}_{}", dr.provider_type, dr.label);
            let data_col_names = data_schemas.get(&data_key).ok_or_else(|| {
                format!(
                    "output '{}' references unknown data block data.{}.{}",
                    output.name, dr.provider_type, dr.label
                )
            })?;

            let field_idx = data_col_names.iter().position(|c| c == &dr.field).ok_or_else(|| {
                format!(
                    "output '{}' references unknown field 'data.{}.{}.{}'",
                    output.name, dr.provider_type, dr.label, dr.field
                )
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
        HclExpr::VarRef(name) => {
            Err(format!(
                "output '{}' has unresolved variable reference 'var.{}'",
                output.name, name
            ))
        }
    }
}

/// Create a `RelDecl` for a resource type.
/// Schema: type_name(label: string, attr1: type, attr2: type, ...)
fn make_rel_decl(
    type_name: &str,
    attr_names: &[String],
    sample_attrs: &IndexMap<String, HclExpr>,
) -> RelDecl {
    let mut attributes = Vec::new();
    // First attribute is always the label.
    attributes.push(Attribute::new("label", DataType::String));
    // Then each attribute in order.
    for attr_name in attr_names {
        let data_type = if let Some(expr) = sample_attrs.get(attr_name) {
            infer_data_type(expr)
        } else {
            DataType::String // default
        };
        attributes.push(Attribute::new(attr_name, data_type));
    }
    RelDecl::new(type_name, attributes, None)
}

/// Infer a FlowLog `DataType` from an HCL expression.
fn infer_data_type(expr: &HclExpr) -> DataType {
    match expr {
        HclExpr::Literal(HclValue::Integer(_)) => DataType::Integer,
        HclExpr::Literal(HclValue::String(_)) => DataType::String,
        HclExpr::Literal(HclValue::Bool(_)) => DataType::Integer, // bools as 0/1
        HclExpr::Reference(_) | HclExpr::NegatedReference(_) | HclExpr::VarRef(_)
        | HclExpr::DataReference(_) => DataType::String,
    }
}

/// Convert an `HclValue` to an i32 for fact encoding.
fn value_to_i32(val: &HclValue, st: &mut StringTable) -> i32 {
    match val {
        HclValue::Integer(i) => *i,
        HclValue::String(s) => st.intern(s),
        HclValue::Bool(b) => if *b { 1 } else { 0 },
    }
}

/// Generate an `FLRule` for an IDB (derived) resource block.
///
/// For a block like:
/// ```hcl
/// resource "monitor" "m1" {
///   target_ip = server.web1.ip
/// }
/// ```
///
/// Produces a rule like:
/// ```text
/// monitor("m1", TargetIp) :- server("web1", TargetIp, _), _hcl_lbl_monitor_m1(HclLabel).
/// ```
///
/// Returns `(rule, label_edb_name, label_edb_decl, label_fact)`.
fn make_rule(
    resource: &HclResource,
    attr_names: &[String],
    _resource_map: &HashMap<(&str, &str), &HclResource>,
    schema_map: &IndexMap<String, Vec<String>>,
    data_schemas: &HashMap<(String, String), Vec<String>>,
    string_table: &mut StringTable,
) -> Result<(FLRule, String, RelDecl, Vec<i32>), String> {
    // Build head arguments.
    let mut head_args = Vec::new();
    // First argument is the label, bound via a helper EDB in the body.
    let label_id = string_table.intern(&resource.label);
    head_args.push(HeadArg::Var("HclLabel".to_string()));

    // Track which variables we introduce (for the positive body atoms).
    // Each binding: (variable_name, source_type, source_label, source_field)
    let mut var_bindings: Vec<(String, String, String, String)> = Vec::new();

    // Track data reference bindings: (variable_name, provider_type, label, field)
    let mut data_bindings: Vec<(String, String, String, String)> = Vec::new();

    // Track negated references separately: (block_type, block_label, field)
    let mut neg_bindings: Vec<(String, String, String)> = Vec::new();

    for attr_name in attr_names {
        let expr = resource.attributes.get(attr_name).ok_or_else(|| {
            format!(
                "resource {}.{} missing attribute '{}'",
                resource.type_name, resource.label, attr_name
            )
        })?;

        match expr {
            HclExpr::Literal(_val) => {
                let var_name = to_datalog_var(attr_name);
                head_args.push(HeadArg::Var(var_name.clone()));
            }
            HclExpr::Reference(r) => {
                let var_name = to_datalog_var(attr_name);
                head_args.push(HeadArg::Var(var_name.clone()));
                var_bindings.push((
                    var_name,
                    r.block_type.clone(),
                    r.block_label.clone(),
                    r.field.clone(),
                ));
            }
            HclExpr::DataReference(dr) => {
                let var_name = to_datalog_var(attr_name);
                head_args.push(HeadArg::Var(var_name.clone()));
                data_bindings.push((
                    var_name,
                    dr.provider_type.clone(),
                    dr.label.clone(),
                    dr.field.clone(),
                ));
            }
            HclExpr::NegatedReference(_) => {
                // Negated refs don't contribute head args — they're filters only.
                // Handled after positive body atoms are built.
            }
            HclExpr::VarRef(_) => {
                return Err(format!(
                    "unresolved variable reference in {}.{}.{}",
                    resource.type_name, resource.label, attr_name
                ));
            }
        }
    }

    // Collect negated bindings from ALL attributes (not just schema attrs).
    for (_, expr) in &resource.attributes {
        if let HclExpr::NegatedReference(r) = expr {
            neg_bindings.push((
                r.block_type.clone(),
                r.block_label.clone(),
                r.field.clone(),
            ));
        }
    }

    // Build the head.
    let head = Head::new(resource.type_name.clone(), head_args);

    // Build body atoms — one per referenced block.
    // Group references by (block_type, block_label) to avoid duplicate atoms.
    let mut body_atoms_map: IndexMap<(String, String), Vec<(String, String)>> = IndexMap::new();
    for (var_name, block_type, block_label, field) in &var_bindings {
        body_atoms_map
            .entry((block_type.clone(), block_label.clone()))
            .or_default()
            .push((var_name.clone(), field.clone()));
    }

    let mut body_predicates = Vec::new();

    for ((block_type, block_label), field_vars) in &body_atoms_map {
        let ref_schema = schema_map.get(block_type).ok_or_else(|| {
            format!(
                "referenced type '{}' not found in schema",
                block_type
            )
        })?;

        // Build atom arguments: label position gets a constant, referenced fields get variables,
        // everything else gets placeholder _.
        let mut atom_args = Vec::new();

        // Label position (first argument): interned integer matching the label.
        let label_id = string_table.intern(block_label);
        atom_args.push(AtomArg::Const(Const::Integer(label_id)));

        // For each attribute in the referenced block's schema:
        for ref_attr_name in ref_schema {
            let matching_var = field_vars
                .iter()
                .find(|(_, field)| field == ref_attr_name);
            if let Some((var_name, _)) = matching_var {
                atom_args.push(AtomArg::Var(var_name.clone()));
            } else {
                atom_args.push(AtomArg::Placeholder);
            }
        }

        let atom = Atom::from_str(block_type, atom_args);
        body_predicates.push(Predicate::AtomPredicate(atom));
    }

    // Build negated body atoms — one per negated referenced block.
    // Group by (block_type, block_label) to avoid duplicate negated atoms.
    let mut neg_atoms_map: IndexMap<(String, String), Vec<String>> = IndexMap::new();
    for (block_type, block_label, field) in &neg_bindings {
        neg_atoms_map
            .entry((block_type.clone(), block_label.clone()))
            .or_default()
            .push(field.clone());
    }

    for ((block_type, block_label), fields) in &neg_atoms_map {
        let ref_schema = schema_map.get(block_type).ok_or_else(|| {
            format!(
                "negated reference type '{}' not found in schema",
                block_type
            )
        })?;

        let mut atom_args = Vec::new();

        // Label position: constant matching the referenced label.
        let label_id = string_table.intern(block_label);
        atom_args.push(AtomArg::Const(Const::Integer(label_id)));

        // For each field in the referenced block's schema, check if there's a
        // positive var_binding with the same field name. If so, share the variable
        // to create the antijoin condition. Otherwise, use placeholder.
        for ref_attr_name in ref_schema {
            if fields.contains(ref_attr_name) {
                // This field is negated — find a matching positive variable by field name.
                let matching_positive = var_bindings
                    .iter()
                    .find(|(_, _, _, field)| field == ref_attr_name);
                if let Some((var_name, _, _, _)) = matching_positive {
                    atom_args.push(AtomArg::Var(var_name.clone()));
                } else {
                    // No matching positive binding — use placeholder (label-only antijoin).
                    atom_args.push(AtomArg::Placeholder);
                }
            } else {
                atom_args.push(AtomArg::Placeholder);
            }
        }

        let atom = Atom::from_str(block_type, atom_args);
        body_predicates.push(Predicate::NegatedAtomPredicate(atom));
    }

    // Build body atoms for data references.
    let mut data_body_atoms_map: IndexMap<(String, String), Vec<(String, String)>> =
        IndexMap::new();
    for (var_name, provider_type, label, field) in &data_bindings {
        data_body_atoms_map
            .entry((provider_type.clone(), label.clone()))
            .or_default()
            .push((var_name.clone(), field.clone()));
    }

    for ((provider_type, label), field_vars) in &data_body_atoms_map {
        let data_key = (provider_type.clone(), label.clone());
        let data_rel_name = format!("_data_{}_{}", provider_type, label);
        let data_col_names = data_schemas.get(&data_key).ok_or_else(|| {
            format!(
                "referenced data block data.{}.{} not found",
                provider_type, label
            )
        })?;

        // Data blocks have no label column — columns start directly.
        let mut atom_args = Vec::new();
        for col_name in data_col_names {
            let matching_var = field_vars.iter().find(|(_, field)| field == col_name);
            if let Some((var_name, _)) = matching_var {
                atom_args.push(AtomArg::Var(var_name.clone()));
            } else {
                atom_args.push(AtomArg::Placeholder);
            }
        }

        let atom = Atom::from_str(&data_rel_name, atom_args);
        body_predicates.push(Predicate::AtomPredicate(atom));
    }

    // Create a helper EDB to bind the label variable in the body.
    // E.g., _hcl_lbl_monitor_m1(HclLabel) with fact [label_id].
    let label_edb_name = format!("_hcl_lbl_{}_{}", resource.type_name, resource.label);
    let label_edb_decl = RelDecl::new(
        &label_edb_name,
        vec![Attribute::new("label", DataType::String)],
        None,
    );
    let label_atom = Atom::from_str(
        &label_edb_name,
        vec![AtomArg::Var("HclLabel".to_string())],
    );
    body_predicates.push(Predicate::AtomPredicate(label_atom));

    let rule = FLRule::new(head, body_predicates, false, false);
    Ok((rule, label_edb_name, label_edb_decl, vec![label_id]))
}

/// Convert a snake_case string to CamelCase for use as a Datalog variable name.
/// E.g., "target_ip" → "TargetIp", "dc" → "Dc".
pub fn to_datalog_var(s: &str) -> String {
    s.split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    let mut result = first.to_uppercase().to_string();
                    result.extend(chars);
                    result
                }
            }
        })
        .collect()
}

/// Generate Datalog text representation of the compiled program.
pub fn emit_datalog(result: &CompileResult) -> String {
    let mut out = String::new();

    // String table (for reference).
    if !result.string_table.id_to_str.is_empty() {
        writeln!(out, "// String table:").unwrap();
        for (id, s) in result.string_table.id_to_str.iter().enumerate() {
            writeln!(out, "// {} = \"{}\"", id, s).unwrap();
        }
        writeln!(out).unwrap();
    }

    // EDB declarations.
    if !result.program.edbs().is_empty() {
        writeln!(out, ".in").unwrap();
        for decl in result.program.edbs() {
            writeln!(out, ".decl {}", decl).unwrap();
        }
    }

    // EDB facts (as comments showing the logical content).
    // Use declared attribute types to decide how to display values.
    for (rel_name, facts) in &result.edb_facts {
        // Find the EDB declaration to get attribute types.
        let decl = result.program.edbs().iter().find(|d| d.name() == rel_name);
        writeln!(out).unwrap();
        for tuple in facts {
            let vals: Vec<String> = tuple
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let is_string = decl
                        .and_then(|d| d.attributes().get(i))
                        .map(|a| matches!(a.data_type(), DataType::String))
                        .unwrap_or(false);
                    if is_string {
                        if let Some(s) = result.string_table.decode(*v) {
                            format!("\"{}\"", s)
                        } else {
                            v.to_string()
                        }
                    } else {
                        v.to_string()
                    }
                })
                .collect();
            writeln!(out, "// {}({}).", rel_name, vals.join(", ")).unwrap();
        }
    }

    // IDB declarations.
    if !result.program.idbs().is_empty() {
        writeln!(out).unwrap();
        writeln!(out, ".printsize").unwrap();
        for decl in result.program.idbs() {
            writeln!(out, ".decl {}", decl).unwrap();
        }
    }

    // Rules.
    if !result.program.rules().is_empty() {
        writeln!(out).unwrap();
        for rule in result.program.rules() {
            writeln!(out, "{}", rule).unwrap();
        }
    }

    out
}

/// Write EDB facts to `.facts` files in the given directory.
/// Each file is named `{relation_name}.facts` with tab-separated i32 values.
pub fn write_facts(
    edb_facts: &HashMap<String, Vec<Vec<i32>>>,
    output_dir: &Path,
) -> Result<(), String> {
    fs::create_dir_all(output_dir)
        .map_err(|e| format!("failed to create facts directory: {}", e))?;

    for (rel_name, facts) in edb_facts {
        let path = output_dir.join(format!("{}.facts", rel_name));
        let mut content = String::new();
        for tuple in facts {
            let line: Vec<String> = tuple.iter().map(|v| v.to_string()).collect();
            writeln!(content, "{}", line.join("\t")).unwrap();
        }
        fs::write(&path, content)
            .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hcl_types::parse_hcl_body;

    #[test]
    fn test_compile_edb_only() {
        let hcl_src = r#"
            resource "server" "web1" {
                ip = "10.0.0.5"
                dc = "us-west"
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();

        assert_eq!(result.program.edbs().len(), 1);
        assert_eq!(result.program.edbs()[0].name(), "server");
        assert_eq!(result.program.edbs()[0].arity(), 3); // label, ip, dc
        assert_eq!(result.program.idbs().len(), 0);
        assert_eq!(result.program.rules().len(), 0);
        assert_eq!(result.edb_facts["server"].len(), 1);
    }

    #[test]
    fn test_compile_edb_and_idb() {
        let hcl_src = r#"
            resource "server" "web1" {
                ip = "10.0.0.5"
                dc = "us-west"
            }

            resource "monitor" "m1" {
                target_ip = server.web1.ip
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();

        // 1 EDB (server) + 1 label-binding EDB (_hcl_lbl_monitor_m1).
        assert_eq!(result.program.edbs().len(), 2);
        assert_eq!(result.program.idbs().len(), 1);
        assert_eq!(result.program.rules().len(), 1);

        let rule = &result.program.rules()[0];
        assert_eq!(rule.head().name(), "monitor");
        // 1 body atom (server join) + 1 body atom (label EDB binding).
        assert_eq!(rule.rhs().len(), 2);
    }

    #[test]
    fn test_variable_substitution() {
        let hcl_src = r#"
            variable "threshold" {
                default = 80
            }

            resource "config" "main" {
                limit = var.threshold
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();

        assert_eq!(result.program.edbs().len(), 1);
        assert_eq!(result.edb_facts["config"].len(), 1);
        // The threshold value (80) should be in the fact tuple.
        let tuple = &result.edb_facts["config"][0];
        assert_eq!(tuple[1], 80); // limit = 80
    }

    #[test]
    fn test_emit_datalog() {
        let hcl_src = r#"
            resource "server" "web1" {
                ip = "10.0.0.5"
                dc = "us-west"
            }

            resource "monitor" "m1" {
                target_ip = server.web1.ip
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();
        let dl = emit_datalog(&result);

        assert!(dl.contains(".in"));
        assert!(dl.contains(".decl server(label: string, ip: string, dc: string)"));
        assert!(dl.contains(".printsize"));
        assert!(dl.contains(".decl monitor(label: string, target_ip: string)"));
        assert!(dl.contains("monitor"));
        assert!(dl.contains("server"));
    }

    #[test]
    fn test_output_reference() {
        let hcl_src = r#"
            resource "server" "web1" {
                ip = "10.0.0.5"
                dc = "us-west"
            }

            output "server_ip" {
                value = server.web1.ip
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();

        // Should have one output.
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].name, "server_ip");
        assert_eq!(result.outputs[0].relation_name, "hcl_output_server_ip");

        // Should have an IDB declaration for the output.
        assert!(result.program.idbs().iter().any(|d| d.name() == "hcl_output_server_ip"));

        // Should have a rule for the output.
        let rule = result.program.rules().iter().find(|r| r.head().name() == "hcl_output_server_ip");
        assert!(rule.is_some());
    }

    #[test]
    fn test_output_reference_emit_dl() {
        let hcl_src = r#"
            resource "server" "web1" {
                ip = "10.0.0.5"
                dc = "us-west"
            }

            resource "monitor" "m1" {
                target_ip = server.web1.ip
            }

            output "monitors" {
                value = monitor.m1.target_ip
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();
        let dl = emit_datalog(&result);

        assert!(dl.contains(".decl hcl_output_monitors(value: string)"));
        // "m1" is interned as id 0 (first block processed in topo order).
        assert!(dl.contains("hcl_output_monitors(Value) :- monitor(0, Value)."),
            "Expected interned label in output rule, got:\n{}", dl);
    }

    #[test]
    fn test_output_literal() {
        let hcl_src = r#"
            output "greeting" {
                value = "hello"
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();

        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].name, "greeting");
        // Literal output becomes an EDB fact.
        assert!(result.edb_facts.contains_key("hcl_output_greeting"));
        assert_eq!(result.edb_facts["hcl_output_greeting"].len(), 1);
    }

    #[test]
    fn test_output_with_variable() {
        let hcl_src = r#"
            variable "port" {
                default = 8080
            }

            resource "server" "web1" {
                ip = "10.0.0.5"
                port = var.port
            }

            output "server_ip" {
                value = server.web1.ip
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();

        assert_eq!(result.outputs.len(), 1);
        assert!(result.program.idbs().iter().any(|d| d.name() == "hcl_output_server_ip"));
    }

    #[test]
    fn test_module_compile_emit_dl() {
        use std::io::Write;

        // Create a child module file.
        let child_hcl = r#"
            variable "ip" {
                default = "0.0.0.0"
            }

            resource "server" "s1" {
                ip = var.ip
            }

            output "server_ip" {
                value = server.s1.ip
            }
        "#;
        let mut child_file = tempfile::Builder::new()
            .suffix(".hcl")
            .tempfile()
            .unwrap();
        child_file.write_all(child_hcl.as_bytes()).unwrap();

        let parent_hcl = format!(
            r#"
            module "web" {{
                source = "{}"
                ip = "10.0.0.1"
            }}

            output "result" {{
                value = module.web.server_ip
            }}
        "#,
            child_file.path().to_string_lossy().replace('\\', "/")
        );

        let body: hcl::Body = hcl::from_str(&parent_hcl).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, Some(Path::new("/tmp")), &[], &[]).unwrap();
        let dl = emit_datalog(&result);

        // Should have namespaced EDB: web_server
        assert!(dl.contains("web_server"), "Expected web_server in DL:\n{}", dl);
        // Should have output IDB: hcl_output_result
        assert!(dl.contains("hcl_output_result"), "Expected hcl_output_result in DL:\n{}", dl);
        // Should have a rule connecting the output to the namespaced relation.
        assert!(dl.contains("hcl_output_result(Value) :- web_server("),
            "Expected output rule in DL:\n{}", dl);
    }

    #[test]
    fn test_data_block_edb() {
        // A data block should generate an EDB relation with the correct schema.
        let hcl_src = r#"
            output "greeting" {
                value = "hello"
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();

        let data_blocks = vec![FetchedDataBlock {
            provider_type: "csv".to_string(),
            label: "users".to_string(),
            schema: dbflow_plugin::DataSchema {
                columns: vec![
                    dbflow_plugin::ColumnDef {
                        name: "name".to_string(),
                        data_type: dbflow_plugin::DataType::String,
                    },
                    dbflow_plugin::ColumnDef {
                        name: "age".to_string(),
                        data_type: dbflow_plugin::DataType::Integer,
                    },
                ],
            },
            rows: vec![
                vec![
                    dbflow_plugin::DataValue::String("alice".to_string()),
                    dbflow_plugin::DataValue::Integer(30),
                ],
                vec![
                    dbflow_plugin::DataValue::String("bob".to_string()),
                    dbflow_plugin::DataValue::Integer(25),
                ],
            ],
        }];

        let result = compile(hcl_prog, None, &data_blocks, &[]).unwrap();

        // Should have an EDB declaration for _data_csv_users.
        assert!(
            result.program.edbs().iter().any(|d| d.name() == "_data_csv_users"),
            "Expected _data_csv_users EDB declaration"
        );

        // Should have 2 fact rows.
        let facts = &result.edb_facts["_data_csv_users"];
        assert_eq!(facts.len(), 2);

        // The integer column (age) should be raw i32 values.
        assert_eq!(facts[0][1], 30);
        assert_eq!(facts[1][1], 25);
    }

    #[test]
    fn test_data_reference_in_output() {
        // An output referencing a data block field should produce an IDB rule.
        let hcl_src = r#"
            output "user_name" {
                value = data.csv.users.name
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();

        let data_blocks = vec![FetchedDataBlock {
            provider_type: "csv".to_string(),
            label: "users".to_string(),
            schema: dbflow_plugin::DataSchema {
                columns: vec![
                    dbflow_plugin::ColumnDef {
                        name: "name".to_string(),
                        data_type: dbflow_plugin::DataType::String,
                    },
                    dbflow_plugin::ColumnDef {
                        name: "age".to_string(),
                        data_type: dbflow_plugin::DataType::Integer,
                    },
                ],
            },
            rows: vec![
                vec![
                    dbflow_plugin::DataValue::String("alice".to_string()),
                    dbflow_plugin::DataValue::Integer(30),
                ],
            ],
        }];

        let result = compile(hcl_prog, None, &data_blocks, &[]).unwrap();

        // Should have an IDB for the output.
        assert!(
            result.program.idbs().iter().any(|d| d.name() == "hcl_output_user_name"),
            "Expected hcl_output_user_name IDB declaration"
        );

        // Should have a rule: hcl_output_user_name(Value) :- _data_csv_users(Value, _).
        let rule = result.program.rules().iter()
            .find(|r| r.head().name() == "hcl_output_user_name")
            .expect("Expected rule for hcl_output_user_name");

        let dl = emit_datalog(&result);
        assert!(
            dl.contains("hcl_output_user_name(Value) :- _data_csv_users(Value, _)."),
            "Expected data reference output rule, got:\n{}",
            dl
        );
        let _ = rule;
    }

    #[test]
    fn test_data_reference_in_resource() {
        // A resource referencing a data block field should produce an IDB rule
        // with a data body atom.
        let hcl_src = r#"
            resource "enriched" "e1" {
                username = data.csv.users.name
            }

            output "result" {
                value = enriched.e1.username
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();

        let data_blocks = vec![FetchedDataBlock {
            provider_type: "csv".to_string(),
            label: "users".to_string(),
            schema: dbflow_plugin::DataSchema {
                columns: vec![
                    dbflow_plugin::ColumnDef {
                        name: "name".to_string(),
                        data_type: dbflow_plugin::DataType::String,
                    },
                ],
            },
            rows: vec![
                vec![dbflow_plugin::DataValue::String("alice".to_string())],
            ],
        }];

        let result = compile(hcl_prog, None, &data_blocks, &[]).unwrap();

        // Should have IDB for enriched.
        assert!(
            result.program.idbs().iter().any(|d| d.name() == "enriched"),
            "Expected enriched IDB declaration"
        );

        // The rule should reference _data_csv_users in its body.
        let dl = emit_datalog(&result);
        assert!(
            dl.contains("_data_csv_users(Username)"),
            "Expected data body atom in rule, got:\n{}",
            dl
        );
    }
}
