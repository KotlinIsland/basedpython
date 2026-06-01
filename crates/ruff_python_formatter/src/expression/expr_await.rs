use ruff_formatter::write;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::{Expr, ExprAwait};
use ruff_text_size::Ranged;

use crate::expression::maybe_parenthesize_expression;
use crate::expression::parentheses::{
    NeedsParentheses, OptionalParentheses, Parenthesize, is_expression_parenthesized,
    is_type_annotation_of,
};
use crate::prelude::*;

#[derive(Default)]
pub struct FormatExprAwait;

impl FormatNodeRule<ExprAwait> for FormatExprAwait {
    fn fmt_fields(&self, item: &ExprAwait, f: &mut PyFormatter) -> FormatResult<()> {
        let ExprAwait {
            range: _,
            node_index: _,
            value,
            postfix,
        } = item;

        // basedpython postfix `.await` binds like attribute access and renders
        // after its operand (`g().await`). the operand must be parenthesised
        // unless it is itself a trailer chain, so `(a + b).await` keeps its
        // parens rather than becoming `a + b.await`
        if *postfix {
            let trailer = matches!(
                value.as_ref(),
                Expr::Name(_)
                    | Expr::Call(_)
                    | Expr::Attribute(_)
                    | Expr::Subscript(_)
                    | Expr::Await(_)
            );
            if trailer {
                return write!(f, [value.format(), token(".await")]);
            }
            return write!(f, [token("("), value.format(), token(")"), token(".await")]);
        }

        write!(
            f,
            [
                token("await"),
                space(),
                maybe_parenthesize_expression(value, item, Parenthesize::IfBreaks)
            ]
        )
    }
}

impl NeedsParentheses for ExprAwait {
    fn needs_parentheses(
        &self,
        parent: AnyNodeRef,
        context: &PyFormatContext,
    ) -> OptionalParentheses {
        if parent.is_expr_await() || is_type_annotation_of(self.range(), parent) {
            OptionalParentheses::Always
        } else if is_expression_parenthesized(
            self.value.as_ref().into(),
            context.comments().ranges(),
            context.source(),
        ) {
            // Prefer splitting the value if it is parenthesized.
            OptionalParentheses::Never
        } else {
            self.value.needs_parentheses(self.into(), context)
        }
    }
}
