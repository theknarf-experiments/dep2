/*
    DataType: number | string
    Attribute: <name>: <DataType>
    RelDecl: <name>(<Attribute>, <Attribute>, ...)
*/

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Integer,
    String,
    Float,
}

/// Sentinel value representing NULL. Uses `i64::MIN` which is unreachable by
/// the string table (starts at 0) and for floats decodes to -0.0 (remapped at encoding).
pub const NULL_SENTINEL: i64 = i64::MIN;

/// Check whether a value is the null sentinel.
pub fn is_null(v: i64) -> bool {
    v == NULL_SENTINEL
}

impl DataType {
    pub fn parse_from(type_str: &str) -> Self {
        match type_str {
            "number" => Self::Integer,
            "string" => Self::String,
            "float" => Self::Float,
            _ => unreachable!(),
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer => write!(f, "number"), // f :: a formatter that can be used to write to a buffer
            Self::String => write!(f, "string"),
            Self::Float => write!(f, "float"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Attribute {
    name: String,
    data_type: DataType,
}

impl fmt::Display for Attribute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.name, self.data_type)
    }
}

impl Attribute {
    pub fn new(name: &str, data_type: DataType) -> Self {
        Self {
            name: name.to_string(),
            data_type,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn data_type(&self) -> &DataType {
        &self.data_type
    }
}

#[derive(Debug, Clone)]
pub struct RelDecl {
    name: String,
    attributes: Vec<Attribute>,
    path: Option<String>,
    /// Declared under `.out` (force-serve over the query API even if the relation
    /// is consumed by another rule). `.printsize` relations default to false.
    force_serve: bool,
}

impl fmt::Display for RelDecl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}({})",
            self.name,
            self.attributes
                .iter()
                .map(|attr| attr.to_string()) // to_string() uses the Display impl for Attribute
                .collect::<Vec<String>>()
                .join(", ")
        )?;
        if let Some(ref path) = self.path {
            write!(f, " read as {}", path)?;
        }
        Ok(())
    }
}

impl RelDecl {
    pub fn new(name: &str, attributes: Vec<Attribute>, path: Option<&str>) -> Self {
        Self {
            name: name.to_string(),
            attributes,
            path: path.map(|p| p.to_string()),
            force_serve: false,
        }
    }

    pub fn push_attr(&mut self, attr: Attribute) {
        self.attributes.push(attr);
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn attributes(&self) -> &[Attribute] {
        &self.attributes
    }

    pub fn arity(&self) -> usize {
        self.attributes.len()
    }

    pub fn path(&self) -> Option<String> {
        self.path.clone()
    }

    pub fn force_serve(&self) -> bool {
        self.force_serve
    }

    pub fn set_force_serve(&mut self, force_serve: bool) {
        self.force_serve = force_serve;
    }
}
