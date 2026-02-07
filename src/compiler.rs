use std::collections::{HashMap, HashSet};
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;

use indexmap::IndexMap;
use parsing::decl::{Attribute, DataType, RelDecl};
use parsing::head::{Head, HeadArg};
use parsing::parser::Program;
use parsing::rule::{Atom, AtomArg, Const, FLRule, Predicate};

use crate::hcl_types::{HclExpr, HclProgram, HclResource, HclValue};
use crate::reference::{analyze_dependencies, resolve_variables, BlockKind, DependencyAnalysis};

/// Bidirectional string interning table.
/// Maps string values to unique i32 identifiers for FlowLog execution.
#[derive(Debug, Default)]
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

/// Result of compiling an HCL program.
pub struct CompileResult {
    pub program: Program,
    pub string_table: StringTable,
    pub analysis: DependencyAnalysis,
    /// For each EDB relation name, the list of fact tuples (as i32 vectors).
    pub edb_facts: HashMap<String, Vec<Vec<i32>>>,
}

/// Compile an `HclProgram` into a FlowLog `Program`.
pub fn compile(mut hcl_program: HclProgram) -> Result<CompileResult, String> {
    // Resolve variable references first.
    resolve_variables(&mut hcl_program);

    // Analyze dependencies to classify blocks.
    let analysis = analyze_dependencies(&hcl_program);

    let mut string_table = StringTable::default();
    let mut edbs = Vec::new();
    let mut idbs = Vec::new();
    let mut rules = Vec::new();
    let mut edb_facts: HashMap<String, Vec<Vec<i32>>> = HashMap::new();

    // Build a lookup from (type_name, label) to the resource for reference resolution.
    let resource_map: HashMap<(&str, &str), &HclResource> = hcl_program
        .resources
        .iter()
        .map(|r| ((r.type_name.as_str(), r.label.as_str()), r))
        .collect();

    // Build schema map: type_name → ordered attribute names (across all blocks of that type).
    // All blocks of the same type must share a schema.
    let mut schema_map: IndexMap<String, Vec<String>> = IndexMap::new();
    for resource in &hcl_program.resources {
        let entry = schema_map
            .entry(resource.type_name.clone())
            .or_default();
        for attr_name in resource.attributes.keys() {
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

                // Generate a rule: head :- body.
                let rule = make_rule(
                    resource,
                    attr_names,
                    &resource_map,
                    &schema_map,
                    &mut string_table,
                )?;
                rules.push(rule);
            }
        }
    }

    // Handle output blocks — add them as IDB declarations if not already declared.
    for output in &hcl_program.outputs {
        match &output.value {
            HclExpr::Reference(r) => {
                // Output references a specific block's type — ensure that type is in idbs.
                if !declared_idb.contains(&r.block_type) && !declared_edb.contains(&r.block_type) {
                    // The referenced type should already be declared; this is just a query marker.
                }
            }
            _ => {
                // For now, outputs must reference a block type.
            }
        }
    }

    let program = Program::new(edbs, idbs, rules);

    Ok(CompileResult {
        program,
        string_table,
        analysis,
        edb_facts,
    })
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
        HclExpr::Reference(_) | HclExpr::VarRef(_) => DataType::String, // references produce strings
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
/// monitor("m1", TargetIp) :- server("web1", TargetIp, _).
/// ```
fn make_rule(
    resource: &HclResource,
    attr_names: &[String],
    _resource_map: &HashMap<(&str, &str), &HclResource>,
    schema_map: &IndexMap<String, Vec<String>>,
    string_table: &mut StringTable,
) -> Result<FLRule, String> {
    // Build head arguments.
    let mut head_args = Vec::new();
    // First argument is the label constant.
    let _label_id = string_table.intern(&resource.label);
    head_args.push(HeadArg::Var(format!("\"{}\"", resource.label)));

    // Track which variables we introduce (for the body atoms).
    // Map: variable_name → (source_type, source_label, source_field)
    let mut var_bindings: Vec<(String, String, String, String)> = Vec::new();

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
            HclExpr::VarRef(_) => {
                return Err(format!(
                    "unresolved variable reference in {}.{}.{}",
                    resource.type_name, resource.label, attr_name
                ));
            }
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

        // Label position (first argument): constant matching the label (with quotes for display).
        atom_args.push(AtomArg::Const(Const::Text(format!("\"{}\"", block_label))));

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

    // Handle literal attributes in the head by adding them as constant bindings.
    // For attributes that are literals in an IDB block, we need body constraints.
    // Since HeadArg::Var is what we used, we need comparison predicates.
    // For the MVP, we'll handle this by making the head use the variable name
    // and adding an equality comparison in the body.
    // Actually — rethinking: for literals in IDB heads, we should just put
    // them as the constant in the head. But Head only supports HeadArg::Var.
    // The simplest MVP approach: treat literal IDB attributes as constant
    // variable names that are bound in the head directly. FlowLog's display
    // will show them. But internally they won't join on anything.
    //
    // For now, the MVP handles the main case: references become join variables.
    // Literal attributes in IDB blocks are an edge case we can address later.

    let rule = FLRule::new(head, body_predicates, false, false);
    Ok(rule)
}

/// Convert a snake_case string to CamelCase for use as a Datalog variable name.
/// E.g., "target_ip" → "TargetIp", "dc" → "Dc".
fn to_datalog_var(s: &str) -> String {
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
        let result = compile(hcl_prog).unwrap();

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
        let result = compile(hcl_prog).unwrap();

        assert_eq!(result.program.edbs().len(), 1);
        assert_eq!(result.program.idbs().len(), 1);
        assert_eq!(result.program.rules().len(), 1);

        let rule = &result.program.rules()[0];
        assert_eq!(rule.head().name(), "monitor");
        assert_eq!(rule.rhs().len(), 1);
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
        let result = compile(hcl_prog).unwrap();

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
        let result = compile(hcl_prog).unwrap();
        let dl = emit_datalog(&result);

        assert!(dl.contains(".in"));
        assert!(dl.contains(".decl server(label: string, ip: string, dc: string)"));
        assert!(dl.contains(".printsize"));
        assert!(dl.contains(".decl monitor(label: string, target_ip: string)"));
        assert!(dl.contains("monitor"));
        assert!(dl.contains("server"));
    }
}
