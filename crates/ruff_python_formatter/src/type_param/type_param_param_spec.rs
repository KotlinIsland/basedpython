use ruff_formatter::write;
use ruff_python_ast::TypeParamParamSpec;

use crate::prelude::*;

#[derive(Default)]
pub struct FormatTypeParamParamSpec;

impl FormatNodeRule<TypeParamParamSpec> for FormatTypeParamParamSpec {
    fn fmt_fields(&self, item: &TypeParamParamSpec, f: &mut PyFormatter) -> FormatResult<()> {
        let TypeParamParamSpec {
            range: _,
            node_index: _,
            name,
            bound,
            default,
            is_reified,
        } = item;
        // basedpython writes `reified` ahead of the pack's own `**`
        if *is_reified && f.options().is_basedpython() {
            write!(f, [token("reified"), space()])?;
        }
        write!(f, [token("**"), name.format()])?;
        if let Some(bound) = bound {
            write!(f, [token(":"), space(), bound.format()])?;
        }
        if let Some(default) = default {
            write!(f, [space(), token("="), space(), default.format()])?;
        }
        Ok(())
    }
}
