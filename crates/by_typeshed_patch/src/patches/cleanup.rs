//! global post-conversion cleanups that apply to every module
//!
//! - [`StripIgnoreComments`] deletes mypy/pyright suppression comments
//!   (`# type: ignore[...]`, `# pyright: ignore[...]`). basedpython has no need
//!   to carry another checker's suppressions through its typeshed
//! - [`BodylessStubs`] drops the `: ...` body from decorated stub methods
//!   (properties, setters, overloads) that reverse-transpile left behind, so
//!   every stub member uses the bodyless basedpython form uniformly

use std::path::Path;

use ruff_python_ast::token::TokenKind;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_stmt};
use ruff_python_ast::{Expr, ModModule, Stmt, StmtFunctionDef};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::{Edit, Patch};

/// deletes `# type: ignore` and `# pyright: ignore` comments
pub struct StripIgnoreComments;

impl Patch for StripIgnoreComments {
    fn name(&self) -> &'static str {
        "strip-ignore-comments"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let bytes = source.as_bytes();
        let mut edits = Vec::new();
        for token in parsed.tokens() {
            if token.kind() != TokenKind::Comment {
                continue;
            }
            let text = &source[token.range()];
            if !is_ignore_comment(text) {
                continue;
            }
            // extend the deletion back over the whitespace that separated the
            // comment from the code (or from the start of the line)
            let mut start = token.range().start().to_usize();
            while start > 0 && matches!(bytes[start - 1], b' ' | b'\t') {
                start -= 1;
            }
            let mut end = token.range().end().to_usize();
            // a comment that occupied its own line takes the trailing newline
            // with it; a trailing comment leaves the code and newline in place
            if start == 0 || bytes[start - 1] == b'\n' {
                if end < bytes.len() && bytes[end] == b'\n' {
                    end += 1;
                }
            }
            edits.push(Edit {
                start,
                end,
                replacement: String::new(),
            });
        }
        edits
    }
}

/// `# type: ignore[...]` / `# type: ignore` / `# pyright: ignore[...]`. the body
/// of a `# type: ignore` may only be followed by `[`, whitespace, or end so a
/// stray `# type: ignorable` comment is not mistaken for a suppression
fn is_ignore_comment(text: &str) -> bool {
    let body = text.trim_start_matches('#').trim_start();
    for prefix in ["type: ignore", "pyright: ignore"] {
        if let Some(rest) = body.strip_prefix(prefix)
            && rest
                .chars()
                .next()
                .is_none_or(|c| c == '[' || c.is_whitespace())
        {
            return true;
        }
    }
    false
}

/// drops the `: ...` body from stub methods that still carry it
pub struct BodylessStubs;

impl Patch for BodylessStubs {
    fn name(&self) -> &'static str {
        "bodyless-stubs"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, parsed: &Parsed<ModModule>, _source: &str) -> Vec<Edit> {
        let mut collector = EllipsisBodies { edits: Vec::new() };
        for stmt in &parsed.syntax().body {
            collector.visit_stmt(stmt);
        }
        collector.edits
    }
}

struct EllipsisBodies {
    edits: Vec<Edit>,
}

impl EllipsisBodies {
    /// if `func`'s body is exactly `...`, emit an edit deleting the `: ...` that
    /// follows the signature, turning it into the bodyless stub form
    fn strip(&mut self, func: &StmtFunctionDef) {
        let [Stmt::Expr(expr)] = func.body.as_slice() else {
            return;
        };
        if !matches!(&*expr.value, Expr::EllipsisLiteral(_)) {
            return;
        }
        let sig_end = func.returns.as_ref().map_or_else(
            || func.parameters.range().end(),
            |returns| returns.range().end(),
        );
        let ellipsis_end = expr.range().end();
        if sig_end >= ellipsis_end {
            return;
        }
        self.edits.push(Edit {
            start: sig_end.to_usize(),
            end: ellipsis_end.to_usize(),
            replacement: String::new(),
        });
    }
}

impl<'a> SourceOrderVisitor<'a> for EllipsisBodies {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if let Stmt::FunctionDef(func) = stmt {
            self.strip(func);
        }
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn strip_comments(src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = StripIgnoreComments.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    fn bodyless(src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = BodylessStubs.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn strips_trailing_type_ignore() {
        let src = "def f(self) -> str  # type: ignore[misc]\n";
        assert_eq!(strip_comments(src), "def f(self) -> str\n");
    }

    #[test]
    fn strips_pyright_ignore() {
        let src = "x: int = 1  # pyright: ignore[reportGeneralTypeIssues]\n";
        assert_eq!(strip_comments(src), "x: int = 1\n");
    }

    #[test]
    fn strips_bare_type_ignore() {
        let src = "__hash__: ClassVar[None]  # type: ignore\n";
        assert_eq!(strip_comments(src), "__hash__: ClassVar[None]\n");
    }

    #[test]
    fn strips_standalone_ignore_line() {
        let src = "a = 1\n# type: ignore[misc]\nb = 2\n";
        assert_eq!(strip_comments(src), "a = 1\nb = 2\n");
    }

    #[test]
    fn keeps_noqa_and_ordinary_comments() {
        let src = "from x import y  # noqa: Y023\n# a normal note\nz = 1\n";
        assert_eq!(strip_comments(src), src);
    }

    #[test]
    fn does_not_match_ignorable_lookalike() {
        let src = "x = 1  # type: ignorable thing\n";
        assert_eq!(strip_comments(src), src);
    }

    #[test]
    fn drops_ellipsis_body_oneline() {
        let src = "def __class__(self) -> type[Self]: ...\n";
        assert_eq!(bodyless(src), "def __class__(self) -> type[Self]\n");
    }

    #[test]
    fn drops_ellipsis_body_no_return() {
        let src = "def f(self): ...\n";
        assert_eq!(bodyless(src), "def f(self)\n");
    }

    #[test]
    fn keeps_docstring_bodies() {
        let src = "def f(self) -> int:\n    \"\"\"doc\"\"\"\n";
        assert_eq!(bodyless(src), src);
    }

    #[test]
    fn keeps_already_bodyless() {
        let src = "def f(self) -> int\n";
        assert_eq!(bodyless(src), src);
    }

    #[test]
    fn drops_ellipsis_across_multiline_signature() {
        let src = "def f(\n    x: int,\n) -> int: ...\n";
        assert_eq!(bodyless(src), "def f(\n    x: int,\n) -> int\n");
    }
}
