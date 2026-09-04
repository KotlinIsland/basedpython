use ruff_formatter::write;
use ruff_python_ast as ast;
use ruff_python_ast::helpers::implements_declaration_expression;
use ruff_python_ast::{Expr, Operator, StmtExpr};

use crate::expression::maybe_parenthesize_expression;
use crate::expression::parentheses::Parenthesize;
use crate::prelude::*;
use crate::statement::trailing_semicolon;

#[derive(Default)]
pub struct FormatStmtExpr;

impl FormatNodeRule<StmtExpr> for FormatStmtExpr {
    fn fmt_fields(&self, item: &StmtExpr, f: &mut PyFormatter) -> FormatResult<()> {
        let StmtExpr { value, .. } = item;

        // basedpython: `implements A, B for ".*"` parses as a call to the
        // synthetic `__implements__` marker, whose func has no source text of its
        // own, so the declaration has to be written back out rather than printed
        // as the call it is stored as
        if f.options().is_basedpython()
            && let Some(declaration) = implements_declaration_expression(value)
        {
            write!(f, [token("implements"), space()])?;
            for (position, interface) in declaration.interfaces.iter().enumerate() {
                if position > 0 {
                    write!(f, [token(","), space()])?;
                }
                interface.format().fmt(f)?;
            }
            if !declaration.patterns.is_empty() {
                write!(f, [space(), token("for"), space()])?;
                for (position, pattern) in declaration.patterns.iter().enumerate() {
                    if position > 0 {
                        write!(f, [token(","), space()])?;
                    }
                    pattern.format().fmt(f)?;
                }
            }
            return Ok(());
        }

        if is_arithmetic_like(value) {
            maybe_parenthesize_expression(value, item, Parenthesize::Optional).fmt(f)?;
        } else {
            value.format().fmt(f)?;
        }

        if f.options().source_type().is_ipynb()
            && f.context().node_level().is_last_top_level_statement()
            && trailing_semicolon(item.into(), f.context().source()).is_some()
        {
            token(";").fmt(f)?;
        }

        Ok(())
    }
}

const fn is_arithmetic_like(expression: &Expr) -> bool {
    matches!(
        expression,
        Expr::BinOp(ast::ExprBinOp {
            op: Operator::BitOr
                | Operator::BitXor
                | Operator::LShift
                | Operator::RShift
                | Operator::Add
                | Operator::Sub,
            ..
        })
    )
}
