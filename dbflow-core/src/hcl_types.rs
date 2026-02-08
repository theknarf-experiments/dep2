use indexmap::IndexMap;
use std::collections::HashMap;
use std::fmt;

/// A parsed HCL program containing all block types.
#[derive(Debug)]
pub struct HclProgram {
    pub variables: HashMap<String, HclValue>,
    pub resources: Vec<HclResource>,
    pub outputs: Vec<HclOutput>,
    pub modules: Vec<HclModule>,
    pub data_blocks: Vec<HclDataBlock>,
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

/// A module block: `module "instance_name" { source = "./path" ... }`.
#[derive(Debug)]
pub struct HclModule {
    pub instance_name: String,
    pub source: String,
    pub inputs: HashMap<String, HclExpr>,
}

/// Comparison operators for filter expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HclComparisonOp {
    Eq,
    NotEq,
    Less,
    LessEq,
    Greater,
    GreaterEq,
}

/// Aggregate operators for aggregate expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HclAggregateOp {
    Count,
    Sum,
    Min,
    Max,
}

/// Arithmetic operators for arithmetic expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HclArithmeticOp {
    Plus,
    Minus,
    Mul,
    Div,
    Mod,
}

/// An expression in an HCL attribute value.
#[derive(Debug, Clone)]
pub enum HclExpr {
    Literal(HclValue),
    Reference(Reference),
    /// A negated reference like `!server.w1.ip` — compiles to a NegatedAtomPredicate (antijoin).
    NegatedReference(Reference),
    VarRef(String),
    /// A reference to a data block field: `data.provider_type.label.field`.
    DataReference(DataRef),
    /// A comparison expression like `data.csv.orders.amount > 1000`.
    Comparison {
        lhs: Box<HclExpr>,
        operator: HclComparisonOp,
        rhs: Box<HclExpr>,
    },
    /// An aggregate expression like `sum(data.csv.sales.amount)`.
    Aggregate {
        operator: HclAggregateOp,
        argument: Box<HclExpr>,
    },
    /// An arithmetic expression like `data.csv.orders.amount + data.csv.orders.tax`.
    ArithmeticOp {
        lhs: Box<HclExpr>,
        operator: HclArithmeticOp,
        rhs: Box<HclExpr>,
    },
}

/// A reference to a data block field: `data.provider_type.label.field`.
#[derive(Debug, Clone)]
pub struct DataRef {
    pub provider_type: String,
    pub label: String,
    pub field: String,
}

/// A data block: `data "provider_type" "label" { config... }`.
#[derive(Debug, Clone)]
pub struct HclDataBlock {
    pub provider_type: String,
    pub label: String,
    pub config: HashMap<String, String>,
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
    let mut modules = Vec::new();
    let mut data_blocks = Vec::new();

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
                    "module" => {
                        let instance_name = block
                            .labels
                            .first()
                            .ok_or("module block missing instance name label")?
                            .as_str()
                            .to_string();
                        let source_attr = block
                            .body
                            .attributes()
                            .find(|a| a.key.as_str() == "source")
                            .ok_or_else(|| {
                                format!("module '{}' missing 'source' attribute", instance_name)
                            })?;
                        let source = match parse_hcl_value(&source_attr.expr)? {
                            HclValue::String(s) => s,
                            _ => return Err(format!(
                                "module '{}' source must be a string", instance_name
                            )),
                        };
                        let mut inputs = HashMap::new();
                        for attr in block.body.attributes() {
                            if attr.key.as_str() == "source" {
                                continue;
                            }
                            let expr = parse_hcl_expr(&attr.expr)?;
                            inputs.insert(attr.key.as_str().to_string(), expr);
                        }
                        modules.push(HclModule {
                            instance_name,
                            source,
                            inputs,
                        });
                    }
                    "data" => {
                        if block.labels.len() < 2 {
                            return Err(
                                "data block requires provider type and label".into(),
                            );
                        }
                        let provider_type = block.labels[0].as_str().to_string();
                        let label = block.labels[1].as_str().to_string();
                        let mut config = HashMap::new();
                        for attr in block.body.attributes() {
                            let val = parse_hcl_value(&attr.expr)?;
                            config.insert(
                                attr.key.as_str().to_string(),
                                hcl_value_to_string(&val),
                            );
                        }
                        data_blocks.push(HclDataBlock {
                            provider_type,
                            label,
                            config,
                        });
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
        modules,
        data_blocks,
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
        hcl::Expression::Operation(op) => {
            match op.as_ref() {
                hcl::expr::Operation::Unary(unary)
                    if unary.operator == hcl::expr::UnaryOperator::Not =>
                {
                    // `!ref` → NegatedReference
                    let inner = parse_hcl_expr(&unary.expr)?;
                    match inner {
                        HclExpr::Reference(r) => Ok(HclExpr::NegatedReference(r)),
                        _ => Err(format!(
                            "negation (!) can only be applied to a reference, got: {:?}",
                            unary.expr
                        )),
                    }
                }
                hcl::expr::Operation::Binary(binary) => {
                    let lhs = parse_hcl_expr(&binary.lhs_expr)?;
                    let rhs = parse_hcl_expr(&binary.rhs_expr)?;

                    // Check if it's a comparison operator.
                    let cmp_op = match binary.operator {
                        hcl::expr::BinaryOperator::Eq => Some(HclComparisonOp::Eq),
                        hcl::expr::BinaryOperator::NotEq => Some(HclComparisonOp::NotEq),
                        hcl::expr::BinaryOperator::Less => Some(HclComparisonOp::Less),
                        hcl::expr::BinaryOperator::LessEq => Some(HclComparisonOp::LessEq),
                        hcl::expr::BinaryOperator::Greater => Some(HclComparisonOp::Greater),
                        hcl::expr::BinaryOperator::GreaterEq => Some(HclComparisonOp::GreaterEq),
                        _ => None,
                    };
                    if let Some(op) = cmp_op {
                        return Ok(HclExpr::Comparison {
                            lhs: Box::new(lhs),
                            operator: op,
                            rhs: Box::new(rhs),
                        });
                    }

                    // Check if it's an arithmetic operator.
                    let arith_op = match binary.operator {
                        hcl::expr::BinaryOperator::Plus => Some(HclArithmeticOp::Plus),
                        hcl::expr::BinaryOperator::Minus => Some(HclArithmeticOp::Minus),
                        hcl::expr::BinaryOperator::Mul => Some(HclArithmeticOp::Mul),
                        hcl::expr::BinaryOperator::Div => Some(HclArithmeticOp::Div),
                        hcl::expr::BinaryOperator::Mod => Some(HclArithmeticOp::Mod),
                        _ => None,
                    };
                    if let Some(op) = arith_op {
                        return Ok(HclExpr::ArithmeticOp {
                            lhs: Box::new(lhs),
                            operator: op,
                            rhs: Box::new(rhs),
                        });
                    }

                    Err(format!("unsupported binary operator: {:?}", binary.operator))
                }
                _ => Err(format!("unsupported operation: {:?}", op)),
            }
        }
        hcl::Expression::FuncCall(func_call) => {
            let name = func_call.name.to_string();
            let agg_op = match name.as_str() {
                "count" => Some(HclAggregateOp::Count),
                "sum" => Some(HclAggregateOp::Sum),
                "min" => Some(HclAggregateOp::Min),
                "max" => Some(HclAggregateOp::Max),
                _ => None,
            };
            match agg_op {
                Some(op) => {
                    if func_call.args.len() != 1 {
                        return Err(format!(
                            "aggregate function '{}' requires exactly 1 argument, got {}",
                            name,
                            func_call.args.len()
                        ));
                    }
                    let arg = parse_hcl_expr(&func_call.args[0])?;
                    Ok(HclExpr::Aggregate {
                        operator: op,
                        argument: Box::new(arg),
                    })
                }
                None => Err(format!("unsupported function: '{}'", name)),
            }
        }
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

    // data.provider_type.label.field → DataReference
    if root == "data" && operators.len() == 3 {
        return Ok(HclExpr::DataReference(DataRef {
            provider_type: operators[0].clone(),
            label: operators[1].clone(),
            field: operators[2].clone(),
        }));
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

/// Convert an HCL value to a string for data block config.
fn hcl_value_to_string(val: &HclValue) -> String {
    match val {
        HclValue::String(s) => s.clone(),
        HclValue::Integer(i) => i.to_string(),
        HclValue::Bool(b) => b.to_string(),
    }
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
