//! lowers basedpython `from X export Y` to `from X import Y as Y`.
//!
//! `Y as Y` is python's explicit re-export spelling — the convention type
//! checkers read as "this name is deliberately part of my public API". `export`
//! says the same thing without the repetition. the parser records the spelling
//! as [`StmtImportFrom::is_export`], so lowering is two source edits: the
//! keyword becomes `import`, and every alias gains its `as` clause

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{ModModule, Stmt, StmtImportFrom};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{AstPass, PassContext};
use super::source_util::from_import_keyword_range;

pub(crate) struct ExportImport<'src> {
    source: &'src str,
}

impl<'src> ExportImport<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl AstPass for ExportImport<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut state = State {
            source: self.source,
            edits: Vec::new(),
        };
        for stmt in &module.body {
            state.visit_stmt(stmt);
        }
        ctx.text_edits.extend(state.edits);
    }
}

struct State<'src> {
    source: &'src str,
    edits: Vec<(TextRange, String)>,
}

impl<'ast> Visitor<'ast> for State<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::ImportFrom(import) = stmt
            && import.is_export
        {
            self.lower(import);
        }
        walk_stmt(self, stmt);
    }
}

impl State<'_> {
    fn lower(&mut self, import: &StmtImportFrom) {
        let Some(keyword) = from_import_keyword_range(self.source, import) else {
            return;
        };
        self.edits.push((keyword, "import".to_owned()));

        for alias in &import.names {
            // the parser rejects `export *` and `export a as b`, so an alias
            // that already carries a name is a recovered parse — leave it be
            if alias.asname.is_none() && alias.name.id != "*" {
                self.edits.push((
                    TextRange::empty(alias.end()),
                    format!(" as {}", alias.name.id),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn single_name() {
        check("from x export y\n", "from x import y as y\n");
    }

    #[test]
    fn multiple_names() {
        check(
            "from x export a, b, c\n",
            "from x import a as a, b as b, c as c\n",
        );
    }

    #[test]
    fn dotted_module() {
        check("from a.b.c export d\n", "from a.b.c import d as d\n");
    }

    #[test]
    fn relative_module() {
        check("from .mod export y\n", "from .mod import y as y\n");
    }

    #[test]
    fn relative_no_module() {
        check("from . export y\n", "from . import y as y\n");
    }

    #[test]
    fn deep_relative_no_module() {
        check("from ... export y\n", "from ... import y as y\n");
    }

    /// a module literally named `export` must not be mistaken for the keyword
    #[test]
    fn module_named_export() {
        check("from export export y\n", "from export import y as y\n");
    }

    #[test]
    fn parenthesized() {
        check(
            indoc! {"
                from x export (
                    a,
                    b,
                )
            "},
            indoc! {"
                from x import (
                    a as a,
                    b as b,
                )
            "},
        );
    }

    #[test]
    fn inline_comment_preserved() {
        check(
            "from x export y  # noqa\n",
            "from x import y as y  # noqa\n",
        );
    }

    #[test]
    fn inside_function_body() {
        check(
            indoc! {"
                def f():
                    from x export y
            "},
            indoc! {"
                def f():
                    from x import y as y
            "},
        );
    }

    #[test]
    fn line_continuation() {
        check(
            "from x \\\n    export y\n",
            "from x \\\n    import y as y\n",
        );
    }

    #[test]
    fn plain_import_unchanged() {
        unchanged("from x import y\n");
    }

    #[test]
    fn redundant_alias_unchanged() {
        unchanged("from x import y as y\n");
    }

    #[test]
    fn export_modifier_on_class_unaffected() {
        // the `export` visibility modifier is a different construct; make sure
        // the two spellings don't interfere in one file
        check(
            indoc! {"
                from x export y
                export class C: ...
            "},
            indoc! {r#"
                from x import y as y
                class C: ...
                __all__ = ["C"]
            "#},
        );
    }
}
