use ruff_formatter::write;
use ruff_python_ast::StmtAugAssign;
use ruff_text_size::Ranged;

use crate::prelude::*;
use crate::statement::assignment_alignment::AssignmentPadding;
use crate::statement::stmt_assign::{
    AnyAssignmentOperator, AnyBeforeOperator, FormatStatementsLastExpression,
    has_target_own_parentheses,
};
use crate::statement::trailing_semicolon;
use crate::{AsFormat, FormatNodeRule};

#[derive(Default)]
pub struct FormatStmtAugAssign;

impl FormatNodeRule<StmtAugAssign> for FormatStmtAugAssign {
    fn fmt_fields(&self, item: &StmtAugAssign, f: &mut PyFormatter) -> FormatResult<()> {
        let StmtAugAssign {
            target,
            op,
            value,
            range: _,
            node_index: _,
        } = item;

        let padding = AssignmentPadding::of(item.start(), f.context());

        if has_target_own_parentheses(target, f.context())
            && !f.context().is_expression_parenthesized(target.into())
        {
            FormatStatementsLastExpression::RightToLeft {
                before_operator: AnyBeforeOperator::Expression(target),
                operator: AnyAssignmentOperator::aug_assign(*op, padding),
                value,
                statement: item.into(),
            }
            .fmt(f)?;
        } else {
            write!(
                f,
                [
                    target.format(),
                    space(),
                    AnyAssignmentOperator::aug_assign(*op, padding),
                    space(),
                    FormatStatementsLastExpression::left_to_right(value, item)
                ]
            )?;
        }

        if f.options().source_type().is_ipynb()
            && f.context().node_level().is_last_top_level_statement()
            && target.is_name_expr()
            && trailing_semicolon(item.into(), f.context().source()).is_some()
        {
            token(";").fmt(f)?;
        }

        Ok(())
    }
}
