use crate::atoms::AtomArgumentSignature;
use parsing::arithmetic::Arithmetic;
use parsing::arithmetic::ArithmeticOperator;
use parsing::arithmetic::BuiltinOp;
use parsing::arithmetic::Factor;
use parsing::decl::DataType;
use parsing::rule::Const;
use std::fmt;

// move from a factor enum (when parsing) -- if it is a variable, we get its argument signature
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FactorPos {
    Var(AtomArgumentSignature),
    Const(Const),
    Builtin(BuiltinOp, Vec<FactorPos>),
}

impl FactorPos {
    /// Resolve a parsed `Factor` to positional form, consuming `var_signatures`
    /// in left-to-right order (builtin args recurse first), matching `Factor::vars`.
    fn from_factor(
        factor: &Factor,
        var_signatures: &[AtomArgumentSignature],
        var_id: &mut usize,
    ) -> Self {
        match factor {
            Factor::Var(_) => {
                let sig = &var_signatures[*var_id];
                *var_id += 1;
                FactorPos::Var(*sig)
            }
            Factor::Const(constant) => FactorPos::Const(constant.clone()),
            Factor::Builtin(op, args) => {
                let args = args
                    .iter()
                    .map(|a| FactorPos::from_factor(a, var_signatures, var_id))
                    .collect();
                FactorPos::Builtin(*op, args)
            }
        }
    }

    pub fn signatures(&self) -> Vec<&AtomArgumentSignature> {
        match self {
            FactorPos::Var(atom_arg_signature) => vec![atom_arg_signature],
            FactorPos::Const(_) => vec![],
            FactorPos::Builtin(_, args) => args.iter().flat_map(|a| a.signatures()).collect(),
        }
    }
}

impl fmt::Display for FactorPos {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FactorPos::Var(atom_arg_signature) => write!(f, "{}", atom_arg_signature),
            FactorPos::Const(constant) => write!(f, "{}", constant),
            FactorPos::Builtin(op, args) => {
                let args = args
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{}({})", op, args)
            }
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ArithmeticPos {
    init: FactorPos,
    rest: Vec<(ArithmeticOperator, FactorPos)>,
    data_type: DataType,
}

impl ArithmeticPos {
    pub fn from_arithmetic(
        arithmetic: &Arithmetic,
        var_signatures: &[AtomArgumentSignature],
    ) -> Self {
        let mut var_id = 0;

        let init = FactorPos::from_factor(arithmetic.init(), var_signatures, &mut var_id);

        let rest = arithmetic
            .rest()
            .iter()
            .map(|(op, factor)| {
                let factor = FactorPos::from_factor(factor, var_signatures, &mut var_id);
                (op.clone(), factor)
            })
            .collect();

        ArithmeticPos {
            init,
            rest,
            data_type: *arithmetic.data_type(),
        }
    }

    pub fn init(&self) -> &FactorPos {
        &self.init
    }

    pub fn rest(&self) -> &[(ArithmeticOperator, FactorPos)] {
        &self.rest
    }

    pub fn is_literal(&self) -> bool {
        self.rest.is_empty()
    }

    pub fn is_var(&self) -> bool {
        self.is_literal() && matches!(self.init, FactorPos::Var(_))
    }

    pub fn signatures(&self) -> Vec<&AtomArgumentSignature> {
        let mut signatures = self.init.signatures();
        for (_, factor) in &self.rest {
            signatures.extend(factor.signatures());
        }
        signatures
    }

    pub fn data_type(&self) -> &DataType {
        &self.data_type
    }
}

impl fmt::Display for ArithmeticPos {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.init)?;
        for (op, factor) in &self.rest {
            write!(f, " {} {}", op, factor)?;
        }
        Ok(())
    }
}
