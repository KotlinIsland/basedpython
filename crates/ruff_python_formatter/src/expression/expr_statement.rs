use ruff_python_ast::{AnyNodeRef, ExprStatement};
use ruff_text_size::Ranged;

use crate::expression::parentheses::{NeedsParentheses, OptionalParentheses};
use crate::prelude::*;
use crate::verbatim::verbatim_text;

#[derive(Default)]
pub struct FormatExprStatement;

impl FormatNodeRule<ExprStatement> for FormatExprStatement {
    fn fmt_fields(&self, item: &ExprStatement, f: &mut PyFormatter) -> FormatResult<()> {
        // a statement expression spans an indented suite, which the expression
        // formatter has no layout for. reproducing the source keeps the suite
        // and its comments intact
        verbatim_text(item.range()).fmt(f)?;
        f.context()
            .comments()
            .mark_verbatim_node_comments_formatted(item.into());
        Ok(())
    }
}

impl NeedsParentheses for ExprStatement {
    fn needs_parentheses(
        &self,
        _parent: AnyNodeRef,
        _context: &PyFormatContext,
    ) -> OptionalParentheses {
        // a suite cannot be wrapped in parentheses
        OptionalParentheses::Never
    }
}
