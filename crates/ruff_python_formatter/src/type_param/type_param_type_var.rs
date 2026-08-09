use ruff_formatter::write;
use ruff_python_ast::{TypeParamTypeVar, Variance};

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
            is_type_mapping,
            default,
            variance,
            is_reified,
            is_some_hole: _,
        } = item;
        // basedpython writes `reified` and the variance keywords ahead of the
        // typevar name. plain python output ignores them — they're only
        // emitted in `.by`/`.byi`
        if f.options().is_basedpython() {
            if *is_reified {
                write!(f, [token("reified"), space()])?;
            }
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
            } else if *is_type_mapping {
                // `T in (int, str)` ranges over a type mapping. plain python has no such
                // spelling — there, the same tuple after a `:` already means constraints
                if f.options().is_basedpython() {
                    write!(f, [space(), token("in"), space(), bound.format()])?;
                } else {
                    write!(f, [token(":"), space(), bound.format()])?;
                }
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
