use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::hcl_types::{HclExpr, HclModule, HclOutput, HclProgram, HclResource};

/// Expand all module blocks in the program by loading and inlining child programs.
///
/// For each module instance:
/// 1. Read and parse the source file
/// 2. Recursively expand any nested modules
/// 3. Namespace all child type names with `{instance_name}_` prefix
/// 4. Substitute module inputs into child variables
/// 5. Record child outputs for reference resolution
/// 6. Append child resources/outputs to parent
///
/// After expansion, `program.modules` is empty and all resources are flattened.
pub fn expand_modules(program: &mut HclProgram, base_path: &Path) -> Result<(), String> {
    let mut visited = HashSet::new();
    expand_modules_inner(program, base_path, &mut visited)
}

fn expand_modules_inner(
    program: &mut HclProgram,
    base_path: &Path,
    visited: &mut HashSet<String>,
) -> Result<(), String> {
    let modules: Vec<HclModule> = program.modules.drain(..).collect();

    if modules.is_empty() {
        return Ok(());
    }

    // Map: (instance_name, output_name) -> namespaced HclExpr
    let mut output_map: HashMap<(String, String), HclExpr> = HashMap::new();

    for module in modules {
        // Resolve source path relative to base_path.
        let source_path = base_path.join(&module.source);
        let canonical = source_path.canonicalize().map_err(|e| {
            format!(
                "module '{}': cannot resolve source '{}' relative to '{}': {}",
                module.instance_name,
                module.source,
                base_path.display(),
                e
            )
        })?;

        let canonical_str = canonical.to_string_lossy().to_string();
        if !visited.insert(canonical_str.clone()) {
            return Err(format!(
                "circular module inclusion detected: '{}' ({})",
                module.instance_name,
                canonical.display()
            ));
        }

        // Read and parse the child file.
        let child_source = std::fs::read_to_string(&canonical).map_err(|e| {
            format!(
                "module '{}': cannot read '{}': {}",
                module.instance_name,
                canonical.display(),
                e
            )
        })?;

        let child_body: hcl::Body = hcl::from_str(&child_source).map_err(|e| {
            format!(
                "module '{}': HCL parse error in '{}': {}",
                module.instance_name,
                canonical.display(),
                e
            )
        })?;

        let mut child_program = crate::hcl_types::parse_hcl_body(&child_body)
            .map_err(|e| format!("module '{}': {}", module.instance_name, e))?;

        // Recursively expand child modules.
        let child_base = canonical.parent().unwrap_or(base_path);
        expand_modules_inner(&mut child_program, child_base, visited)?;

        // Remove the visited entry to allow the same module from different parents
        // (cycle detection is per-chain, not global).
        visited.remove(&canonical_str);

        // Substitute module inputs into child variables.
        for (input_name, input_expr) in &module.inputs {
            match input_expr {
                HclExpr::Literal(val) => {
                    child_program
                        .variables
                        .insert(input_name.clone(), val.clone());
                }
                HclExpr::Reference(_)
                | HclExpr::NegatedReference(_)
                | HclExpr::VarRef(_)
                | HclExpr::DataReference(_)
                | HclExpr::Comparison { .. }
                | HclExpr::Aggregate { .. }
                | HclExpr::ArithmeticOp { .. }
                | HclExpr::FunctionCall { .. } => {
                    // For reference/varref/data-ref inputs, substitute directly into child resources.
                    substitute_expr_in_program(&mut child_program, input_name, input_expr);
                }
            }
        }

        // Namespace child: prefix all type_name fields with {instance_name}_
        let prefix = &module.instance_name;
        namespace_program(&mut child_program, prefix);

        // Record child outputs in the output map.
        for output in &child_program.outputs {
            output_map.insert(
                (module.instance_name.clone(), output.name.clone()),
                output.value.clone(),
            );
        }

        // Append child resources and data blocks to parent.
        program.resources.extend(child_program.resources);
        program.data_blocks.extend(child_program.data_blocks);
        // Merge child variables (namespaced variables don't collide).
        program.variables.extend(child_program.variables);
        // Don't merge child outputs into parent outputs — they're accessed via module.X.Y references.
    }

    // Rewrite parent references: replace module.instance.output references
    // with the expression from the output map.
    rewrite_module_refs(&mut program.resources, &output_map);
    rewrite_module_refs_outputs(&mut program.outputs, &output_map);

    Ok(())
}

/// Substitute VarRef expressions in a child program for reference-type module inputs.
fn substitute_expr_in_program(program: &mut HclProgram, var_name: &str, replacement: &HclExpr) {
    for resource in &mut program.resources {
        for expr in resource.attributes.values_mut() {
            substitute_varref_in_expr(expr, var_name, replacement);
        }
    }
    for output in &mut program.outputs {
        substitute_varref_in_expr(&mut output.value, var_name, replacement);
    }
}

fn substitute_varref_in_expr(expr: &mut HclExpr, var_name: &str, replacement: &HclExpr) {
    match expr {
        HclExpr::VarRef(name) => {
            if name == var_name {
                *expr = replacement.clone();
            }
        }
        HclExpr::Comparison { lhs, rhs, .. } | HclExpr::ArithmeticOp { lhs, rhs, .. } => {
            substitute_varref_in_expr(lhs, var_name, replacement);
            substitute_varref_in_expr(rhs, var_name, replacement);
        }
        HclExpr::Aggregate { argument, .. } => {
            substitute_varref_in_expr(argument, var_name, replacement);
        }
        HclExpr::FunctionCall { args, .. } => {
            for arg in args {
                substitute_varref_in_expr(arg, var_name, replacement);
            }
        }
        HclExpr::Literal(_)
        | HclExpr::Reference(_)
        | HclExpr::NegatedReference(_)
        | HclExpr::DataReference(_) => {}
    }
}

/// Prefix all type_name and reference block_type fields with `{prefix}_`.
/// DataReferences are left unchanged since data providers are global.
fn namespace_program(program: &mut HclProgram, prefix: &str) {
    for resource in &mut program.resources {
        resource.type_name = format!("{}_{}", prefix, resource.type_name);
        for expr in resource.attributes.values_mut() {
            namespace_expr(expr, prefix);
        }
    }
    for output in &mut program.outputs {
        namespace_expr(&mut output.value, prefix);
    }
}

fn namespace_expr(expr: &mut HclExpr, prefix: &str) {
    match expr {
        HclExpr::Reference(r) | HclExpr::NegatedReference(r) => {
            if r.block_type != "module" {
                r.block_type = format!("{}_{}", prefix, r.block_type);
            }
        }
        HclExpr::Comparison { lhs, rhs, .. } | HclExpr::ArithmeticOp { lhs, rhs, .. } => {
            namespace_expr(lhs, prefix);
            namespace_expr(rhs, prefix);
        }
        HclExpr::Aggregate { argument, .. } => {
            namespace_expr(argument, prefix);
        }
        HclExpr::FunctionCall { args, .. } => {
            for arg in args {
                namespace_expr(arg, prefix);
            }
        }
        HclExpr::DataReference(_) | HclExpr::Literal(_) | HclExpr::VarRef(_) => {}
    }
}

/// Rewrite `module.{instance}.{output}` references in resource attributes
/// to the expression from the output map.
fn rewrite_module_refs(
    resources: &mut [HclResource],
    output_map: &HashMap<(String, String), HclExpr>,
) {
    for resource in resources.iter_mut() {
        for expr in resource.attributes.values_mut() {
            rewrite_module_ref_expr(expr, output_map);
        }
    }
}

fn rewrite_module_ref_expr(expr: &mut HclExpr, output_map: &HashMap<(String, String), HclExpr>) {
    match expr {
        HclExpr::Reference(r) | HclExpr::NegatedReference(r) => {
            if r.block_type == "module" {
                let key = (r.block_label.clone(), r.field.clone());
                if let Some(replacement) = output_map.get(&key) {
                    *expr = replacement.clone();
                }
            }
        }
        HclExpr::Comparison { lhs, rhs, .. } | HclExpr::ArithmeticOp { lhs, rhs, .. } => {
            rewrite_module_ref_expr(lhs, output_map);
            rewrite_module_ref_expr(rhs, output_map);
        }
        HclExpr::Aggregate { argument, .. } => {
            rewrite_module_ref_expr(argument, output_map);
        }
        HclExpr::FunctionCall { args, .. } => {
            for arg in args {
                rewrite_module_ref_expr(arg, output_map);
            }
        }
        HclExpr::DataReference(_) | HclExpr::Literal(_) | HclExpr::VarRef(_) => {}
    }
}

/// Rewrite `module.{instance}.{output}` references in output blocks.
fn rewrite_module_refs_outputs(
    outputs: &mut [HclOutput],
    output_map: &HashMap<(String, String), HclExpr>,
) {
    for output in outputs.iter_mut() {
        rewrite_module_ref_expr(&mut output.value, output_map);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hcl_types::parse_hcl_body;
    use std::io::Write;

    fn write_temp_hcl(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(".hcl").tempfile().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn test_simple_module_expansion() {
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
        let child_file = write_temp_hcl(child_hcl);

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
        let mut prog = parse_hcl_body(&body).unwrap();

        assert_eq!(prog.modules.len(), 1);

        expand_modules(&mut prog, Path::new("/tmp")).unwrap();

        // Modules should be drained.
        assert!(prog.modules.is_empty());

        // Should have one resource with namespaced type: web_server
        assert_eq!(prog.resources.len(), 1);
        assert_eq!(prog.resources[0].type_name, "web_server");

        // The parent output should have been rewritten from module.web.server_ip
        // to the child's namespaced reference.
        assert_eq!(prog.outputs.len(), 1);
        match &prog.outputs[0].value {
            HclExpr::Reference(r) => {
                assert_eq!(r.block_type, "web_server");
                assert_eq!(r.field, "ip");
            }
            other => panic!("expected Reference, got {:?}", other),
        }
    }

    #[test]
    fn test_circular_module_detection() {
        // Create two files that reference each other.
        let dir = tempfile::tempdir().unwrap();
        let a_path = dir.path().join("a.hcl");
        let b_path = dir.path().join("b.hcl");

        std::fs::write(
            &a_path,
            format!(
                "module \"b\" {{\n  source = \"{}\"\n}}",
                b_path.to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();
        std::fs::write(
            &b_path,
            format!(
                "module \"a\" {{\n  source = \"{}\"\n}}",
                a_path.to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();

        let source = std::fs::read_to_string(&a_path).unwrap();
        let body: hcl::Body = hcl::from_str(&source).unwrap();
        let mut prog = parse_hcl_body(&body).unwrap();

        let result = expand_modules(&mut prog, dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("circular"));
    }

    #[test]
    fn test_nested_modules() {
        let dir = tempfile::tempdir().unwrap();

        // Inner module: defines a resource.
        let inner_hcl = r#"
            resource "calc" "c1" {
                value = 42
            }
            output "result" {
                value = calc.c1.value
            }
        "#;
        let inner_path = dir.path().join("inner.hcl");
        std::fs::write(&inner_path, inner_hcl).unwrap();

        // Outer module: includes inner.
        let outer_hcl = format!(
            r#"
            module "inner" {{
                source = "{}"
            }}

            resource "wrapper" "w1" {{
                result = module.inner.result
            }}

            output "wrapped" {{
                value = wrapper.w1.result
            }}
        "#,
            inner_path.to_string_lossy().replace('\\', "/")
        );
        let outer_path = dir.path().join("outer.hcl");
        std::fs::write(&outer_path, &outer_hcl).unwrap();

        // Parent includes outer.
        let parent_hcl = format!(
            r#"
            module "outer" {{
                source = "{}"
            }}

            output "final" {{
                value = module.outer.wrapped
            }}
        "#,
            outer_path.to_string_lossy().replace('\\', "/")
        );

        let body: hcl::Body = hcl::from_str(&parent_hcl).unwrap();
        let mut prog = parse_hcl_body(&body).unwrap();

        expand_modules(&mut prog, dir.path()).unwrap();

        // Should have two resources: outer_inner_calc and outer_wrapper
        assert_eq!(prog.resources.len(), 2);
        let type_names: Vec<&str> = prog
            .resources
            .iter()
            .map(|r| r.type_name.as_str())
            .collect();
        assert!(type_names.contains(&"outer_inner_calc"));
        assert!(type_names.contains(&"outer_wrapper"));
    }
}
