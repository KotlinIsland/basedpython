use ruff_formatter::{format_args, write};
use ruff_python_ast::{AnyNodeRef, ElifElseClause, Expr, Pattern, StmtIf};
use ruff_text_size::Ranged;

use crate::expression::maybe_parenthesize_expression;
use crate::expression::parentheses::Parenthesize;
use crate::pattern::maybe_parenthesize_pattern;
use crate::prelude::*;
use crate::statement::clause::{ClauseHeader, clause};
use crate::statement::suite::SuiteKind;

/// Formats the condition of an `if` / `elif` clause: a plain expression, or the
/// basedpython pattern-matching form `let <pattern> := <subject>`
fn format_if_condition<'a>(
    pattern: Option<&'a Pattern>,
    test: &'a Expr,
    parent: AnyNodeRef<'a>,
) -> FormatIfCondition<'a> {
    FormatIfCondition {
        pattern,
        test,
        parent,
    }
}

struct FormatIfCondition<'a> {
    pattern: Option<&'a Pattern>,
    test: &'a Expr,
    parent: AnyNodeRef<'a>,
}

impl Format<PyFormatContext<'_>> for FormatIfCondition<'_> {
    fn fmt(&self, f: &mut PyFormatter) -> FormatResult<()> {
        if let Some(pattern) = self.pattern {
            write!(
                f,
                [
                    token("let"),
                    space(),
                    maybe_parenthesize_pattern(pattern, self.parent),
                    space(),
                    token(":="),
                    space(),
                ]
            )?;
        }
        maybe_parenthesize_expression(self.test, self.parent, Parenthesize::IfBreaks).fmt(f)
    }
}

#[derive(Default)]
pub struct FormatStmtIf;

impl FormatNodeRule<StmtIf> for FormatStmtIf {
    fn fmt_fields(&self, item: &StmtIf, f: &mut PyFormatter) -> FormatResult<()> {
        let StmtIf {
            range: _,
            node_index: _,
            pattern,
            test,
            body,
            elif_else_clauses,
        } = item;

        let comments = f.context().comments().clone();
        let trailing_colon_comment = comments.dangling(item);

        write!(
            f,
            [clause(
                ClauseHeader::If(item),
                &format_args![
                    token("if"),
                    space(),
                    format_if_condition(pattern.as_deref(), test, item.into()),
                ],
                trailing_colon_comment,
                body,
                SuiteKind::other(elif_else_clauses.is_empty()),
            )]
        )?;

        let mut last_node = body.last().unwrap().into();
        for clause in elif_else_clauses {
            format_elif_else_clause(
                clause,
                f,
                Some(last_node),
                SuiteKind::other(clause == elif_else_clauses.last().unwrap()),
            )?;
            last_node = clause.body.last().unwrap().into();
        }

        Ok(())
    }
}

/// Extracted so we can implement `FormatElifElseClause` but also pass in `last_node` from
/// `FormatStmtIf`
pub(crate) fn format_elif_else_clause(
    item: &ElifElseClause,
    f: &mut PyFormatter,
    last_node: Option<AnyNodeRef>,
    suite_kind: SuiteKind,
) -> FormatResult<()> {
    let ElifElseClause {
        range: _,
        node_index: _,
        pattern,
        test,
        body,
    } = item;

    let comments = f.context().comments().clone();
    let trailing_colon_comment = comments.dangling(item);
    let leading_comments = comments.leading(item);

    write!(
        f,
        [
            clause(
                ClauseHeader::ElifElse(item),
                &format_with(|f: &mut PyFormatter| {
                    f.options()
                        .source_map_generation()
                        .is_enabled()
                        .then_some(source_position(item.start()))
                        .fmt(f)?;
                    if let Some(test) = test {
                        write!(
                            f,
                            [
                                token("elif"),
                                space(),
                                format_if_condition(pattern.as_deref(), test, item.into()),
                            ]
                        )
                    } else {
                        token("else").fmt(f)
                    }
                }),
                trailing_colon_comment,
                body,
                suite_kind,
            )
            .with_leading_comments(leading_comments, last_node),
            f.options()
                .source_map_generation()
                .is_enabled()
                .then_some(source_position(item.end()))
        ]
    )
}
