use std::collections::{HashMap, HashSet};

use crate::hcl_types::{HclExpr, HclProgram, HclResource, Reference};

/// A unique identifier for a resource block: (type_name, label).
pub type BlockId = (String, String);

/// Classification of a resource block based on its references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    /// No references to other blocks — becomes an EDB fact.
    Edb,
    /// Has references to other blocks — becomes an IDB rule.
    Idb,
}

/// Result of analyzing the dependency structure of an HCL program.
#[derive(Debug)]
pub struct DependencyAnalysis {
    /// Classification of each block.
    pub block_kinds: HashMap<BlockId, BlockKind>,
    /// For each block, the set of blocks it directly references.
    pub dependencies: HashMap<BlockId, HashSet<BlockId>>,
    /// Blocks ordered so that dependencies come before dependents.
    pub topo_order: Vec<BlockId>,
    /// Strongly connected components with >1 member (recursive groups).
    pub recursive_groups: Vec<Vec<BlockId>>,
}

/// Collect all references from a resource block's attributes (positive and negated).
pub fn collect_references(resource: &HclResource) -> Vec<&Reference> {
    let mut refs = Vec::new();
    for expr in resource.attributes.values() {
        collect_references_from_expr(expr, &mut refs);
    }
    refs
}

fn collect_references_from_expr<'a>(expr: &'a HclExpr, refs: &mut Vec<&'a Reference>) {
    match expr {
        HclExpr::Reference(r) | HclExpr::NegatedReference(r) => refs.push(r),
        HclExpr::Comparison { lhs, rhs, .. } | HclExpr::ArithmeticOp { lhs, rhs, .. } => {
            collect_references_from_expr(lhs, refs);
            collect_references_from_expr(rhs, refs);
        }
        HclExpr::Aggregate { argument, .. } => {
            collect_references_from_expr(argument, refs);
        }
        HclExpr::Literal(_) | HclExpr::VarRef(_) | HclExpr::DataReference(_) => {}
    }
}

/// Check whether a resource block has any references to other blocks
/// (positive, negated, or data references).
pub fn has_references(resource: &HclResource) -> bool {
    resource.attributes.values().any(|expr| expr_has_references(expr))
}

fn expr_has_references(expr: &HclExpr) -> bool {
    match expr {
        HclExpr::Reference(_) | HclExpr::NegatedReference(_) | HclExpr::DataReference(_) => true,
        HclExpr::Comparison { lhs, rhs, .. } | HclExpr::ArithmeticOp { lhs, rhs, .. } => {
            expr_has_references(lhs) || expr_has_references(rhs)
        }
        HclExpr::Aggregate { argument, .. } => expr_has_references(argument),
        HclExpr::Literal(_) | HclExpr::VarRef(_) => false,
    }
}

/// Analyze the dependency graph of an HCL program.
///
/// Returns classification of each block as EDB/IDB, the dependency edges,
/// a topological ordering (when acyclic), and any recursive groups (SCCs).
pub fn analyze_dependencies(program: &HclProgram) -> DependencyAnalysis {
    let mut block_kinds = HashMap::new();
    let mut dependencies: HashMap<BlockId, HashSet<BlockId>> = HashMap::new();

    // Build the set of known block IDs for validation.
    let known_blocks: HashSet<BlockId> = program
        .resources
        .iter()
        .map(|r| (r.type_name.clone(), r.label.clone()))
        .collect();

    // Classify each block and collect its dependencies.
    for resource in &program.resources {
        let id = (resource.type_name.clone(), resource.label.clone());
        let refs = collect_references(resource);

        let mut deps = HashSet::new();
        for r in &refs {
            let target = (r.block_type.clone(), r.block_label.clone());
            if known_blocks.contains(&target) {
                deps.insert(target);
            }
        }

        let kind = if has_references(resource) {
            BlockKind::Idb
        } else {
            BlockKind::Edb
        };

        block_kinds.insert(id.clone(), kind);
        dependencies.insert(id, deps);
    }

    // Build adjacency list for topological sort / SCC detection.
    let block_ids: Vec<BlockId> = program
        .resources
        .iter()
        .map(|r| (r.type_name.clone(), r.label.clone()))
        .collect();

    let id_to_idx: HashMap<&BlockId, usize> = block_ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id, i))
        .collect();

    let n = block_ids.len();
    let mut adj: Vec<Vec<usize>> = vec![vec![]; n];
    let mut adj_rev: Vec<Vec<usize>> = vec![vec![]; n];

    for (id, deps) in &dependencies {
        if let Some(&from) = id_to_idx.get(id) {
            for dep in deps {
                if let Some(&to) = id_to_idx.get(dep) {
                    adj[from].push(to);
                    adj_rev[to].push(from);
                }
            }
        }
    }

    // Kosaraju's algorithm for SCCs.
    let mut visited = vec![false; n];
    let mut finish_order = Vec::with_capacity(n);

    fn dfs_forward(
        node: usize,
        adj: &[Vec<usize>],
        visited: &mut [bool],
        finish_order: &mut Vec<usize>,
    ) {
        visited[node] = true;
        for &next in &adj[node] {
            if !visited[next] {
                dfs_forward(next, adj, visited, finish_order);
            }
        }
        finish_order.push(node);
    }

    for i in 0..n {
        if !visited[i] {
            dfs_forward(i, &adj, &mut visited, &mut finish_order);
        }
    }

    let mut component = vec![0usize; n];
    let mut visited = vec![false; n];
    let mut current_component = 0;

    fn dfs_reverse(
        node: usize,
        adj_rev: &[Vec<usize>],
        visited: &mut [bool],
        component: &mut [usize],
        comp_id: usize,
    ) {
        visited[node] = true;
        component[node] = comp_id;
        for &next in &adj_rev[node] {
            if !visited[next] {
                dfs_reverse(next, adj_rev, visited, component, comp_id);
            }
        }
    }

    for &node in finish_order.iter().rev() {
        if !visited[node] {
            dfs_reverse(node, &adj_rev, &mut visited, &mut component, current_component);
            current_component += 1;
        }
    }

    // Group nodes by component.
    let mut scc_groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for (idx, &comp) in component.iter().enumerate() {
        scc_groups.entry(comp).or_default().push(idx);
    }

    let recursive_groups: Vec<Vec<BlockId>> = scc_groups
        .values()
        .filter(|group| {
            // A single node is recursive only if it has a self-loop.
            if group.len() == 1 {
                let node = group[0];
                adj[node].contains(&node)
            } else {
                true
            }
        })
        .map(|group| group.iter().map(|&idx| block_ids[idx].clone()).collect())
        .collect();

    // Topological sort of SCCs for ordering.
    // We use the reverse finish order of components as the topo order.
    let mut topo_order = Vec::with_capacity(n);
    let mut seen_components = HashSet::new();
    for &node in finish_order.iter().rev() {
        let comp = component[node];
        if seen_components.insert(comp) {
            // Add all members of this component.
            for &idx in &scc_groups[&comp] {
                topo_order.push(block_ids[idx].clone());
            }
        }
    }

    DependencyAnalysis {
        block_kinds,
        dependencies,
        topo_order,
        recursive_groups,
    }
}

/// Resolve `var.*` references by substituting variable values into expressions.
pub fn resolve_variables(program: &mut HclProgram) {
    let vars = program.variables.clone();
    for resource in &mut program.resources {
        for expr in resource.attributes.values_mut() {
            resolve_variables_in_expr(expr, &vars);
        }
    }
    for output in &mut program.outputs {
        resolve_variables_in_expr(&mut output.value, &vars);
    }
}

fn resolve_variables_in_expr(expr: &mut HclExpr, vars: &std::collections::HashMap<String, crate::hcl_types::HclValue>) {
    match expr {
        HclExpr::VarRef(name) => {
            if let Some(val) = vars.get(name) {
                *expr = HclExpr::Literal(val.clone());
            }
        }
        HclExpr::Comparison { lhs, rhs, .. } | HclExpr::ArithmeticOp { lhs, rhs, .. } => {
            resolve_variables_in_expr(lhs, vars);
            resolve_variables_in_expr(rhs, vars);
        }
        HclExpr::Aggregate { argument, .. } => {
            resolve_variables_in_expr(argument, vars);
        }
        HclExpr::Literal(_) | HclExpr::Reference(_) | HclExpr::NegatedReference(_)
        | HclExpr::DataReference(_) => {}
    }
}
