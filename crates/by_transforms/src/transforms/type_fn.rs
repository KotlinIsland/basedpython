//! AST pass that erases `type def` declarations.
//!
//! ```by
//! type def F[X]:
//!     if X <= int:
//!         return int
//!     return str
//!
//! def f(a: F[bool]): ...
//! ```
//!
//! lowers to
//!
//! ```python
//! def f(a: int): ...
//! ```
//!
//! the *applications* are rewritten by the symbolic fold pass, which reads back
//! the type ty resolved each `F[...]` to (see
//! [`TypeInfo::is_type_fn_application`]). this pass removes what is left: the
//! declaration itself, which has no runtime meaning once every application has
//! been inlined.
//!
//! [`TypeInfo::is_type_fn_application`]: crate::type_info::TypeInfo::is_type_fn_application

use ruff_python_ast::helpers::is_type_def;
use ruff_python_ast::{ModModule, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, PassContext};

pub(crate) struct TypeFnPass<'a> {
    source: &'a str,
}

impl<'a> TypeFnPass<'a> {
    pub(crate) fn new(source: &'a str) -> Self {
        Self { source }
    }
}

impl AstPass for TypeFnPass<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        // a `type def` is only valid at statement level, and nesting it inside a
        // function would give it a closure the evaluator cannot reproduce, so only
        // module and class bodies are walked
        collect(&module.body, self.source, ctx);
    }
}

fn collect(body: &[Stmt], source: &str, ctx: &mut PassContext) {
    // erasing every member of a class body would leave `class A:` with nothing
    // under it, which does not parse — the first erasure keeps the block alive by
    // leaving a `pass` behind instead of deleting outright
    let mut keep_block_alive = body_would_be_emptied(body);

    for stmt in body {
        match stmt {
            Stmt::FunctionDef(function) if is_type_def(function) => {
                let range = erased_range(function, source);
                let replacement = if keep_block_alive {
                    keep_block_alive = false;
                    let indent = usize::from(function.range().start() - range.start());
                    format!("{}pass\n", " ".repeat(indent))
                } else {
                    String::new()
                };
                ctx.text_edits.push((range, replacement));
            }
            Stmt::ClassDef(class) => collect(&class.body, source, ctx),
            _ => {}
        }
    }
}

/// Whether erasing this body's `type def`s would leave it with no statements.
fn body_would_be_emptied(body: &[Stmt]) -> bool {
    !body.is_empty()
        && body
            .iter()
            .all(|stmt| matches!(stmt, Stmt::FunctionDef(f) if is_type_def(f)))
}

/// The range to delete: the statement and the newline that terminates it, plus any
/// *following* blank lines — but only as far as the next line that is indented at
/// least as deeply as the declaration itself.
///
/// The bound matters. A `type def` at the end of a class body is followed by a
/// blank line and then a dedented, module-level statement; swallowing that blank
/// line splices the dedented statement onto the end of the class block, silently
/// turning a module-level function into a method.
fn erased_range(function: &StmtFunctionDef, source: &str) -> TextRange {
    let statement_start = usize::from(function.range().start());
    // delete from the *line* start: the declaration's own indentation is not part
    // of its node range, and leaving it behind would prepend it to whatever line
    // follows — over-indenting the next member of the same block
    let line_start = source[..statement_start]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let indent = statement_start - line_start;

    let mut end = usize::from(function.range().end());
    // the statement's own line terminator always goes
    match source[end..].find('\n') {
        Some(newline) => end += newline + 1,
        None => end = source.len(),
    }

    // then blank lines, but only while what follows stays inside this block
    while let Some(newline) = source[end..].find('\n') {
        let rest = &source[end..];
        if !rest[..newline].trim().is_empty() {
            break;
        }
        let after_blank = &rest[newline + 1..];
        let next_indent = after_blank.len() - after_blank.trim_start_matches([' ', '\t']).len();
        if !after_blank.trim_start().is_empty() && next_indent < indent {
            break;
        }
        end += newline + 1;
    }

    TextRange::new(
        TextSize::try_from(line_start).unwrap_or(function.range().start()),
        TextSize::try_from(end).unwrap_or(function.range().end()),
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
    fn application_is_inlined_and_declaration_erased() {
        check(
            indoc! {"
                type def F[X]:
                    if X <= int:
                        return int
                    return str

                def f(a: F[bool], b: F[str]) -> F[bool]:
                    return a
            "},
            indoc! {"
                def f(a: int, b: str) -> int:
                    return a
            "},
        );
    }

    #[test]
    fn erasing_a_class_member_keeps_following_statements_at_module_level() {
        check(
            indoc! {"
                class A:
                    type def F[X]:
                        return int

                def f(a: A.F[bool]) -> None: ...
            "},
            indoc! {"
                class A:
                    pass

                def f(a: int) -> None: ...
            "},
        );
    }

    #[test]
    fn erasing_one_of_several_class_members_leaves_no_pass() {
        check(
            indoc! {"
                class A:
                    type def F[X]:
                        return int

                    def m(self) -> None: ...
            "},
            indoc! {"
                class A:
                    def m(self) -> None: ...
            "},
        );
    }

    #[test]
    fn union_result_is_inlined() {
        check(
            indoc! {"
                type def Opt[X]:
                    return X | None

                def f(a: Opt[int]):
                    print(a)
            "},
            indoc! {"
                def f(a: int | None):
                    print(a)
            "},
        );
    }
}
