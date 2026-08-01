use ruff_python_ast::ExprProtocolMethod;

use crate::expression::format_verbatim_expr;
use crate::prelude::*;

#[derive(Default)]
pub struct FormatExprProtocolMethod;

impl FormatNodeRule<ExprProtocolMethod> for FormatExprProtocolMethod {
    fn fmt_fields(&self, item: &ExprProtocolMethod, f: &mut PyFormatter) -> FormatResult<()> {
        format_verbatim_expr(item, f)
    }
}
