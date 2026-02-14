use std::fmt;

/// Structured error type for the HCL-to-FlowLog compiler.
#[derive(Debug)]
pub enum CompileError {
    /// Module blocks require a base path for source resolution.
    MissingBasePath,
    /// Error during module expansion.
    Module(String),
    /// An EDB block contains an expression that is not allowed (e.g., comparison, aggregate).
    InvalidEdbExpr {
        type_name: String,
        label: String,
        detail: String,
    },
    /// A reference to an unknown type, field, or data block.
    UnknownReference {
        context: String,
        reference: String,
    },
    /// A required attribute is missing from a resource block.
    MissingAttribute {
        type_name: String,
        label: String,
        attribute: String,
    },
    /// An unresolved variable reference remains after variable substitution.
    UnresolvedVariable {
        context: String,
        var_name: String,
    },
    /// An expression is used in a context where it is not valid.
    InvalidExprContext {
        context: String,
        expr_kind: String,
    },
    /// A negated reference appears within a recursive (strongly connected) component.
    /// This violates stratified negation semantics and would produce undefined results.
    NegationInRecursion {
        block_type: String,
        block_label: String,
        negated_type: String,
        negated_label: String,
    },
    /// Two or more output blocks share the same name.
    DuplicateOutput {
        name: String,
    },
    /// An invalid expression in an arithmetic or comparison context.
    InvalidArithmeticExpr(String),
    /// Internal compiler error (should not happen in well-formed programs).
    Internal(String),
    /// I/O error (e.g., writing facts files).
    Io(std::io::Error),
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompileError::MissingBasePath => {
                write!(f, "module blocks require a base path for source resolution")
            }
            CompileError::Module(msg) => write!(f, "{}", msg),
            CompileError::InvalidEdbExpr {
                type_name,
                label,
                detail,
            } => write!(f, "EDB block {}.{} {}", type_name, label, detail),
            CompileError::UnknownReference { context, reference } => {
                write!(f, "{} references unknown {}", context, reference)
            }
            CompileError::MissingAttribute {
                type_name,
                label,
                attribute,
            } => write!(
                f,
                "resource {}.{} missing attribute '{}'",
                type_name, label, attribute
            ),
            CompileError::UnresolvedVariable { context, var_name } => {
                write!(
                    f,
                    "{} has unresolved variable reference 'var.{}'",
                    context, var_name
                )
            }
            CompileError::InvalidExprContext { context, expr_kind } => {
                write!(
                    f,
                    "{} cannot use {} as its value",
                    context, expr_kind
                )
            }
            CompileError::NegationInRecursion {
                block_type,
                block_label,
                negated_type,
                negated_label,
            } => write!(
                f,
                "resource {}.{} uses negation on {}.{} within a recursive component \
                 (stratified negation violation)",
                block_type, block_label, negated_type, negated_label
            ),
            CompileError::DuplicateOutput { name } => {
                write!(
                    f,
                    "duplicate output name '{}': each output must have a unique name",
                    name
                )
            }
            CompileError::InvalidArithmeticExpr(msg) => write!(f, "{}", msg),
            CompileError::Internal(msg) => write!(f, "internal error: {}", msg),
            CompileError::Io(err) => write!(f, "{}", err),
        }
    }
}

impl std::error::Error for CompileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CompileError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CompileError {
    fn from(err: std::io::Error) -> Self {
        CompileError::Io(err)
    }
}
