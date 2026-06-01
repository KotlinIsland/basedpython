use ruff_formatter::write;
use ruff_python_ast::{Decorator, Expr, ExprContext, NodeKind, StmtClassDef};
use ruff_python_trivia::lines_after_ignoring_end_of_line_trivia;
use ruff_text_size::Ranged;

use crate::comments::format::{
    empty_lines_after_leading_comments, empty_lines_before_trailing_comments,
};
use crate::comments::{SourceComment, leading_comments, trailing_comments};
use crate::prelude::*;
use crate::statement::clause::{ClauseHeader, clause};
use crate::statement::suite::SuiteKind;
use crate::verbatim::verbatim_text;

/// True when this class is a basedpython `enum class` declaration: the parser
/// tags it with a synthetic `enum_def` marker decorator (a zero-binding `Name`
/// with `ExprContext::Invalid`). The formatter has no printer for the based-enum
/// surface (`enum class Name:` + bare variants), so reformatting from the AST
/// would mangle it into plain `class Name:` / `class Variant`. Render it
/// verbatim instead.
fn is_based_enum(item: &StmtClassDef) -> bool {
    item.decorator_list.iter().any(|decorator| {
        matches!(
            &decorator.expression,
            Expr::Name(name)
                if name.id.as_str() == "enum_def" && name.ctx == ExprContext::Invalid
        )
    })
}

#[derive(Default)]
pub struct FormatStmtClassDef;

impl FormatNodeRule<StmtClassDef> for FormatStmtClassDef {
    fn fmt_fields(&self, item: &StmtClassDef, f: &mut PyFormatter) -> FormatResult<()> {
        // basedpython `enum` blocks have no AST-faithful surface printer; emit
        // them verbatim so formatting doesn't rewrite `enum Name:` / bare
        // variants into `enum class Name:` / `class Variant`
        if is_based_enum(item) {
            return write!(f, [verbatim_text(item.range())]);
        }

        let StmtClassDef {
            range: _,
            node_index: _,
            name,
            arguments,
            body,
            type_params,
            decorator_list,
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
        // class Class:
        //     ...
        // ```
        //
        // At the top-level in a non-stub file, reformat as:
        // ```python
        // # comment
        //
        //
        // class Class:
        //     ...
        // ```
        // Note that this is only really relevant for the specific case in which there's a single
        // newline between the comment and the node, but we _require_ two newlines. If there are
        // _no_ newlines between the comment and the node, we don't insert _any_ newlines; if there
        // are more than two, then `leading_comments` will preserve the correct number of newlines.
        empty_lines_after_leading_comments(comments.leading(item)).fmt(f)?;

        // basedpython: `protocol Foo:` introduces a class without the `class`
        // keyword. the parser still synthesizes a `ClassDef` but tags it with a
        // synthetic `protocol_class` decorator so the round-trip emits the
        // keyword form instead of `protocol class Foo:`
        let suppress_class_keyword = f.options().is_basedpython()
            && decorator_list.iter().any(
                |d| matches!(&d.expression, Expr::Name(n) if n.id.as_str() == "protocol_class"),
            );

        let format_header = format_with(|f| {
            if !suppress_class_keyword {
                write!(f, [token("class"), space()])?;
            }
            write!(f, [name.format()])?;

            if let Some(type_params) = type_params.as_deref() {
                write!(f, [type_params.format()])?;
            }

            if let Some(arguments) = arguments.as_deref() {
                // Drop empty the arguments node entirely (i.e., remove the parentheses) if it is empty,
                // e.g., given:
                // ```python
                // class A():
                //     ...
                // ```
                //
                // Format as:
                // ```python
                // class A:
                //     ...
                // ```
                //
                // However, preserve any dangling end-of-line comments, e.g., given:
                // ```python
                // class A(  # comment
                // ):
                //     ...
                //
                // Format as:
                // ```python
                // class A:  # comment
                //     ...
                // ```
                //
                // However, the arguments contain any dangling own-line comments, we retain the
                // parentheses, e.g., given:
                // ```python
                // class A(  # comment
                //     # comment
                // ):
                //     ...
                // ```
                //
                // Format as:
                // ```python
                // class A(  # comment
                //     # comment
                // ):
                //     ...
                // ```
                if arguments.is_empty()
                    && comments
                        .dangling(arguments)
                        .iter()
                        .all(|comment| comment.line_position().is_end_of_line())
                {
                    let dangling = comments.dangling(arguments);
                    write!(f, [trailing_comments(dangling)])?;
                } else {
                    write!(f, [arguments.format()])?;
                }
            }

            Ok(())
        });

        // basedpython: `class Foo` with no body — no colon, no suite
        if body.is_empty() {
            write!(
                f,
                [
                    FormatDecorators {
                        decorators: decorator_list,
                        leading_definition_comments,
                    },
                    &format_header,
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
                        ClauseHeader::Class(item),
                        &format_header,
                        trailing_definition_comments,
                        body,
                        SuiteKind::Class,
                    ),
                ]
            )?;
        }

        // If the class contains trailing comments, insert newlines before them.
        // For example, given:
        // ```python
        // class Class:
        //     ...
        // # comment
        // ```
        //
        // At the top-level in a non-stub file, reformat as:
        // ```python
        // class Class:
        //     ...
        //
        //
        // # comment
        // ```
        empty_lines_before_trailing_comments(comments.trailing(item), NodeKind::StmtClassDef)
            .fmt(f)?;

        Ok(())
    }
}

pub(super) struct FormatDecorators<'a> {
    pub(super) decorators: &'a [Decorator],
    pub(super) leading_definition_comments: &'a [SourceComment],
}

impl Format<PyFormatContext<'_>> for FormatDecorators<'_> {
    fn fmt(&self, f: &mut Formatter<PyFormatContext<'_>>) -> FormatResult<()> {
        if let Some(last_decorator) = self.decorators.last() {
            let source = f.context().source();
            // basedpython modifier keywords are synthesized as decorators but
            // need to flow inline with the def/class header rather than be
            // separated by a hard line break — emit them in source order and
            // only break on real `@…` decorators.
            for (i, dec) in self.decorators.iter().enumerate() {
                let is_synthetic = crate::other::decorator::is_synthetic_modifier(dec, source);
                dec.format().fmt(f)?;
                let is_last = i + 1 == self.decorators.len();
                if !is_last && !is_synthetic {
                    hard_line_break().fmt(f)?;
                }
            }
            let last_synthetic =
                crate::other::decorator::is_synthetic_modifier(last_decorator, source);

            if self.leading_definition_comments.is_empty() {
                if !last_synthetic {
                    write!(f, [hard_line_break()])?;
                }
            } else {
                // Write any leading definition comments (between last decorator and the header)
                // while maintaining the right amount of empty lines between the comment
                // and the last decorator.
                let leading_line = if lines_after_ignoring_end_of_line_trivia(
                    last_decorator.end(),
                    f.context().source(),
                ) <= 1
                {
                    hard_line_break()
                } else {
                    empty_line()
                };

                write!(
                    f,
                    [
                        leading_line,
                        leading_comments(self.leading_definition_comments)
                    ]
                )?;
            }
        }

        Ok(())
    }
}
