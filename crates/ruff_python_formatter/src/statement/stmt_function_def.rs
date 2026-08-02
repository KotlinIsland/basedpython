use crate::comments::format::{
    empty_lines_after_leading_comments, empty_lines_before_trailing_comments,
};
use crate::expression::maybe_parenthesize_expression;
use crate::expression::parentheses::{Parentheses, Parenthesize};
use crate::prelude::*;
use crate::statement::clause::{ClauseHeader, clause};
use crate::statement::stmt_class_def::FormatDecorators;
use crate::statement::suite::SuiteKind;
use crate::verbatim::{verbatim_node_range, write_verbatim_node};
use ruff_formatter::write;
use ruff_python_ast::{Expr, ExprContext, NodeKind, StmtFunctionDef};

/// True when this function is a basedpython `init(...)` constructor shorthand:
/// the parser tags it with a synthetic `__init_method__` marker decorator (a
/// zero-binding `Name` with `ExprContext::Invalid`). The synthesised `__init__`
/// name, implicit `self` parameter, `-> None` annotation and
/// `self.<name> = <name>` body statements are all zero-width, so printing from
/// the AST would emit the surface `init(...)` text alongside a mangled
/// reconstruction of the synthesised `def`. Render it verbatim instead.
fn is_init_method(item: &StmtFunctionDef) -> bool {
    item.decorator_list.iter().any(|decorator| {
        matches!(
            &decorator.expression,
            Expr::Name(name)
                if name.id.as_str() == "__init_method__" && name.ctx == ExprContext::Invalid
        )
    })
}

#[derive(Default)]
pub struct FormatStmtFunctionDef;

impl FormatNodeRule<StmtFunctionDef> for FormatStmtFunctionDef {
    fn fmt_fields(&self, item: &StmtFunctionDef, f: &mut PyFormatter) -> FormatResult<()> {
        // basedpython trailing lambda blocks (`f(2):` + suite) and `init(...)`
        // constructor shorthands have no AST-faithful surface printer — their
        // identifiers are synthetic and zero-width — so emit them verbatim, like
        // based enums
        if item.is_trailing_lambda || is_init_method(item) {
            let comments = f.context().comments().clone();
            let range = verbatim_node_range(item.into(), &comments);
            return write_verbatim_node(item, range, f);
        }

        let StmtFunctionDef {
            decorator_list,
            body,
            ..
        } = item;

        let comments = f.context().comments().clone();

        let dangling_comments = comments.dangling(item);
        let trailing_definition_comments_start =
            dangling_comments.partition_point(|comment| comment.line_position().is_own_line());

        let (leading_definition_comments, trailing_definition_comments) =
            dangling_comments.split_at(trailing_definition_comments_start);

        // If the class contains leading comments, insert newlines before them.
        // For example, given:
        // ```python
        // # comment
        //
        // def func():
        //     ...
        // ```
        //
        // At the top-level in a non-stub file, reformat as:
        // ```python
        // # comment
        //
        //
        // def func():
        //     ...
        // ```
        // Note that this is only really relevant for the specific case in which there's a single
        // newline between the comment and the node, but we _require_ two newlines. If there are
        // _no_ newlines between the comment and the node, we don't insert _any_ newlines; if there
        // are more than two, then `leading_comments` will preserve the correct number of newlines.
        empty_lines_after_leading_comments(comments.leading(item)).fmt(f)?;

        // basedpython: `def f()` with no body — no colon, no suite
        if body.is_empty() {
            write!(
                f,
                [
                    FormatDecorators {
                        decorators: decorator_list,
                        leading_definition_comments,
                    },
                    format_with(|f| format_function_header(f, item)),
                    hard_line_break(),
                ]
            )?;
        } else {
            write!(
                f,
                [
                    FormatDecorators {
                        decorators: decorator_list,
                        leading_definition_comments,
                    },
                    clause(
                        ClauseHeader::Function(item),
                        &format_with(|f| format_function_header(f, item)),
                        trailing_definition_comments,
                        body,
                        SuiteKind::Function,
                    ),
                ]
            )?;
        }

        // If the function contains trailing comments, insert newlines before them.
        // For example, given:
        // ```python
        // def func():
        //     ...
        // # comment
        // ```
        //
        // At the top-level in a non-stub file, reformat as:
        // ```python
        // def func():
        //     ...
        //
        //
        // # comment
        // ```
        empty_lines_before_trailing_comments(comments.trailing(item), NodeKind::StmtFunctionDef)
            .fmt(f)
    }
}

fn format_function_header(f: &mut PyFormatter, item: &StmtFunctionDef) -> FormatResult<()> {
    let StmtFunctionDef {
        range: _,
        node_index: _,
        is_async,
        decorator_list: _,
        name,
        type_params,
        parameters,
        returns,
        raises,
        body: _,
        is_trailing_lambda: _,
        is_asserts_return,
    } = item;

    let comments = f.context().comments().clone();

    if *is_async {
        write!(f, [token("async"), space()])?;
    }

    write!(f, [token("def"), space(), name.format()])?;

    // basedpython: a list made only of `some T` holes was never written, so emitting it would
    // invent an empty `[]`
    if let Some(type_params) = type_params.as_ref()
        && !type_params.iter().all(|type_param| {
            type_param
                .as_type_var()
                .is_some_and(|type_var| type_var.is_some_hole)
        })
    {
        type_params.format().fmt(f)?;
    }

    let format_inner = format_with(|f: &mut PyFormatter| {
        // basedpython: a `type def` takes its parameters from the type-parameter
        // list and has no `(...)`. its synthetic marker decorator already printed
        // the `type` keyword verbatim, so emitting the (empty, zero-width)
        // parameter list here would turn `type def F[X]:` into `type def F[X]():`,
        // which does not parse
        if !ruff_python_ast::helpers::is_type_def(item) {
            parameters.format().fmt(f)?;
        }

        if let Some(return_annotation) = returns.as_deref() {
            write!(f, [space(), token("->"), space()])?;

            // basedpython: `-> asserts x` — the keyword lives on the function, so it
            // has to be written back before the asserted expression
            if *is_asserts_return {
                write!(f, [token("asserts"), space()])?;
            }

            if return_annotation.is_tuple_expr() {
                let parentheses = if comments.has_leading(return_annotation) {
                    Parentheses::Always
                } else {
                    Parentheses::Never
                };
                return_annotation.format().with_options(parentheses).fmt(f)
            } else if comments.has_trailing(return_annotation) {
                // Intentionally parenthesize any return annotations with trailing comments.
                // This avoids an instability in cases like:
                // ```python
                // def double(
                //     a: int
                // ) -> (
                //     int  # Hello
                // ):
                //     pass
                // ```
                // If we allow this to break, it will be formatted as follows:
                // ```python
                // def double(
                //     a: int
                // ) -> int:  # Hello
                //     pass
                // ```
                // On subsequent formats, the `# Hello` will be interpreted as a dangling
                // comment on a function, yielding:
                // ```python
                // def double(a: int) -> int:  # Hello
                //     pass
                // ```
                // Ideally, we'd reach that final formatting in a single pass, but doing so
                // requires that the parent be aware of how the child is formatted, which
                // is challenging. As a compromise, we break those expressions to avoid an
                // instability.

                return_annotation
                    .format()
                    .with_options(Parentheses::Always)
                    .fmt(f)
            } else {
                let parenthesize = if parameters.is_empty() && !comments.has(parameters.as_ref()) {
                    // If the parameters are empty, add parentheses around literal expressions
                    // (any non splitable expression) but avoid parenthesizing subscripts and
                    // other parenthesized expressions unless necessary.
                    Parenthesize::IfBreaksParenthesized
                } else {
                    // Otherwise, use our normal rules for parentheses, which allows us to break
                    // like:
                    // ```python
                    // def f(
                    //     x,
                    // ) -> Tuple[
                    //     int,
                    //     int,
                    // ]:
                    //     ...
                    // ```
                    Parenthesize::IfBreaks
                };
                maybe_parenthesize_expression(return_annotation, item, parenthesize).fmt(f)
            }
        } else {
            Ok(())
        }?;

        // basedpython: `raises <type>` follows the return annotation
        if let Some(raises) = raises.as_deref() {
            write!(f, [space(), token("raises"), space()])?;
            maybe_parenthesize_expression(raises, item, Parenthesize::IfBreaks).fmt(f)?;
        }

        Ok(())
    });

    group(&format_inner).fmt(f)
}
