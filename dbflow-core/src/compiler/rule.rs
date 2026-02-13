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
        HclExpr::FunctionCall { args, .. } => {
            for arg in args {
                collect_leaf_refs(arg, refs);
            }
        }
        HclExpr::NegatedReference(_) | HclExpr::VarRef(_) => {}
    }
}

/// Resolve the DataType of an HclExpr leaf using available type information.
fn resolve_expr_type(
    expr: &HclExpr,
    var_bindings: &[(String, String, String, String)],
    data_bindings: &[(String, String, String, String)],
    data_col_types: &HashMap<(String, String), Vec<(String, DataType)>>,
    resource_map: &HashMap<(&str, &str), &HclResource>,
) -> DataType {
    match expr {
        HclExpr::Literal(HclValue::Float(_)) => DataType::Float,
        HclExpr::Literal(HclValue::Integer(_)) => DataType::Integer,
        HclExpr::DataReference(dr) => {
            let key = (dr.provider_type.clone(), dr.label.clone());
            data_col_types
                .get(&key)
                .and_then(|cols| {
                    cols.iter()
                        .find(|(name, _)| name == &dr.field)
                        .map(|(_, dt)| *dt)
                })
                .unwrap_or(DataType::Integer)
        }
        HclExpr::Reference(r) => {
            resource_map
                .get(&(r.block_type.as_str(), r.block_label.as_str()))
                .and_then(|res| res.attributes.get(&r.field))
                .map(|e| infer_data_type(e))
                .unwrap_or(DataType::Integer)
        }
        HclExpr::ArithmeticOp { lhs, rhs, .. } => {
            let lt = resolve_expr_type(lhs, var_bindings, data_bindings, data_col_types, resource_map);
            let rt = resolve_expr_type(rhs, var_bindings, data_bindings, data_col_types, resource_map);
            if lt == DataType::Float || rt == DataType::Float {
                DataType::Float
            } else {
                DataType::Integer
            }
        }
        _ => DataType::Integer,
    }
}

/// Promote type: if either side is Float, the result is Float.
fn promote_type(a: &DataType, b: &DataType) -> DataType {
    if *a == DataType::Float || *b == DataType::Float {
        DataType::Float
    } else {
        *a
    }
}

/// Convert an HclExpr into a FlowLog `Arithmetic` expression.
///
/// For leaf expressions (Reference, DataReference, Literal), produces a simple
/// `Arithmetic::new(Factor, vec![])`. For ArithmeticOp, flattens into init + rest pairs.
/// Type information is resolved from schema metadata and propagated via `Arithmetic::with_type()`.
fn hcl_expr_to_arithmetic(
    expr: &HclExpr,
    var_bindings: &[(String, String, String, String)],
    data_bindings: &[(String, String, String, String)],
    data_col_types: &HashMap<(String, String), Vec<(String, DataType)>>,
    resource_map: &HashMap<(&str, &str), &HclResource>,
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
            let dt = resolve_expr_type(expr, var_bindings, data_bindings, data_col_types, resource_map);
            Ok(Arithmetic::with_type(Factor::Var(var_name), vec![], dt))
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
            let dt = resolve_expr_type(expr, var_bindings, data_bindings, data_col_types, resource_map);
            Ok(Arithmetic::with_type(Factor::Var(var_name), vec![], dt))
        }
        HclExpr::Literal(HclValue::Integer(i)) => {
            Ok(Arithmetic::with_type(
                Factor::Const(Const::Integer(*i)),
                vec![],
                DataType::Integer,
            ))
        }
        HclExpr::Literal(HclValue::Float(f)) => {
            let bits = f.to_bits() as i64;
            Ok(Arithmetic::with_type(
                Factor::Const(Const::Float(bits)),
                vec![],
                DataType::Float,
            ))
        }
        HclExpr::Literal(v) => Err(CompileError::InvalidArithmeticExpr(format!(
            "non-numeric literal '{}' cannot be used in arithmetic/comparison",
            v
        ))),
        HclExpr::ArithmeticOp { lhs, operator, rhs } => {
            // Flatten: lhs becomes init, rhs becomes a single rest element.
            let lhs_arith = hcl_expr_to_arithmetic(lhs, var_bindings, data_bindings, data_col_types, resource_map)?;
            let rhs_arith = hcl_expr_to_arithmetic(rhs, var_bindings, data_bindings, data_col_types, resource_map)?;
            // Type promotion: Float if either side is Float.
            let dt = promote_type(lhs_arith.data_type(), rhs_arith.data_type());
            // Combine: take lhs's init and rest, append (op, rhs_init), then rhs's rest.
            let fl_op = hcl_arith_to_fl(operator);
            let mut rest = lhs_arith.rest().to_vec();
            rest.push((fl_op, rhs_arith.init().clone()));
            rest.extend(rhs_arith.rest().iter().cloned());
            Ok(Arithmetic::with_type(lhs_arith.init().clone(), rest, dt))
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

/// Describes a function-lookup EDB to be generated by the compiler.
pub(crate) struct FnEdbInfo {
    /// Name of the EDB relation (e.g., `_fn_neg_negated_all_0`).
    pub edb_name: String,
    /// Source data EDB name (e.g., `_data_csv_nums`).
    pub source_data_edb: String,
    /// Column index in source data for the function input.
    pub input_col_idx: usize,
    /// Which function to apply.
    pub function: super::types::ScalarFnKind,
}

/// Extra declarations and rules generated during multi-aggregate decomposition
/// or scalar function expansion.
pub(crate) struct ExtraDecls {
    /// Additional IDB declarations (helper aggregate relations).
    pub idbs: Vec<RelDecl>,
    /// Additional rules for the helper aggregate relations.
    pub rules: Vec<FLRule>,
    /// Additional EDB declarations (function lookup tables).
    pub fn_edbs: Vec<(RelDecl, FnEdbInfo)>,
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
/// Returns `(rule, label_edb_name, label_edb_decl, label_fact, Option<ExtraDecls>)`.
pub(crate) fn make_rule(
    resource: &HclResource,
    attr_names: &[String],
    _resource_map: &HashMap<(&str, &str), &HclResource>,
    schema_map: &IndexMap<String, Vec<String>>,
    data_schemas: &HashMap<(String, String), Vec<String>>,
    data_col_types: &HashMap<(String, String), Vec<(String, DataType)>>,
    string_table: &mut StringTable,
) -> Result<(FLRule, String, RelDecl, Vec<i64>, Option<ExtraDecls>), CompileError> {
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

    // Track aggregates: (head_arg_index, HeadArg::Aggregation, attr_name, operator, argument).
    let mut aggregates: Vec<(usize, HeadArg, String, HclAggregateOp, Box<HclExpr>)> = Vec::new();

    // Track scalar function calls: (head_arg_index, func_name, argument_expr).
    let mut function_calls: Vec<(usize, String, HclExpr)> = Vec::new();

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
                let fl_arith = hcl_expr_to_arithmetic(argument, &var_bindings, &data_bindings, data_col_types, _resource_map)?;
                let fl_op = hcl_agg_to_fl(operator);
                let agg_dt = resolve_expr_type(argument, &var_bindings, &data_bindings, data_col_types, _resource_map);
                let agg = Aggregation::with_type(fl_op, fl_arith, agg_dt);
                let idx = head_args.len();
                aggregates.push((
                    idx,
                    HeadArg::Aggregation(agg),
                    attr_name.clone(),
                    *operator,
                    argument.clone(),
                ));
                head_args.push(HeadArg::Var("__agg_placeholder__".to_string()));
                // placeholder, replaced below for single-aggregate case
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
                let fl_arith = hcl_expr_to_arithmetic(expr, &var_bindings, &data_bindings, data_col_types, _resource_map)?;
                head_args.push(HeadArg::Arith(fl_arith));
            }
            HclExpr::FunctionCall { name, args } => {
                // Auto-bind the argument references.
                let mut leaf_refs = Vec::new();
                for arg in args {
                    collect_leaf_refs(arg, &mut leaf_refs);
                }
                for leaf in &leaf_refs {
                    ensure_binding(
                        leaf,
                        &mut var_bindings,
                        &mut data_bindings,
                        &mut auto_var_counter,
                    );
                }
                let idx = head_args.len();
                function_calls.push((idx, name.clone(), args[0].clone()));
                // Placeholder — will be replaced by Arith in generated rules.
                head_args.push(HeadArg::Var("__func_placeholder__".to_string()));
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

    // Handle single aggregate case: replace placeholder with actual aggregation head arg.
    if aggregates.len() == 1 {
        let (idx, agg_arg, _, _, _) = aggregates.remove(0);
        head_args[idx] = agg_arg;
        // FlowLog requires the aggregate to be the last head argument.
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

    // --- Multi-aggregate decomposition ---
    // If 2+ aggregates, decompose into helper IDBs and a final join rule.
    if aggregates.len() >= 2 {
        return make_multi_aggregate_rule(
            resource,
            attr_names,
            &head_args,
            &aggregates,
            &var_bindings,
            &data_bindings,
            &neg_bindings,
            &comparisons,
            schema_map,
            data_schemas,
            data_col_types,
            _resource_map,
            string_table,
        );
    }

    // --- Scalar function expansion ---
    // If there are function calls, generate multiple rules (one per case).
    if !function_calls.is_empty() {
        return make_function_call_rules(
            resource,
            &head_args,
            &function_calls,
            &var_bindings,
            &data_bindings,
            &neg_bindings,
            &comparisons,
            schema_map,
            data_schemas,
            data_col_types,
            _resource_map,
            string_table,
        );
    }

    // Build the head.
    let head = Head::new(resource.type_name.clone(), head_args);

    // Build body atoms — one per referenced block.
    // Group references by (block_type, block_label) to avoid duplicate atoms.
    let body_predicates = build_body_predicates(
        &var_bindings,
        &data_bindings,
        &neg_bindings,
        &comparisons,
        schema_map,
        data_schemas,
        data_col_types,
        _resource_map,
        string_table,
    )?;

    // Create a helper EDB to bind the label variable in the body.
    // E.g., _hcl_lbl_monitor_m1(HclLabel) with fact [label_id].
    let label_edb_name = format!("_hcl_lbl_{}_{}", resource.type_name, resource.label);
    let label_edb_decl = RelDecl::new(
        &label_edb_name,
        vec![Attribute::new("label", DataType::String)],
        None,
    );
    let label_atom = Atom::from_str(&label_edb_name, vec![AtomArg::Var("HclLabel".to_string())]);
    let mut all_predicates = body_predicates;
    all_predicates.push(Predicate::AtomPredicate(label_atom));

    let rule = FLRule::new(head, all_predicates, false, false);
    Ok((rule, label_edb_name, label_edb_decl, vec![label_id], None))
}

/// Handle scalar function calls by generating auxiliary function-lookup EDBs.
///
/// The FlowLog engine cannot evaluate arithmetic in rule heads or aggregation arguments.
/// Instead, we generate an EDB lookup table `_fn_{name}_{type}_{label}_{idx}(input, output)`
/// that maps each input value to its function result. For batch data, the facts are
/// precomputed at compile time. For streaming data, the engine's encoding thread computes
/// them at runtime.
///
/// The rule then joins with the lookup EDB:
///   main(Label, ..., FnResult) :- body..., _fn_...(InputVar, FnResult), _hcl_lbl_...(Label).
#[allow(clippy::too_many_arguments)]
fn make_function_call_rules(
    resource: &HclResource,
    head_args: &[HeadArg],
    function_calls: &[(usize, String, HclExpr)],
    var_bindings: &[(String, String, String, String)],
    data_bindings: &[(String, String, String, String)],
    neg_bindings: &[(String, String, String)],
    comparisons: &[(HclComparisonOp, HclExpr, HclExpr)],
    schema_map: &IndexMap<String, Vec<String>>,
    data_schemas: &HashMap<(String, String), Vec<String>>,
    data_col_types: &HashMap<(String, String), Vec<(String, DataType)>>,
    resource_map: &HashMap<(&str, &str), &HclResource>,
    string_table: &mut StringTable,
) -> Result<(FLRule, String, RelDecl, Vec<i64>, Option<ExtraDecls>), CompileError> {
    let label_id = string_table.intern(&resource.label);
    let label_edb_name = format!("_hcl_lbl_{}_{}", resource.type_name, resource.label);
    let label_edb_decl = RelDecl::new(
        &label_edb_name,
        vec![Attribute::new("label", DataType::String)],
        None,
    );

    // Build the base body predicates.
    let base_body = build_body_predicates(
        var_bindings,
        data_bindings,
        neg_bindings,
        comparisons,
        schema_map,
        data_schemas,
        data_col_types,
        resource_map,
        string_table,
    )?;

    let mut fn_edb_infos: Vec<(RelDecl, FnEdbInfo)> = Vec::new();
    let mut fn_edb_body_atoms: Vec<Predicate> = Vec::new();
    let mut fn_result_vars: Vec<(usize, String)> = Vec::new(); // (head_idx, result_var_name)

    for (fc_idx, (_head_idx, func_name, arg_expr)) in function_calls.iter().enumerate() {
        let fn_edb_name = format!("_fn_{}_{}_{}", resource.type_name, resource.label, fc_idx);
        let result_var = format!("FnResult{}", fc_idx);

        // Determine the function kind.
        let fn_kind = match func_name.as_str() {
            "neg" => super::types::ScalarFnKind::Neg,
            "abs" => super::types::ScalarFnKind::Abs,
            other => {
                return Err(CompileError::Internal(format!(
                    "unsupported scalar function: {}",
                    other
                )));
            }
        };

        // Find the input variable name for the function argument.
        let input_var = match arg_expr {
            HclExpr::DataReference(r) => {
                data_bindings
                    .iter()
                    .find(|(_, pt, lbl, f)| {
                        *pt == r.provider_type && *lbl == r.label && *f == r.field
                    })
                    .map(|(v, _, _, _)| v.clone())
                    .ok_or_else(|| {
                        CompileError::Internal(format!(
                            "function arg data ref not bound: {}.{}.{}",
                            r.provider_type, r.label, r.field
                        ))
                    })?
            }
            HclExpr::Reference(r) => {
                var_bindings
                    .iter()
                    .find(|(_, bt, bl, f)| {
                        *bt == r.block_type && *bl == r.block_label && *f == r.field
                    })
                    .map(|(v, _, _, _)| v.clone())
                    .ok_or_else(|| {
                        CompileError::Internal(format!(
                            "function arg ref not bound: {}.{}.{}",
                            r.block_type, r.block_label, r.field
                        ))
                    })?
            }
            _ => {
                return Err(CompileError::Internal(
                    "function argument must be a reference".to_string(),
                ));
            }
        };

        // Determine the source data EDB and column index for streaming fn computation.
        let (source_edb, input_col_idx) = match arg_expr {
            HclExpr::DataReference(r) => {
                let data_rel = format!("_data_{}_{}", r.provider_type, r.label);
                let data_key = (r.provider_type.clone(), r.label.clone());
                let cols = data_schemas.get(&data_key).ok_or_else(|| {
                    CompileError::Internal(format!(
                        "no schema for data.{}.{}",
                        r.provider_type, r.label
                    ))
                })?;
                let col_idx = cols.iter().position(|c| *c == r.field).ok_or_else(|| {
                    CompileError::Internal(format!(
                        "column {} not found in data.{}.{}",
                        r.field, r.provider_type, r.label
                    ))
                })?;
                (data_rel, col_idx)
            }
            HclExpr::Reference(r) => {
                // Resource relation = type_name; column 0 = label, columns 1+ = attributes
                let source_edb = r.block_type.clone();
                let schema = schema_map.get(&r.block_type).ok_or_else(|| {
                    CompileError::UnknownReference {
                        context: "function call source".to_string(),
                        reference: format!("type '{}'", r.block_type),
                    }
                })?;
                let field_pos = schema.iter().position(|a| *a == r.field).ok_or_else(|| {
                    CompileError::UnknownReference {
                        context: "function call source".to_string(),
                        reference: format!("field '{}' in type '{}'", r.field, r.block_type),
                    }
                })?;
                let input_col_idx = field_pos + 1; // +1 for label at column 0
                (source_edb, input_col_idx)
            }
            _ => {
                return Err(CompileError::Internal(
                    "function argument must be a data or resource reference".to_string(),
                ));
            }
        };

        // Declare the function lookup EDB: (input: number, output: number)
        let fn_decl = RelDecl::new(
            &fn_edb_name,
            vec![
                Attribute::new("input", DataType::Integer),
                Attribute::new("output", DataType::Integer),
            ],
            None,
        );

        let fn_info = FnEdbInfo {
            edb_name: fn_edb_name.clone(),
            source_data_edb: source_edb,
            input_col_idx,
            function: fn_kind,
        };

        fn_edb_infos.push((fn_decl, fn_info));

        // Add a body atom joining with the function EDB.
        let fn_atom = Atom::from_str(
            &fn_edb_name,
            vec![
                AtomArg::Var(input_var),
                AtomArg::Var(result_var.clone()),
            ],
        );
        fn_edb_body_atoms.push(Predicate::AtomPredicate(fn_atom));

        fn_result_vars.push((*_head_idx, result_var));
    }

    // Build the final rule head, replacing function placeholders with result vars.
    let mut final_head_args: Vec<HeadArg> = Vec::new();
    for (i, arg) in head_args.iter().enumerate() {
        if let Some((_, result_var)) = fn_result_vars.iter().find(|(idx, _)| *idx == i) {
            final_head_args.push(HeadArg::Var(result_var.clone()));
        } else {
            final_head_args.push(arg.clone());
        }
    }

    let head = Head::new(resource.type_name.clone(), final_head_args);

    // Build body: base predicates + function EDB joins + label EDB.
    let mut body = base_body;
    body.extend(fn_edb_body_atoms);
    let label_atom = Atom::from_str(&label_edb_name, vec![AtomArg::Var("HclLabel".to_string())]);
    body.push(Predicate::AtomPredicate(label_atom));

    let rule = FLRule::new(head, body, false, false);

    Ok((
        rule,
        label_edb_name,
        label_edb_decl,
        vec![label_id],
        Some(ExtraDecls {
            idbs: Vec::new(),
            rules: Vec::new(),
            fn_edbs: fn_edb_infos,
        }),
    ))
}

/// Build body predicates from bindings (shared by single-rule and multi-aggregate paths).
#[allow(clippy::too_many_arguments)]
fn build_body_predicates(
    var_bindings: &[(String, String, String, String)],
    data_bindings: &[(String, String, String, String)],
    neg_bindings: &[(String, String, String)],
    comparisons: &[(HclComparisonOp, HclExpr, HclExpr)],
    schema_map: &IndexMap<String, Vec<String>>,
    data_schemas: &HashMap<(String, String), Vec<String>>,
    data_col_types: &HashMap<(String, String), Vec<(String, DataType)>>,
    resource_map: &HashMap<(&str, &str), &HclResource>,
    string_table: &mut StringTable,
) -> Result<Vec<Predicate>, CompileError> {
    let mut body_atoms_map: IndexMap<(String, String), Vec<(String, String)>> = IndexMap::new();
    for (var_name, block_type, block_label, field) in var_bindings {
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

        let mut atom_args = Vec::new();
        let label_id = string_table.intern(block_label);
        atom_args.push(AtomArg::Const(Const::Integer(label_id)));

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

    // Negated body atoms.
    let mut neg_atoms_map: IndexMap<(String, String), Vec<String>> = IndexMap::new();
    for (block_type, block_label, field) in neg_bindings {
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
        let label_id = string_table.intern(block_label);
        atom_args.push(AtomArg::Const(Const::Integer(label_id)));

        for ref_attr_name in ref_schema {
            if fields.contains(ref_attr_name) {
                let matching_positive = var_bindings
                    .iter()
                    .find(|(_, _, _, field)| field == ref_attr_name);
                if let Some((var_name, _, _, _)) = matching_positive {
                    atom_args.push(AtomArg::Var(var_name.clone()));
                } else {
                    atom_args.push(AtomArg::Placeholder);
                }
            } else {
                atom_args.push(AtomArg::Placeholder);
            }
        }

        let atom = Atom::from_str(block_type, atom_args);
        body_predicates.push(Predicate::NegatedAtomPredicate(atom));
    }

    // Data reference body atoms.
    let mut data_body_atoms_map: IndexMap<(String, String), Vec<(String, String)>> =
        IndexMap::new();
    for (var_name, provider_type, label, field) in data_bindings {
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

    // Comparison predicates.
    for (op, lhs, rhs) in comparisons {
        let fl_left = hcl_expr_to_arithmetic(lhs, var_bindings, data_bindings, data_col_types, resource_map)?;
        let fl_right = hcl_expr_to_arithmetic(rhs, var_bindings, data_bindings, data_col_types, resource_map)?;
        let fl_op = hcl_cmp_to_fl(op);
        let cmp = ComparisonExpr::new(fl_left, fl_op, fl_right);
        body_predicates.push(Predicate::ComparePredicate(cmp));
    }

    Ok(body_predicates)
}

/// Handle the multi-aggregate case: decompose into helper IDB rules and a join rule.
#[allow(clippy::too_many_arguments)]
fn make_multi_aggregate_rule(
    resource: &HclResource,
    attr_names: &[String],
    head_args: &[HeadArg],
    aggregates: &[(usize, HeadArg, String, HclAggregateOp, Box<HclExpr>)],
    var_bindings: &[(String, String, String, String)],
    data_bindings: &[(String, String, String, String)],
    neg_bindings: &[(String, String, String)],
    comparisons: &[(HclComparisonOp, HclExpr, HclExpr)],
    schema_map: &IndexMap<String, Vec<String>>,
    data_schemas: &HashMap<(String, String), Vec<String>>,
    data_col_types: &HashMap<(String, String), Vec<(String, DataType)>>,
    resource_map: &HashMap<(&str, &str), &HclResource>,
    string_table: &mut StringTable,
) -> Result<(FLRule, String, RelDecl, Vec<i64>, Option<ExtraDecls>), CompileError> {
    let label_id = string_table.intern(&resource.label);

    // Identify group-by columns: all non-aggregate head args, excluding label (index 0).
    let agg_indices: Vec<usize> = aggregates.iter().map(|(idx, _, _, _, _)| *idx).collect();
    let mut group_by_vars: Vec<String> = Vec::new();
    for (i, arg) in head_args.iter().enumerate() {
        if i == 0 {
            continue; // skip label
        }
        if agg_indices.contains(&i) {
            continue; // skip aggregates
        }
        if let HeadArg::Var(v) = arg {
            group_by_vars.push(v.clone());
        }
    }

    let mut extra_idbs = Vec::new();
    let mut extra_rules = Vec::new();
    let mut helper_rel_names = Vec::new();
    let mut helper_agg_var_names = Vec::new();

    // For each aggregate, generate a helper IDB.
    for (idx, (_, agg_head_arg, attr_name, _, _)) in aggregates.iter().enumerate() {
        let helper_name = format!("_agg_{}_{}_{}", resource.type_name, resource.label, idx);

        // Helper head: group-by vars + one aggregation.
        let mut helper_head_args: Vec<HeadArg> = group_by_vars
            .iter()
            .map(|v| HeadArg::Var(v.clone()))
            .collect();
        helper_head_args.push(agg_head_arg.clone());

        let helper_head = Head::new(helper_name.clone(), helper_head_args);

        // Helper body: same body atoms as original rule (data refs, resource refs, comparisons).
        let helper_body = build_body_predicates(
            var_bindings,
            data_bindings,
            neg_bindings,
            comparisons,
            schema_map,
            data_schemas,
            data_col_types,
            resource_map,
            string_table,
        )?;

        let helper_rule = FLRule::new(helper_head, helper_body, false, false);
        extra_rules.push(helper_rule);

        // Helper IDB declaration: group-by columns + aggregate column.
        let mut helper_attrs: Vec<Attribute> = group_by_vars
            .iter()
            .map(|v| {
                // Infer type from original schema: find the attr_name that maps to this var.
                let original_attr = attr_names.iter().find(|a| to_datalog_var(a) == *v);
                let dt = original_attr
                    .and_then(|a| resource.attributes.get(a))
                    .map(|e| super::types::infer_data_type(e))
                    .unwrap_or(DataType::String);
                Attribute::new(v, dt)
            })
            .collect();
        helper_attrs.push(Attribute::new(attr_name, DataType::Integer));

        let helper_decl = RelDecl::new(&helper_name, helper_attrs, None);
        extra_idbs.push(helper_decl);

        // Track for the final join rule.
        let agg_var_name = to_datalog_var(attr_name);
        helper_rel_names.push(helper_name);
        helper_agg_var_names.push(agg_var_name);
    }

    // Build the final join rule:
    // type_name(HclLabel, GroupBy1, ..., Agg1, Agg2, ...) :-
    //   _agg_..._0(GroupBy1, ..., Agg1),
    //   _agg_..._1(GroupBy1, ..., Agg2),
    //   _hcl_lbl_...(HclLabel).
    let mut final_head_args: Vec<HeadArg> = vec![HeadArg::Var("HclLabel".to_string())];
    // Add group-by vars.
    for v in &group_by_vars {
        final_head_args.push(HeadArg::Var(v.clone()));
    }
    // Add aggregate result vars (in order of original schema).
    // We need to rebuild head_args in schema order, replacing aggregate placeholders.
    // Actually, let's build in the correct schema order: iterate attr_names.
    let mut final_head_args: Vec<HeadArg> = vec![HeadArg::Var("HclLabel".to_string())];
    for (i, arg) in head_args.iter().enumerate() {
        if i == 0 {
            continue; // label already added
        }
        if let Some(agg_pos) = aggregates.iter().position(|(idx, _, _, _, _)| *idx == i) {
            // This was an aggregate placeholder — use the agg var name.
            final_head_args.push(HeadArg::Var(helper_agg_var_names[agg_pos].clone()));
        } else {
            final_head_args.push(arg.clone());
        }
    }

    let final_head = Head::new(resource.type_name.clone(), final_head_args);

    // Build body: one atom per helper relation + label EDB.
    let mut final_body = Vec::new();
    for (idx, helper_name) in helper_rel_names.iter().enumerate() {
        let mut atom_args: Vec<AtomArg> = group_by_vars
            .iter()
            .map(|v| AtomArg::Var(v.clone()))
            .collect();
        atom_args.push(AtomArg::Var(helper_agg_var_names[idx].clone()));
        let atom = Atom::from_str(helper_name, atom_args);
        final_body.push(Predicate::AtomPredicate(atom));
    }

    // Label EDB.
    let label_edb_name = format!("_hcl_lbl_{}_{}", resource.type_name, resource.label);
    let label_edb_decl = RelDecl::new(
        &label_edb_name,
        vec![Attribute::new("label", DataType::String)],
        None,
    );
    let label_atom = Atom::from_str(&label_edb_name, vec![AtomArg::Var("HclLabel".to_string())]);
    final_body.push(Predicate::AtomPredicate(label_atom));

    let final_rule = FLRule::new(final_head, final_body, false, false);

    Ok((
        final_rule,
        label_edb_name,
        label_edb_decl,
        vec![label_id],
        Some(ExtraDecls {
            idbs: extra_idbs,
            rules: extra_rules,
            fn_edbs: Vec::new(),
        }),
    ))
}
