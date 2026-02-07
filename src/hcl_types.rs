use indexmap::IndexMap;
use std::collections::HashMap;
use std::fmt;

/// A parsed HCL program containing all block types.
#[derive(Debug)]
pub struct HclProgram {
    pub variables: HashMap<String, HclValue>,
    pub resources: Vec<HclResource>,
    pub outputs: Vec<HclOutput>,
}

/// A resource block: `resource "type" "label" { ... }`.
#[derive(Debug)]
pub struct HclResource {
    pub type_name: String,
    pub label: String,
    pub attributes: IndexMap<String, HclExpr>,
}

/// An output block: `output "name" { value = expr }`.
#[derive(Debug)]
pub struct HclOutput {
    pub name: String,
    pub value: HclExpr,
}

/// An expression in an HCL attribute value.
#[derive(Debug, Clone)]
pub enum HclExpr {
    Literal(HclValue),
    Reference(Reference),
    VarRef(String),
}

/// A concrete value.
#[derive(Debug, Clone)]
pub enum HclValue {
    String(String),
    Integer(i32),
    Bool(bool),
}

impl fmt::Display for HclValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HclValue::String(s) => write!(f, "\"{}\"", s),
            HclValue::Integer(i) => write!(f, "{}", i),
            HclValue::Bool(b) => write!(f, "{}", b),
        }
    }
}

/// A reference like `server.web1.ip` parsed into components.
#[derive(Debug, Clone)]
pub struct Reference {
    pub block_type: String,
    pub block_label: String,
    pub field: String,
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.block_type, self.block_label, self.field)
    }
}

/// Parse an `hcl::Body` into our intermediate `HclProgram`.
pub fn parse_hcl_body(body: &hcl::Body) -> Result<HclProgram, String> {
    let mut variables = HashMap::new();
    let mut resources = Vec::new();
    let mut outputs = Vec::new();

    for structure in body.iter() {
        match structure {
            hcl::Structure::Block(block) => {
                match block.identifier.as_str() {
                    "variable" => {
                        let name = block
                            .labels
                            .first()
                            .ok_or("variable block missing name label")?
                            .as_str()
                            .to_string();
                        let default_val = block
                            .body
                            .attributes()
                            .find(|a| a.key.as_str() == "default")
                            .ok_or_else(|| {
                                format!("variable '{}' missing 'default' attribute", name)
                            })?;
                        let value = parse_hcl_value(&default_val.expr)?;
                        variables.insert(name, value);
                    }
                    "resource" => {
                        if block.labels.len() < 2 {
                            return Err("resource block requires type and label".into());
                        }
                        let type_name = block.labels[0].as_str().to_string();
                        let label = block.labels[1].as_str().to_string();
                        let mut attributes = IndexMap::new();
                        for attr in block.body.attributes() {
                            let expr = parse_hcl_expr(&attr.expr)?;
                            attributes.insert(attr.key.as_str().to_string(), expr);
                        }
                        resources.push(HclResource {
                            type_name,
                            label,
                            attributes,
                        });
                    }
                    "output" => {
                        let name = block
                            .labels
                            .first()
                            .ok_or("output block missing name label")?
                            .as_str()
                            .to_string();
                        let value_attr = block
                            .body
                            .attributes()
                            .find(|a| a.key.as_str() == "value")
                            .ok_or_else(|| {
                                format!("output '{}' missing 'value' attribute", name)
                            })?;
                        let value = parse_hcl_expr(&value_attr.expr)?;
                        outputs.push(HclOutput { name, value });
                    }
                    other => {
                        return Err(format!("unsupported block type: '{}'", other));
                    }
                }
            }
            hcl::Structure::Attribute(_) => {
                // Top-level attributes are ignored for now.
            }
        }
    }

    Ok(HclProgram {
        variables,
        resources,
        outputs,
    })
}

/// Parse an HCL expression into our intermediate representation.
fn parse_hcl_expr(expr: &hcl::Expression) -> Result<HclExpr, String> {
    match expr {
        hcl::Expression::String(s) => Ok(HclExpr::Literal(HclValue::String(s.clone()))),
        hcl::Expression::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(HclExpr::Literal(HclValue::Integer(i as i32)))
            } else {
                Err(format!("unsupported number: {}", n))
            }
        }
        hcl::Expression::Bool(b) => Ok(HclExpr::Literal(HclValue::Bool(*b))),
        hcl::Expression::Variable(v) => {
            // A bare variable like `var` — likely a var reference prefix.
            // In HCL traversals are more common for references.
            Ok(HclExpr::VarRef(v.as_str().to_string()))
        }
        hcl::Expression::Traversal(traversal) => parse_traversal(traversal),
        other => Err(format!("unsupported expression: {:?}", other)),
    }
}

/// Parse an HCL traversal like `server.web1.ip` or `var.threshold`.
fn parse_traversal(traversal: &hcl::expr::Traversal) -> Result<HclExpr, String> {
    let root = match &traversal.expr {
        hcl::Expression::Variable(v) => v.as_str().to_string(),
        other => return Err(format!("unsupported traversal root: {:?}", other)),
    };

    let operators: Vec<String> = traversal
        .operators
        .iter()
        .filter_map(|op| match op {
            hcl::expr::TraversalOperator::GetAttr(ident) => Some(ident.as_str().to_string()),
            _ => None,
        })
        .collect();

    if root == "var" && operators.len() == 1 {
        return Ok(HclExpr::VarRef(operators[0].clone()));
    }

    if operators.len() == 2 {
        return Ok(HclExpr::Reference(Reference {
            block_type: root,
            block_label: operators[0].clone(),
            field: operators[1].clone(),
        }));
    }

    Err(format!(
        "unsupported traversal: {}.{}",
        root,
        operators.join(".")
    ))
}

/// Parse an HCL expression into a concrete value (for variable defaults).
fn parse_hcl_value(expr: &hcl::Expression) -> Result<HclValue, String> {
    match expr {
        hcl::Expression::String(s) => Ok(HclValue::String(s.clone())),
        hcl::Expression::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(HclValue::Integer(i as i32))
            } else {
                Err(format!("unsupported number value: {}", n))
            }
        }
        hcl::Expression::Bool(b) => Ok(HclValue::Bool(*b)),
        _ => Err(format!("variable default must be a literal, got: {:?}", expr)),
    }
}
