use std::collections::HashMap;
use std::sync::Mutex;

use parsing::decl::{DataType, NULL_SENTINEL};


use crate::hcl_types::{HclExpr, HclValue};
use crate::reference::DependencyAnalysis;

/// Bidirectional string interning table.
/// Maps string values to unique i64 identifiers for FlowLog execution.
#[derive(Debug, Default, Clone)]
pub struct StringTable {
    pub(crate) str_to_id: HashMap<String, i64>,
    pub(crate) id_to_str: Vec<String>,
}

impl StringTable {
    pub fn intern(&mut self, s: &str) -> i64 {
        if let Some(&id) = self.str_to_id.get(s) {
            return id;
        }
        let id = self.id_to_str.len() as i64;
        self.id_to_str.push(s.to_string());
        self.str_to_id.insert(s.to_string(), id);
        id
    }

    pub fn decode(&self, id: i64) -> Option<&str> {
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

    pub fn intern(&self, s: &str) -> i64 {
        self.inner.lock().unwrap().intern(s)
    }

    pub fn decode(&self, id: i64) -> Option<String> {
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

/// A built-in scalar function kind.
#[derive(Debug, Clone)]
pub enum ScalarFnKind {
    Neg,
    Abs,
    Sign,
    Floor,
    Ceil,
    Round,
    Sqrt,
}

impl ScalarFnKind {
    /// Returns true if this function operates on float-encoded i64 values.
    pub fn is_float_function(&self) -> bool {
        matches!(
            self,
            ScalarFnKind::Floor | ScalarFnKind::Ceil | ScalarFnKind::Round | ScalarFnKind::Sqrt
        )
    }

    /// Parse a function name into a ScalarFnKind, if recognized.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "neg" => Some(ScalarFnKind::Neg),
            "abs" => Some(ScalarFnKind::Abs),
            "sign" => Some(ScalarFnKind::Sign),
            "floor" => Some(ScalarFnKind::Floor),
            "ceil" => Some(ScalarFnKind::Ceil),
            "round" => Some(ScalarFnKind::Round),
            "sqrt" => Some(ScalarFnKind::Sqrt),
            _ => None,
        }
    }
}

/// Describes an auxiliary EDB that precomputes a scalar function for streaming data.
/// For each value `x` arriving in `source_edb_name` at column `input_col_idx`,
/// the engine encoding thread sends `(x, f(x))` to the `fn_edb_name` channel.
#[derive(Debug, Clone)]
pub struct StreamingFnEdb {
    /// Name of the auxiliary EDB (e.g., `_fn_neg_negated_all_0`).
    pub fn_edb_name: String,
    /// Name of the source data EDB (e.g., `_data_csv_nums`).
    pub source_edb_name: String,
    /// Column index within the source EDB row to use as function input.
    pub input_col_idx: usize,
    /// Which scalar function to apply.
    pub function: ScalarFnKind,
}

/// Result of compiling an HCL program.
pub struct CompileResult {
    pub program: parsing::parser::Program,
    pub string_table: StringTable,
    pub analysis: DependencyAnalysis,
    /// For each EDB relation name, the list of fact tuples (as i64 vectors).
    pub edb_facts: HashMap<String, Vec<Vec<i64>>>,
    /// Metadata about output blocks for post-execution display.
    pub outputs: Vec<OutputInfo>,
    /// Names of EDB relations that will be populated at runtime via streaming.
    pub streaming_edbs: Vec<String>,
    /// Auxiliary function EDBs that need runtime computation for streaming sources.
    pub streaming_fn_edbs: Vec<StreamingFnEdb>,
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
        dbflow_plugin::DataType::Float => DataType::Float,
    }
}

/// Convert a plugin `DataValue` to an i64 for fact encoding.
pub(crate) fn data_value_to_i64(
    val: &dbflow_plugin::DataValue,
    st: &mut StringTable,
) -> i64 {
    match val {
        dbflow_plugin::DataValue::String(s) => st.intern(s),
        dbflow_plugin::DataValue::Integer(i) => *i,
        dbflow_plugin::DataValue::Float(f) => {
            let bits = f.to_bits() as i64;
            // Safety: if the bit pattern collides with NULL_SENTINEL, nudge it.
            if bits == NULL_SENTINEL {
                NULL_SENTINEL + 1
            } else {
                bits
            }
        }
        dbflow_plugin::DataValue::Bool(b) => if *b { 1 } else { 0 },
        dbflow_plugin::DataValue::Null => NULL_SENTINEL,
    }
}

/// Convert an `HclValue` to an i64 for fact encoding.
pub(crate) fn value_to_i64(val: &HclValue, st: &mut StringTable) -> i64 {
    match val {
        HclValue::Integer(i) => *i,
        HclValue::Float(f) => {
            let bits = f.to_bits() as i64;
            if bits == NULL_SENTINEL {
                NULL_SENTINEL + 1
            } else {
                bits
            }
        }
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

/// Infer a FlowLog `DataType` from an HCL expression using optional schema context.
/// When `data_col_types` is provided, DataReferences resolve to their actual column types.
pub(crate) fn infer_data_type_with_context(
    expr: &HclExpr,
    data_col_types: &std::collections::HashMap<(String, String), Vec<(String, DataType)>>,
    resource_map: &std::collections::HashMap<(&str, &str), &crate::hcl_types::HclResource>,
) -> DataType {
    infer_data_type_with_context_depth(expr, data_col_types, resource_map, 0)
}

/// Max recursion depth for following resource references during type inference.
const MAX_TYPE_INFER_DEPTH: usize = 10;

fn infer_data_type_with_context_depth(
    expr: &HclExpr,
    data_col_types: &std::collections::HashMap<(String, String), Vec<(String, DataType)>>,
    resource_map: &std::collections::HashMap<(&str, &str), &crate::hcl_types::HclResource>,
    depth: usize,
) -> DataType {
    if depth > MAX_TYPE_INFER_DEPTH {
        return DataType::String;
    }
    match expr {
        HclExpr::Literal(HclValue::Integer(_)) => DataType::Integer,
        HclExpr::Literal(HclValue::Float(_)) => DataType::Float,
        HclExpr::Literal(HclValue::String(_)) => DataType::String,
        HclExpr::Literal(HclValue::Bool(_)) => DataType::Integer,
        HclExpr::DataReference(dr) => {
            let key = (dr.provider_type.clone(), dr.label.clone());
            data_col_types
                .get(&key)
                .and_then(|cols| {
                    cols.iter()
                        .find(|(name, _)| name == &dr.field)
                        .map(|(_, dt)| *dt)
                })
                .unwrap_or(DataType::String)
        }
        HclExpr::Reference(r) => {
            resource_map
                .get(&(r.block_type.as_str(), r.block_label.as_str()))
                .and_then(|res| res.attributes.get(&r.field))
                .map(|e| infer_data_type_with_context_depth(e, data_col_types, resource_map, depth + 1))
                .unwrap_or(DataType::String)
        }
        HclExpr::Aggregate { operator, argument } => {
            match operator {
                crate::hcl_types::HclAggregateOp::Count => DataType::Integer,
                _ => infer_data_type_with_context_depth(argument, data_col_types, resource_map, depth + 1),
            }
        }
        HclExpr::ArithmeticOp { lhs, rhs, .. } => {
            let lt = infer_data_type_with_context_depth(lhs, data_col_types, resource_map, depth + 1);
            let rt = infer_data_type_with_context_depth(rhs, data_col_types, resource_map, depth + 1);
            if lt == DataType::Float || rt == DataType::Float {
                DataType::Float
            } else {
                DataType::Integer
            }
        }
        HclExpr::FunctionCall { name, args } => {
            let kind = ScalarFnKind::from_name(name);
            match kind {
                Some(k) if k.is_float_function() => DataType::Float,
                Some(_) => {
                    if let Some(arg) = args.first() {
                        match infer_data_type_with_context_depth(arg, data_col_types, resource_map, depth + 1) {
                            DataType::Float => DataType::Float,
                            _ => DataType::Integer,
                        }
                    } else {
                        DataType::Integer
                    }
                }
                None => DataType::Integer,
            }
        }
        _ => DataType::String,
    }
}

/// Infer a FlowLog `DataType` from an HCL expression.
pub(crate) fn infer_data_type(expr: &HclExpr) -> DataType {
    match expr {
        HclExpr::Literal(HclValue::Integer(_)) => DataType::Integer,
        HclExpr::Literal(HclValue::Float(_)) => DataType::Float,
        HclExpr::Literal(HclValue::String(_)) => DataType::String,
        HclExpr::Literal(HclValue::Bool(_)) => DataType::Integer, // bools as 0/1
        HclExpr::Reference(_)
        | HclExpr::NegatedReference(_)
        | HclExpr::VarRef(_)
        | HclExpr::DataReference(_) => DataType::String,
        HclExpr::Comparison { .. } | HclExpr::ArithmeticOp { .. } => DataType::Integer,
        HclExpr::Aggregate { operator, argument } => {
            // Count always returns Integer. For Sum/Min/Max, propagate
            // the argument's type (Float if the argument is Float).
            match operator {
                crate::hcl_types::HclAggregateOp::Count => DataType::Integer,
                _ => infer_data_type(argument),
            }
        }
        HclExpr::FunctionCall { name, args } => {
            // Float-only functions always return Float.
            // Integer-preserving functions (neg, abs, sign) return Integer unless
            // the argument is a known Float expression. References default to Integer
            // here since scalar functions are numeric operations.
            let kind = ScalarFnKind::from_name(name);
            match kind {
                Some(k) if k.is_float_function() => DataType::Float,
                Some(_) => {
                    if let Some(arg) = args.first() {
                        match infer_data_type(arg) {
                            DataType::Float => DataType::Float,
                            _ => DataType::Integer,
                        }
                    } else {
                        DataType::Integer
                    }
                }
                None => DataType::Integer,
            }
        }
    }
}
