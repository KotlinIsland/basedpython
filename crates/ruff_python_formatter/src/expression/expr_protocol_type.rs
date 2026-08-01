use ruff_python_ast::ExprProtocolType;

use crate::prelude::*;
use crate::expression::format_verbatim_expr;

#[derive(Default)]
pub struct FormatExprProtocolType;

impl FormatNodeRule<ExprProtocolType> for FormatExprProtocolType {
    fn fmt_fields(&self, item: &ExprProtocolType, f: &mut PyFormatter) -> FormatResult<()> {
        format_verbatim_expr(item, f)
    }
}
