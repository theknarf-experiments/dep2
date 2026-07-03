use catalog::arithmetic::ArithmeticPos;
use catalog::arithmetic::FactorPos;
use parsing::arithmetic::ArithmeticOperator;
use parsing::arithmetic::BuiltinOp;
use parsing::decl::DataType;
use parsing::rule::Const;
use std::fmt;

use crate::arguments::TransformationArgument;

// move from a factor signature (pos) enum (when parsing) -- if it is a signature, we replace by the transformation argument
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FactorArgument {
    Var(TransformationArgument),
    Const(Const),
    Builtin(BuiltinOp, Vec<FactorArgument>),
    /// A parenthesised sub-expression (its own arithmetic chain).
    Paren(Box<ArithmeticArgument>),
}

impl FactorArgument {
    /// Resolve a `FactorPos` to argument form, consuming `var_arguments` in
    /// left-to-right order (builtin args and parenthesised sub-expressions
    /// recurse first).
    fn from_factor_pos(
        factor: &FactorPos,
        var_arguments: &[TransformationArgument],
        var_id: &mut usize,
    ) -> Self {
        match factor {
            FactorPos::Var(_) => {
                let arg = &var_arguments[*var_id];
                *var_id += 1;
                FactorArgument::Var(*arg)
            }
            FactorPos::Const(constant) => FactorArgument::Const(constant.clone()),
            FactorPos::Builtin(op, args) => {
                let args = args
                    .iter()
                    .map(|a| FactorArgument::from_factor_pos(a, var_arguments, var_id))
                    .collect();
                FactorArgument::Builtin(*op, args)
            }
            FactorPos::Paren(inner) => FactorArgument::Paren(Box::new(
                ArithmeticArgument::from_arithmetic_shared(inner, var_arguments, var_id),
            )),
        }
    }

    pub fn transformation_arguments(&self) -> Vec<&TransformationArgument> {
        match self {
            FactorArgument::Var(transformation_arg) => vec![transformation_arg],
            FactorArgument::Const(_) => vec![],
            FactorArgument::Builtin(_, args) => args
                .iter()
                .flat_map(|a| a.transformation_arguments())
                .collect(),
            FactorArgument::Paren(inner) => inner.transformation_arguments(),
        }
    }

    fn jn_flip(&self) -> Self {
        match self {
            FactorArgument::Var(arg) => FactorArgument::Var(arg.jn_flip()),
            FactorArgument::Const(constant) => FactorArgument::Const(constant.clone()),
            FactorArgument::Builtin(op, args) => {
                FactorArgument::Builtin(*op, args.iter().map(|a| a.jn_flip()).collect())
            }
            FactorArgument::Paren(inner) => FactorArgument::Paren(Box::new(inner.jn_flip())),
        }
    }
}

impl fmt::Display for FactorArgument {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FactorArgument::Var(transformation_arg) => write!(f, "{}", transformation_arg),
            FactorArgument::Const(constant) => write!(f, "{}", constant),
            FactorArgument::Builtin(op, args) => {
                let args = args
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{}({})", op, args)
            }
            FactorArgument::Paren(inner) => write!(f, "({})", inner),
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ArithmeticArgument {
    init: FactorArgument,
    rest: Vec<(ArithmeticOperator, FactorArgument)>,
    data_type: DataType,
}

impl ArithmeticArgument {
    pub fn new(
        init: FactorArgument,
        rest: Vec<(ArithmeticOperator, FactorArgument)>,
        data_type: DataType,
    ) -> Self {
        Self {
            init,
            rest,
            data_type,
        }
    }

    pub fn from_arithmetic(
        arithmetic: &ArithmeticPos,
        var_arguments: &[TransformationArgument],
    ) -> Self {
        let mut var_id = 0;
        Self::from_arithmetic_shared(arithmetic, var_arguments, &mut var_id)
    }

    /// Like [`Self::from_arithmetic`] but threading the caller's argument
    /// cursor, so a parenthesised sub-expression consumes from the same
    /// left-to-right walk as its enclosing expression.
    fn from_arithmetic_shared(
        arithmetic: &ArithmeticPos,
        var_arguments: &[TransformationArgument],
        var_id: &mut usize,
    ) -> Self {
        let init = FactorArgument::from_factor_pos(arithmetic.init(), var_arguments, var_id);

        let rest = arithmetic
            .rest()
            .iter()
            .map(|(op, factor)| {
                let factor = FactorArgument::from_factor_pos(factor, var_arguments, var_id);
                (op.clone(), factor)
            })
            .collect();

        ArithmeticArgument {
            init,
            rest,
            data_type: *arithmetic.data_type(),
        }
    }

    pub fn init(&self) -> &FactorArgument {
        &self.init
    }

    pub fn rest(&self) -> &Vec<(ArithmeticOperator, FactorArgument)> {
        &self.rest
    }

    pub fn is_literal(&self) -> bool {
        self.rest.is_empty()
    }

    pub fn data_type(&self) -> &DataType {
        &self.data_type
    }

    pub fn transformation_arguments(&self) -> Vec<&TransformationArgument> {
        let mut transformation_arguments = self.init.transformation_arguments();
        for (_, factor) in &self.rest {
            transformation_arguments.extend(factor.transformation_arguments());
        }
        transformation_arguments
    }

    // flip the underlying transformation arguments
    pub fn jn_flip(&self) -> Self {
        let init = self.init.jn_flip();

        let rest = self
            .rest
            .iter()
            .map(|(op, factor)| (op.clone(), factor.jn_flip()))
            .collect();

        ArithmeticArgument {
            init,
            rest,
            data_type: self.data_type,
        }
    }
}

impl fmt::Display for ArithmeticArgument {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.init)?;
        for (op, factor) in &self.rest {
            write!(f, " {} {}", op, factor)?;
        }
        Ok(())
    }
}
