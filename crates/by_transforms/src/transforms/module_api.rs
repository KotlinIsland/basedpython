//! AST pass that erases `implements` declarations.
//!
//! ```by
//! implements Backend
//!
//! def connect(url: str) -> str: ...
//! ```
//!
//! lowers to
//!
//! ```python
//! def connect(url: str) -> str: ...
//! ```
//!
//! the declaration is checked entirely by ty and has no runtime meaning, so
//! nothing is left behind. The import that named the interface stays: it may be
//! needed by annotations, and removing it would change the emitted module's own
//! surface.
//!
//! A declaration that is not at module level has nothing to attach to, and ty
//! reports it as an error of its own. It is erased all the same: leaving it in
//! would emit a call to a name that does not exist at runtime, which is a worse
//! failure than the one the author already has to fix.

use ruff_python_ast::helpers::implements_declaration;
use ruff_python_ast::statement_visitor::{StatementVisitor, walk_stmt};
use ruff_python_ast::{ModModule, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, PassContext};

pub(crate) struct ModuleApiPass<'a> {
    source: &'a str,
}

impl<'a> ModuleApiPass<'a> {
    pub(crate) fn new(source: &'a str) -> Self {
        Self { source }
    }
}

impl AstPass for ModuleApiPass<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        collect(&module.body, self.source, ctx);
    }
}

/// Collects the erasures for one body, and for every body nested inside it.
fn collect(body: &[Stmt], source: &str, ctx: &mut PassContext) {
    // erasing every statement of a body would leave a block with nothing under
    // it, which does not parse — the first erasure keeps the block alive by
    // leaving a `pass` behind instead
    let mut keep_block_alive = !body.is_empty()
        && body
            .iter()
            .all(|stmt| implements_declaration(stmt).is_some());

    for stmt in body {
        if implements_declaration(stmt).is_some() {
            let range = erased_range(stmt, source);
            let replacement = if keep_block_alive {
                keep_block_alive = false;
                let indent = usize::from(stmt.range().start() - range.start());
                format!("{}pass\n", " ".repeat(indent))
            } else {
                String::new()
            };
            ctx.text_edits.push((range, replacement));
        } else {
            let mut nested = Nested { source, ctx };
            walk_stmt(&mut nested, stmt);
        }
    }
}

/// Walks into the bodies a statement carries, so a declaration written inside one
/// is erased too.
struct Nested<'a, 'ctx> {
    source: &'a str,
    ctx: &'ctx mut PassContext,
}

impl<'a> StatementVisitor<'a> for Nested<'_, '_> {
    fn visit_body(&mut self, body: &'a [Stmt]) {
        collect(body, self.source, self.ctx);
        for stmt in body {
            if implements_declaration(stmt).is_none() {
                walk_stmt(self, stmt);
            }
        }
    }
}

/// The range to delete: the declaration's whole line, terminator included, and
/// the blank lines that followed it.
///
/// From the line start rather than the statement start, because the declaration's
/// own indentation is not part of its node range and leaving it behind would
/// prepend it to whatever line follows. Through the blank lines after it because a
/// declaration is written with a gap under it, and leaving the gap behind opens
/// the emitted module with an empty line.
fn erased_range(stmt: &Stmt, source: &str) -> TextRange {
    let start = usize::from(stmt.range().start());
    let line_start = source[..start].rfind('\n').map_or(0, |newline| newline + 1);
    let end = usize::from(stmt.range().end());
    let mut end = match source[end..].find('\n') {
        Some(newline) => end + newline + 1,
        None => source.len(),
    };
    // a declaration is only ever at module level, so what follows is at column
    // zero too and there is no block for a swallowed blank line to close
    while let Some(newline) = source[end..].find('\n') {
        if !source[end..end + newline].trim().is_empty() {
            break;
        }
        end += newline + 1;
    }
    TextRange::new(
        TextSize::try_from(line_start).unwrap_or_default(),
        TextSize::try_from(end).unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn a_declaration_is_erased() {
        check(
            indoc! {"
                implements Backend

                def connect(url: str) -> str:
                    return url
            "},
            indoc! {"
                def connect(url: str) -> str:
                    return url
            "},
        );
    }

    #[test]
    fn a_rule_is_erased() {
        check(
            indoc! {"
                implements Backend, Migratable for \".*\", \"!.base\"

                x = 1
            "},
            indoc! {"
                x = 1
            "},
        );
    }

    #[test]
    fn a_declaration_inside_a_body_is_erased_too() {
        // ty reports it; emitting a call to a name that does not exist at runtime
        // would be a second, worse problem
        check(
            indoc! {"
                def f():
                    implements Backend
            "},
            indoc! {"
                def f():
                    pass
            "},
        );
    }

    #[test]
    fn a_trailing_comment_goes_with_the_declaration() {
        check(
            indoc! {"
                implements Backend  # the plugin interface

                x = 1
            "},
            indoc! {"
                x = 1
            "},
        );
    }

    #[test]
    fn consecutive_declarations_are_erased() {
        check(
            indoc! {"
                implements Backend
                implements Migratable
                x = 1
            "},
            indoc! {"
                x = 1
            "},
        );
    }

    #[test]
    fn a_declaration_at_the_end_of_a_file_is_erased() {
        check(
            indoc! {"
                x = 1
                implements Backend
            "},
            indoc! {"
                x = 1
            "},
        );
    }
}
