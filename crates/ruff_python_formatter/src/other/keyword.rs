use ruff_formatter::write;
use ruff_python_ast::Keyword;

use crate::prelude::*;

#[derive(Default)]
pub struct FormatKeyword;

impl FormatNodeRule<Keyword> for FormatKeyword {
    fn fmt_fields(&self, item: &Keyword, f: &mut PyFormatter) -> FormatResult<()> {
        let Keyword {
            range: _,
            node_index: _,
            arg,
            // basedpython lets the name be written as a string (`f("a b"=1)`) or
            // as a dotted path. `arg` holds the name, and formatting it slices
            // the source it was written from, so both come back exactly as
            // spelled — a quoted one keeps its own quote characters rather than
            // being normalised, the same way an identifier is left alone
            key: _,
            value,
        } = item;
        // Comments after the `=` or `**` are reassigned as leading comments on the value.
        if let Some(arg) = arg {
            write!(f, [arg.format(), token("="), value.format()])
        } else {
            write!(f, [token("**"), value.format()])
        }
    }
}
