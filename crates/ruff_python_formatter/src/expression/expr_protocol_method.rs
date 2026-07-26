use ruff_python_ast::ExprProtocolMethod;
use ruff_text_size::Ranged;

use crate::prelude::*;
use crate::verbatim::verbatim_text;

#[derive(Default)]
pub struct FormatExprProtocolMethod;

impl FormatNodeRule<ExprProtocolMethod> for FormatExprProtocolMethod {
    fn fmt_fields(&self, item: &ExprProtocolMethod, f: &mut PyFormatter) -> FormatResult<()> {
        verbatim_text(item.range()).fmt(f)
    }
}
