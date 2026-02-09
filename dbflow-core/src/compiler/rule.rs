use std::collections::HashMap;

use indexmap::IndexMap;
use parsing::aggregation::{Aggregation, AggregationOperator};
use parsing::arithmetic::{Arithmetic, ArithmeticOperator, Factor};
use parsing::compare::{ComparisonExpr, ComparisonOperator};
use parsing::decl::{Attribute, DataType, RelDecl};
use parsing::head::{Head, HeadArg};
use parsing::rule::{Atom, AtomArg, Const, FLRule, Predicate};

use super::emit::to_datalog_var;
use super::error::CompileError;
use super::types::{infer_data_type, StringTable};
use crate::hcl_types::{
    HclAggregateOp, HclArithmeticOp, HclComparisonOp, HclExpr, HclResource, HclValue,
};

/// Create a `RelDecl` for a resource type.
/// Schema: type_name(label: string, attr1: type, attr2: type, ...)
pub(crate) fn make_rel_decl(
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

/// Convert an HCL comparison operator to a FlowLog comparison operator.
fn hcl_cmp_to_fl(op: &HclComparisonOp) -> ComparisonOperator {
    match op {
        HclComparisonOp::Eq => ComparisonOperator::Equals,
        HclComparisonOp::NotEq => ComparisonOperator::NotEquals,
        HclComparisonOp::Less => ComparisonOperator::LessThan,
        HclComparisonOp::LessEq => ComparisonOperator::LessEqualThan,
        HclComparisonOp::Greater => ComparisonOperator::GreaterThan,
        HclComparisonOp::GreaterEq => ComparisonOperator::GreaterEqualThan,
    }
}

/// Convert an HCL aggregate operator to a FlowLog aggregation operator.
fn hcl_agg_to_fl(op: &HclAggregateOp) -> AggregationOperator {
    match op {
        HclAggregateOp::Count => AggregationOperator::Count,
        HclAggregateOp::Sum => AggregationOperator::Sum,
        HclAggregateOp::Min => AggregationOperator::Min,
        HclAggregateOp::Max => AggregationOperator::Max,
    }
}

/// Convert an HCL arithmetic operator to a FlowLog arithmetic operator.
fn hcl_arith_to_fl(op: &HclArithmeticOp) -> ArithmeticOperator {
    match op {
        HclArithmeticOp::Plus => ArithmeticOperator::Plus,
        HclArithmeticOp::Minus => ArithmeticOperator::Minus,
        HclArithmeticOp::Mul => ArithmeticOperator::Multiply,
        HclArithmeticOp::Div => ArithmeticOperator::Divide,
        HclArithmeticOp::Mod => ArithmeticOperator::Modulo,
    }
}

/// Collect all leaf references (Reference and DataReference) from an HclExpr recursively.
/// Used to auto-bind variables that appear in comparisons/aggregates/arithmetic.
fn collect_leaf_refs(expr: &HclExpr, refs: &mut Vec<HclExpr>) {
    match expr {
        HclExpr::Reference(_) | HclExpr::DataReference(_) | HclExpr::Literal(_) => {
            refs.push(expr.clone());
        }
        HclExpr::Comparison { lhs, rhs, .. } | HclExpr::ArithmeticOp { lhs, rhs, .. } => {
            collect_leaf_refs(lhs, refs);
            collect_leaf_refs(rhs, refs);
        }
        HclExpr::Aggregate { argument, .. } => {
            collect_leaf_refs(argument, refs);
        }
        HclExpr::NegatedReference(_) | HclExpr::VarRef(_) => {}
    }
}

/// Convert an HclExpr into a FlowLog `Arithmetic` expression.
///
/// For leaf expressions (Reference, DataReference, Literal), produces a simple
/// `Arithmetic::new(Factor, vec![])`. For ArithmeticOp, flattens into init + rest pairs.
fn hcl_expr_to_arithmetic(
    expr: &HclExpr,
    var_bindings: &[(String, String, String, String)],
    data_bindings: &[(String, String, String, String)],
) -> Result<Arithmetic, CompileError> {
    match expr {
        HclExpr::Reference(r) => {
            // Find the variable name from var_bindings that matches this reference's field.
            let var_name = var_bindings
                .iter()
                .find(|(_, bt, bl, f)| bt == &r.block_type && bl == &r.block_label && f == &r.field)
                .map(|(vn, _, _, _)| vn.clone())
                .ok_or_else(|| CompileError::UnknownReference {
                    context: format!("{}.{}", r.block_type, r.block_label),
                    reference: format!("field '{}' (not bound to a variable)", r.field),
                })?;
            Ok(Arithmetic::new(Factor::Var(var_name), vec![]))
        }
        HclExpr::DataReference(dr) => {
            let var_name = data_bindings
                .iter()
                .find(|(_, pt, l, f)| pt == &dr.provider_type && l == &dr.label && f == &dr.field)
                .map(|(vn, _, _, _)| vn.clone())
                .ok_or_else(|| CompileError::UnknownReference {
                    context: format!("data.{}.{}", dr.provider_type, dr.label),
                    reference: format!("field '{}' (not bound to a variable)", dr.field),
                })?;
            Ok(Arithmetic::new(Factor::Var(var_name), vec![]))
        }
        HclExpr::Literal(HclValue::Integer(i)) => {
            Ok(Arithmetic::new(Factor::Const(Const::Integer(*i)), vec![]))
        }
        HclExpr::Literal(v) => Err(CompileError::InvalidArithmeticExpr(format!(
            "non-integer literal '{}' cannot be used in arithmetic/comparison",
            v
        ))),
        HclExpr::ArithmeticOp { lhs, operator, rhs } => {
            // Flatten: lhs becomes init, rhs becomes a single rest element.
            let lhs_arith = hcl_expr_to_arithmetic(lhs, var_bindings, data_bindings)?;
            let rhs_arith = hcl_expr_to_arithmetic(rhs, var_bindings, data_bindings)?;
            // Combine: take lhs's init and rest, append (op, rhs_init), then rhs's rest.
            let fl_op = hcl_arith_to_fl(operator);
            let mut rest = lhs_arith.rest().to_vec();
            rest.push((fl_op, rhs_arith.init().clone()));
            rest.extend(rhs_arith.rest().iter().cloned());
            Ok(Arithmetic::new(lhs_arith.init().clone(), rest))
        }
        _ => Err(CompileError::InvalidArithmeticExpr(format!(
            "unsupported expression in arithmetic context: {:?}",
            expr
        ))),
    }
}

/// Ensure a leaf reference is bound in the appropriate bindings list.
/// If not already present, auto-bind it with a generated variable name.
/// Returns the variable name.
fn ensure_binding(
    expr: &HclExpr,
    var_bindings: &mut Vec<(String, String, String, String)>,
    data_bindings: &mut Vec<(String, String, String, String)>,
    auto_var_counter: &mut usize,
) -> Option<String> {
    match expr {
        HclExpr::Reference(r) => {
            // Check if already bound.
            if let Some((vn, _, _, _)) = var_bindings
                .iter()
                .find(|(_, bt, bl, f)| bt == &r.block_type && bl == &r.block_label && f == &r.field)
            {
                return Some(vn.clone());
            }
            // Auto-bind with a generated variable name.
            let var_name = format!("AutoVar{}", auto_var_counter);
            *auto_var_counter += 1;
            var_bindings.push((
                var_name.clone(),
                r.block_type.clone(),
                r.block_label.clone(),
                r.field.clone(),
            ));
            Some(var_name)
        }
        HclExpr::DataReference(dr) => {
            if let Some((vn, _, _, _)) = data_bindings
                .iter()
                .find(|(_, pt, l, f)| pt == &dr.provider_type && l == &dr.label && f == &dr.field)
            {
                return Some(vn.clone());
            }
            let var_name = format!("AutoVar{}", auto_var_counter);
            *auto_var_counter += 1;
            data_bindings.push((
                var_name.clone(),
                dr.provider_type.clone(),
                dr.label.clone(),
                dr.field.clone(),
            ));
            Some(var_name)
        }
        HclExpr::Literal(_) => None, // Literals don't need bindings.
        _ => None,
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
pub(crate) fn make_rule(
    resource: &HclResource,
    attr_names: &[String],
    _resource_map: &HashMap<(&str, &str), &HclResource>,
    schema_map: &IndexMap<String, Vec<String>>,
    data_schemas: &HashMap<(String, String), Vec<String>>,
    string_table: &mut StringTable,
) -> Result<(FLRule, String, RelDecl, Vec<i32>), CompileError> {
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

    // Track comparison expressions to emit as predicates later.
    let mut comparisons: Vec<(HclComparisonOp, HclExpr, HclExpr)> = Vec::new();

    // Counter for auto-generated variable names.
    let mut auto_var_counter = 0usize;

    // Track which attribute has an aggregate (at most one).
    let mut aggregate_head_idx: Option<usize> = None;
    let mut aggregate_head_arg: Option<HeadArg> = None;

    for attr_name in attr_names {
        let expr = resource.attributes.get(attr_name).ok_or_else(|| {
            CompileError::MissingAttribute {
                type_name: resource.type_name.clone(),
                label: resource.label.clone(),
                attribute: attr_name.clone(),
            }
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
            HclExpr::Aggregate { operator, argument } => {
                if aggregate_head_idx.is_some() {
                    return Err(CompileError::MultipleAggregates {
                        type_name: resource.type_name.clone(),
                        label: resource.label.clone(),
                    });
                }
                // Auto-bind the inner reference.
                let mut leaf_refs = Vec::new();
                collect_leaf_refs(argument, &mut leaf_refs);
                for leaf in &leaf_refs {
                    ensure_binding(
                        leaf,
                        &mut var_bindings,
                        &mut data_bindings,
                        &mut auto_var_counter,
                    );
                }
                // Build the FlowLog aggregation.
                let fl_arith = hcl_expr_to_arithmetic(argument, &var_bindings, &data_bindings)?;
                let fl_op = hcl_agg_to_fl(operator);
                let agg = Aggregation::new(fl_op, fl_arith);
                aggregate_head_idx = Some(head_args.len());
                aggregate_head_arg = Some(HeadArg::Aggregation(agg));
                head_args.push(HeadArg::Var("__agg_placeholder__".to_string()));
                // placeholder, replaced below
            }
            HclExpr::ArithmeticOp { .. } => {
                // Arithmetic as a standalone attribute value — treat like aggregate argument binding.
                let _var_name = to_datalog_var(attr_name);
                let mut leaf_refs = Vec::new();
                collect_leaf_refs(expr, &mut leaf_refs);
                for leaf in &leaf_refs {
                    ensure_binding(
                        leaf,
                        &mut var_bindings,
                        &mut data_bindings,
                        &mut auto_var_counter,
                    );
                }
                let fl_arith = hcl_expr_to_arithmetic(expr, &var_bindings, &data_bindings)?;
                head_args.push(HeadArg::Arith(fl_arith));
            }
            HclExpr::Comparison { .. } => {
                // Comparisons are excluded from schema, so they won't appear here.
                // This case shouldn't be reached (schema excludes them), but handle gracefully.
            }
            HclExpr::VarRef(_) => {
                return Err(CompileError::UnresolvedVariable {
                    context: format!("{}.{}.{}", resource.type_name, resource.label, attr_name),
                    var_name: if let HclExpr::VarRef(name) = expr {
                        name.clone()
                    } else {
                        String::new()
                    },
                });
            }
        }
    }

    // Replace the aggregate placeholder with the actual HeadArg::Aggregation.
    if let (Some(idx), Some(agg_arg)) = (aggregate_head_idx, aggregate_head_arg) {
        head_args[idx] = agg_arg;
        // FlowLog requires the aggregate to be the last head argument.
        // Move it to the end if it isn't already.
        if idx != head_args.len() - 1 {
            let agg = head_args.remove(idx);
            head_args.push(agg);
        }
    }

    // Collect comparison expressions from ALL attributes (comparisons are excluded from schema).
    for (_, expr) in &resource.attributes {
        if let HclExpr::Comparison { lhs, operator, rhs } = expr {
            // Auto-bind any leaf references in the comparison.
            let mut leaf_refs = Vec::new();
            collect_leaf_refs(lhs, &mut leaf_refs);
            collect_leaf_refs(rhs, &mut leaf_refs);
            for leaf in &leaf_refs {
                ensure_binding(
                    leaf,
                    &mut var_bindings,
                    &mut data_bindings,
                    &mut auto_var_counter,
                );
            }
            comparisons.push((*operator, *lhs.clone(), *rhs.clone()));
        }
    }

    // Collect negated bindings from ALL attributes (not just schema attrs).
    for (_, expr) in &resource.attributes {
        if let HclExpr::NegatedReference(r) = expr {
            neg_bindings.push((r.block_type.clone(), r.block_label.clone(), r.field.clone()));
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
        let ref_schema = schema_map
            .get(block_type)
            .ok_or_else(|| CompileError::UnknownReference {
                context: "rule body".to_string(),
                reference: format!("type '{}'", block_type),
            })?;

        // Build atom arguments: label position gets a constant, referenced fields get variables,
        // everything else gets placeholder _.
        let mut atom_args = Vec::new();

        // Label position (first argument): interned integer matching the label.
        let label_id = string_table.intern(block_label);
        atom_args.push(AtomArg::Const(Const::Integer(label_id)));

        // For each attribute in the referenced block's schema:
        for ref_attr_name in ref_schema {
            let matching_var = field_vars.iter().find(|(_, field)| field == ref_attr_name);
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
            CompileError::UnknownReference {
                context: "negated reference".to_string(),
                reference: format!("type '{}'", block_type),
            }
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
            CompileError::UnknownReference {
                context: "rule body".to_string(),
                reference: format!("data block data.{}.{}", provider_type, label),
            }
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

    // Emit comparison predicates.
    for (op, lhs, rhs) in &comparisons {
        let fl_left = hcl_expr_to_arithmetic(lhs, &var_bindings, &data_bindings)?;
        let fl_right = hcl_expr_to_arithmetic(rhs, &var_bindings, &data_bindings)?;
        let fl_op = hcl_cmp_to_fl(op);
        let cmp = ComparisonExpr::new(fl_left, fl_op, fl_right);
        body_predicates.push(Predicate::ComparePredicate(cmp));
    }

    // Create a helper EDB to bind the label variable in the body.
    // E.g., _hcl_lbl_monitor_m1(HclLabel) with fact [label_id].
    let label_edb_name = format!("_hcl_lbl_{}_{}", resource.type_name, resource.label);
    let label_edb_decl = RelDecl::new(
        &label_edb_name,
        vec![Attribute::new("label", DataType::String)],
        None,
    );
    let label_atom = Atom::from_str(&label_edb_name, vec![AtomArg::Var("HclLabel".to_string())]);
    body_predicates.push(Predicate::AtomPredicate(label_atom));

    let rule = FLRule::new(head, body_predicates, false, false);
    Ok((rule, label_edb_name, label_edb_decl, vec![label_id]))
}
