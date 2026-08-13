use ruff_formatter::FormatRuleWithOptions;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::{Expr, ExprCall};
use ruff_text_size::Ranged;

use crate::comments::dangling_comments;
use crate::expression::CallChainLayout;
use crate::expression::parentheses::{NeedsParentheses, OptionalParentheses, Parentheses};
use crate::prelude::*;

#[derive(Default)]
pub struct FormatExprCall {
    call_chain_layout: CallChainLayout,
}

impl FormatRuleWithOptions<ExprCall, PyFormatContext<'_>> for FormatExprCall {
    type Options = CallChainLayout;

    fn with_options(mut self, options: Self::Options) -> Self {
        self.call_chain_layout = options;
        self
    }
}

impl FormatNodeRule<ExprCall> for FormatExprCall {
    fn fmt_fields(&self, item: &ExprCall, f: &mut PyFormatter) -> FormatResult<()> {
        let ExprCall {
            range_start: _,
            node_index: _,
            is_cast,
            is_checked_cast,
            is_string_tag,
            func,
            arguments,
        } = item;

        // basedpython custom string tags parse as a `tag(t"...")` call, but their
        // surface form glues the template straight onto the tag name. render the
        // template from source — it carries no `t` prefix there — or the tag is
        // rewritten into a t-string call that the surface syntax never had
        if *is_string_tag && let [template] = arguments.args.as_ref() {
            func.format().fmt(f)?;
            return source_text_slice(template.range()).fmt(f);
        }

        // basedpython casts parse as a synthetic `cast(<type>, <value>)` call
        // but their surface form is the infix `<value> cast <type>` (checked)
        // or `<value> cast? <type>` (safe). render that back rather than the
        // call, or the surface keyword — and, for `cast?`, its semantics — are
        // lost on reformat
        if (*is_cast || *is_checked_cast)
            && let [type_arg, value_arg] = arguments.args.as_ref()
        {
            value_arg.format().fmt(f)?;
            space().fmt(f)?;
            token("cast").fmt(f)?;
            if *is_checked_cast {
                token("?").fmt(f)?;
            }
            space().fmt(f)?;
            return type_arg.format().fmt(f);
        }

        let comments = f.context().comments().clone();
        let dangling = comments.dangling(item);

        let call_chain_layout = self.call_chain_layout.apply_in_node(item, f);

        let fmt_func = format_with(|f: &mut PyFormatter| {
            // Format the function expression.
            if f.context().is_expression_parenthesized(func.into()) {
                func.format().with_options(Parentheses::Always).fmt(f)
            } else {
                match func.as_ref() {
                    Expr::Attribute(expr) => expr
                        .format()
                        .with_options(call_chain_layout.decrement_call_like_count())
                        .fmt(f),
                    Expr::Call(expr) => expr.format().with_options(call_chain_layout).fmt(f),
                    Expr::Subscript(expr) => expr.format().with_options(call_chain_layout).fmt(f),
                    _ => func.format().with_options(Parentheses::Never).fmt(f),
                }
            }?;

            // Format comments between the function and its arguments.
            dangling_comments(dangling).fmt(f)?;

            // Format the arguments.
            arguments.format().fmt(f)
        });

        // Allow to indent the parentheses while
        // ```python
        // g1 = (
        //     queryset.distinct().order_by(field.name).values_list(field_name_flat_long_long=True)
        // )
        // ```
        if call_chain_layout.is_fluent() && self.call_chain_layout == CallChainLayout::Default {
            group(&fmt_func).fmt(f)
        } else {
            fmt_func.fmt(f)
        }
    }
}

impl NeedsParentheses for ExprCall {
    fn needs_parentheses(
        &self,
        _parent: AnyNodeRef,
        context: &PyFormatContext,
    ) -> OptionalParentheses {
        if CallChainLayout::from_expression(self.into(), context).is_fluent() {
            OptionalParentheses::Multiline
        } else if context.comments().has_dangling(self) {
            OptionalParentheses::Always
        } else if context.is_expression_parenthesized(self.func.as_ref().into()) {
            OptionalParentheses::Never
        } else {
            self.func.needs_parentheses(self.into(), context)
        }
    }
}
