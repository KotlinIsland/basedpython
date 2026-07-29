use ruff_formatter::{format_args, write};
use ruff_python_ast::{Expr, MatchCase, StmtTypeAlias, TypeParams};

use crate::comments::leading_alternate_branch_comments;
use crate::context::{NodeLevel, WithNodeLevel};
use crate::expression::is_invalid_type_expression;
use crate::expression::maybe_parenthesize_expression;
use crate::expression::parentheses::{Parentheses, Parenthesize};
use crate::prelude::*;
use crate::statement::assignment_alignment::AssignmentPadding;
use crate::statement::clause::{ClauseHeader, clause_header};
use crate::statement::stmt_assign::{
    AnyAssignmentOperator, AnyBeforeOperator, FormatStatementsLastExpression,
};

#[derive(Default)]
pub struct FormatStmtTypeAlias;

impl FormatNodeRule<StmtTypeAlias> for FormatStmtTypeAlias {
    fn fmt_fields(&self, item: &StmtTypeAlias, f: &mut PyFormatter) -> FormatResult<()> {
        let StmtTypeAlias {
            name,
            type_params,
            value,
            cases,
            range: _,
            node_index: _,
            is_private,
        } = item;

        if *is_private {
            write!(f, [token("private"), space()])?;
        }

        // basedpython: a match type's value is decided by `case` blocks, so the statement is
        // a clause header (`type X[...] = match S:`) followed by an indented suite rather
        // than an assignment
        if !cases.is_empty() {
            return format_match_type_alias(item, name, type_params.as_deref(), value, cases, f);
        }

        write!(f, [token("type"), space(), name.as_ref().format()])?;

        if is_invalid_type_expression(value) {
            if let Some(type_params) = type_params {
                type_params.format().fmt(f)?;
            }

            return write!(
                f,
                [
                    space(),
                    token("="),
                    space(),
                    value.format().with_options(Parentheses::Preserve)
                ]
            );
        }

        if let Some(type_params) = type_params {
            return FormatStatementsLastExpression::RightToLeft {
                before_operator: AnyBeforeOperator::TypeParams(type_params),
                // Type aliases aren't part of the runs of assignments that get lined up.
                operator: AnyAssignmentOperator::assign(AssignmentPadding::None),
                value,
                statement: item.into(),
            }
            .fmt(f);
        }

        write!(
            f,
            [
                space(),
                token("="),
                space(),
                FormatStatementsLastExpression::left_to_right(value, item)
            ]
        )
    }
}

/// basedpython: formats `type X[...] = match S:` and its `case` blocks.
///
/// This mirrors [`FormatStmtMatch`], the only differences being the header — which carries
/// the alias name and type parameters ahead of the `match` keyword — and that a case body
/// is a single type expression.
///
/// [`FormatStmtMatch`]: crate::statement::stmt_match::FormatStmtMatch
fn format_match_type_alias(
    item: &StmtTypeAlias,
    name: &Expr,
    type_params: Option<&TypeParams>,
    subject: &Expr,
    cases: &[MatchCase],
    f: &mut PyFormatter,
) -> FormatResult<()> {
    let comments = f.context().comments().clone();
    let dangling_item_comments = comments.dangling(item);

    clause_header(
        ClauseHeader::TypeAliasMatch(item),
        dangling_item_comments,
        &format_args![
            token("type"),
            space(),
            name.format(),
            type_params.map(AsFormat::format),
            space(),
            token("="),
            space(),
            token("match"),
            space(),
            maybe_parenthesize_expression(subject, item, Parenthesize::IfBreaks),
        ],
    )
    .fmt(f)?;

    let mut cases_iter = cases.iter();
    let Some(first) = cases_iter.next() else {
        return Ok(());
    };

    // The new level is for the `case` nodes.
    let mut f = WithNodeLevel::new(NodeLevel::CompoundStatement, f);

    write!(f, [block_indent(&first.format())])?;
    let mut last_case = first;

    for case in cases_iter {
        let last_suite_in_statement = Some(case) == cases.last();
        write!(
            f,
            [block_indent(&format_args!(
                leading_alternate_branch_comments(comments.leading(case), last_case.body.last()),
                case.format().with_options(last_suite_in_statement)
            ))]
        )?;
        last_case = case;
    }

    Ok(())
}
