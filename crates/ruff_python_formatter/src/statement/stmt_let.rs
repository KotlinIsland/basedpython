use ruff_formatter::{format_args, write};
use ruff_python_ast::StmtLet;

use crate::expression::maybe_parenthesize_expression;
use crate::expression::parentheses::Parenthesize;
use crate::pattern::maybe_parenthesize_pattern;
use crate::prelude::*;
use crate::statement::clause::{ClauseHeader, clause};
use crate::statement::suite::SuiteKind;

#[derive(Default)]
pub struct FormatStmtLet;

impl FormatNodeRule<StmtLet> for FormatStmtLet {
    fn fmt_fields(&self, item: &StmtLet, f: &mut PyFormatter) -> FormatResult<()> {
        let StmtLet {
            range: _,
            node_index: _,
            pattern,
            value,
            orelse,
        } = item;

        let comments = f.context().comments().clone();
        let dangling_comments = comments.dangling(item);

        let binding = format_with(|f: &mut PyFormatter| {
            write!(
                f,
                [
                    token("let"),
                    space(),
                    maybe_parenthesize_pattern(pattern, item),
                    space(),
                    token(":="),
                    space(),
                    maybe_parenthesize_expression(value, item, Parenthesize::IfBreaks),
                ]
            )
        });

        // without an `else` block this is an ordinary simple statement; with one
        // the whole `let ... else:` line is that block's clause header
        if orelse.is_empty() {
            return binding.fmt(f);
        }

        write!(
            f,
            [clause(
                ClauseHeader::Let(item),
                &format_args![binding, space(), token("else")],
                dangling_comments,
                orelse,
                SuiteKind::other(true),
            )]
        )
    }
}
