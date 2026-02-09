use std::collections::HashMap;
use std::sync::Mutex;

use parsing::decl::DataType;

use super::error::CompileError;
use crate::hcl_types::{HclExpr, HclValue};
use crate::reference::DependencyAnalysis;

/// Bidirectional string interning table.
/// Maps string values to unique i32 identifiers for FlowLog execution.
#[derive(Debug, Default, Clone)]
pub struct StringTable {
    pub(crate) str_to_id: HashMap<String, i32>,
    pub(crate) id_to_str: Vec<String>,
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
    pub program: parsing::parser::Program,
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
pub(crate) fn convert_data_type(dt: &dbflow_plugin::DataType) -> DataType {
    match dt {
        dbflow_plugin::DataType::String => DataType::String,
        dbflow_plugin::DataType::Integer => DataType::Integer,
    }
}

/// Convert a plugin `DataValue` to an i32 for fact encoding.
pub(crate) fn data_value_to_i32(
    val: &dbflow_plugin::DataValue,
    st: &mut StringTable,
) -> Result<i32, CompileError> {
    match val {
        dbflow_plugin::DataValue::String(s) => Ok(st.intern(s)),
        dbflow_plugin::DataValue::Integer(i) => {
            if *i < i32::MIN as i64 || *i > i32::MAX as i64 {
                Err(CompileError::IntegerOverflow(*i))
            } else {
                Ok(*i as i32)
            }
        }
        dbflow_plugin::DataValue::Bool(b) => Ok(if *b { 1 } else { 0 }),
        dbflow_plugin::DataValue::Null => Ok(st.intern("__null__")),
    }
}

/// Convert an `HclValue` to an i32 for fact encoding.
pub(crate) fn value_to_i32(val: &HclValue, st: &mut StringTable) -> i32 {
    match val {
        HclValue::Integer(i) => *i,
        HclValue::String(s) => st.intern(s),
        HclValue::Bool(b) => {
            if *b {
                1
            } else {
                0
            }
        }
    }
}

/// Infer a FlowLog `DataType` from an HCL expression.
pub(crate) fn infer_data_type(expr: &HclExpr) -> DataType {
    match expr {
        HclExpr::Literal(HclValue::Integer(_)) => DataType::Integer,
        HclExpr::Literal(HclValue::String(_)) => DataType::String,
        HclExpr::Literal(HclValue::Bool(_)) => DataType::Integer, // bools as 0/1
        HclExpr::Reference(_)
        | HclExpr::NegatedReference(_)
        | HclExpr::VarRef(_)
        | HclExpr::DataReference(_) => DataType::String,
        HclExpr::Comparison { .. } | HclExpr::Aggregate { .. } | HclExpr::ArithmeticOp { .. } => {
            DataType::Integer
        }
    }
}
