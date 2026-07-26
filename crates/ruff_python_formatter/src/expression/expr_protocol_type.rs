use ruff_python_ast::ExprProtocolType;
use ruff_text_size::Ranged;

use crate::prelude::*;
use crate::verbatim::verbatim_text;

#[derive(Default)]
pub struct FormatExprProtocolType;

impl FormatNodeRule<ExprProtocolType> for FormatExprProtocolType {
    fn fmt_fields(&self, item: &ExprProtocolType, f: &mut PyFormatter) -> FormatResult<()> {
        verbatim_text(item.range()).fmt(f)
    }
}
