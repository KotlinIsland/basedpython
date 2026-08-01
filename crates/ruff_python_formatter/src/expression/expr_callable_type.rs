use ruff_python_ast::ExprCallableType;

use crate::expression::format_verbatim_expr;
use crate::prelude::*;

#[derive(Default)]
pub struct FormatExprCallableType;

impl FormatNodeRule<ExprCallableType> for FormatExprCallableType {
    fn fmt_fields(&self, item: &ExprCallableType, f: &mut PyFormatter) -> FormatResult<()> {
        format_verbatim_expr(item, f)
    }
}
