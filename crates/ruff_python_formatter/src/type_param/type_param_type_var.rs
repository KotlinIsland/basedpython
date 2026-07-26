use ruff_formatter::write;
use ruff_python_ast::{Expr, TypeParamTypeVar, Variance};

use crate::prelude::*;

#[derive(Default)]
pub struct FormatTypeParamTypeVar;

impl FormatNodeRule<TypeParamTypeVar> for FormatTypeParamTypeVar {
    fn fmt_fields(&self, item: &TypeParamTypeVar, f: &mut PyFormatter) -> FormatResult<()> {
        let TypeParamTypeVar {
            range: _,
            node_index: _,
            name,
            lower_bound,
            bound,
            default,
            variance,
        } = item;
        // basedpython variance keywords precede the typevar name. plain
        // python output ignores them — they're only emitted in `.by`/`.byi`
        if f.options().is_basedpython() {
            match variance {
                Some(Variance::Covariant) => write!(f, [token("out"), space()])?,
                Some(Variance::Contravariant) => write!(f, [token("in"), space()])?,
                Some(Variance::Invariant) => {
                    write!(f, [token("in"), space(), token("out"), space()])?;
                }
                None => {}
            }
        }
        name.format().fmt(f)?;
        if let Some(bound) = bound {
            // in basedpython .by files, `constraints (int, str)` is keyword syntax —
            // preserve the space between `constraints` and `(` to distinguish it from a call
            let is_constraints_call = f.options().is_basedpython()
                && matches!(bound.as_ref(), Expr::Call(call)
                    if call.func.as_name_expr().is_some_and(|n| n.id == "constraints"));
            if let Some(lower_bound) = lower_bound {
                write!(
                    f,
                    [
                        token(":"),
                        space(),
                        lower_bound.format(),
                        token(".."),
                        bound.format()
                    ]
                )?;
            } else if is_constraints_call {
                write!(
                    f,
                    [
                        token(":"),
                        space(),
                        token("constraints"),
                        space(),
                        bound.as_call_expr().unwrap().arguments.format()
                    ]
                )?;
            } else {
                write!(f, [token(":"), space(), bound.format()])?;
            }
        }
        if let Some(default) = default {
            write!(f, [space(), token("="), space(), default.format()])?;
        }
        Ok(())
    }
}
