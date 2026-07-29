//! reverse of `crate::transforms::export_import`:
//!   `from x import a as a, b as b` → `from x export a, b`
//!
//! only fires when *every* alias in the statement uses the redundant-alias
//! spelling. a mixed statement (`from x import a as a, b`) has no single-keyword
//! basedpython form, and splitting it in two would strand the statement's
//! comments, so it is left alone — a missed rewrite is a no-op, a wrong one
//! changes what the module re-exports

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Alias, Stmt, StmtImportFrom};
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::source_util::from_import_keyword_range;

pub(crate) struct ExportImportReverse<'src> {
    source: &'src str,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> ExportImportReverse<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            edits: Vec::new(),
        }
    }

    fn rewrite(&mut self, import: &StmtImportFrom) {
        if import.names.is_empty() || !import.names.iter().all(is_redundant_alias) {
            return;
        }
        let Some(keyword) = from_import_keyword_range(self.source, import) else {
            return;
        };

        let mut edits = vec![Edit::range_replacement("export".to_owned(), keyword)];
        for alias in &import.names {
            edits.push(Edit::range_deletion(TextRange::new(
                alias.name.end(),
                alias.end(),
            )));
        }
        // one fix: the keyword swap and the `as` deletions are only correct
        // together, so a range conflict must drop the whole statement's rewrite
        let (first, rest) = edits.split_first().expect("keyword edit always present");
        self.edits
            .push(Fix::safe_edits(first.clone(), rest.to_vec()));
    }
}

impl<'ast> Visitor<'ast> for ExportImportReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::ImportFrom(import) = stmt
            && !import.is_export
        {
            self.rewrite(import);
        }
        walk_stmt(self, stmt);
    }
}

/// `name as name` — python's explicit re-export spelling. a star import has no
/// `as` clause at all, so it never matches
fn is_redundant_alias(alias: &Alias) -> bool {
    alias
        .asname
        .as_ref()
        .is_some_and(|asname| asname.id == alias.name.id)
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    fn unchanged(input: &str) {
        check(input, input);
    }

    #[test]
    fn single_name() {
        check("from x import y as y\n", "from x export y\n");
    }

    #[test]
    fn multiple_names() {
        check("from x import a as a, b as b\n", "from x export a, b\n");
    }

    #[test]
    fn dotted_module() {
        check("from a.b.c import d as d\n", "from a.b.c export d\n");
    }

    #[test]
    fn relative_module() {
        check("from .mod import y as y\n", "from .mod export y\n");
    }

    #[test]
    fn relative_no_module() {
        check("from . import y as y\n", "from . export y\n");
    }

    #[test]
    fn module_named_import() {
        check("from import_ import y as y\n", "from import_ export y\n");
    }

    #[test]
    fn parenthesized() {
        check(
            indoc! {"
                from x import (
                    a as a,
                    b as b,
                )
            "},
            indoc! {"
                from x export (
                    a,
                    b,
                )
            "},
        );
    }

    #[test]
    fn inline_comment_preserved() {
        check(
            "from x import y as y  # noqa\n",
            "from x export y  # noqa\n",
        );
    }

    #[test]
    fn inside_function_body() {
        check(
            indoc! {"
                def f():
                    from x import y as y
            "},
            indoc! {"
                def f():
                    from x export y
            "},
        );
    }

    #[test]
    fn mixed_aliases_unchanged() {
        // no single-keyword basedpython spelling covers a partial re-export
        unchanged("from x import a as a, b\n");
    }

    #[test]
    fn renaming_alias_unchanged() {
        unchanged("from x import a as b\n");
    }

    #[test]
    fn plain_import_unchanged() {
        unchanged("from x import y\n");
    }

    #[test]
    fn star_import_unchanged() {
        unchanged("from x import *\n");
    }

    #[test]
    fn module_import_unchanged() {
        // `import x as x` is a module re-export, a different statement kind
        unchanged("import x as x\n");
    }

    /// the two directions compose: reversing then transpiling is the identity
    #[test]
    fn round_trips() {
        let python = "from x import a as a, b as b\n";
        let by = reverse_transpile(python, &Config::test_default()).unwrap();
        assert_eq!(by, "from x export a, b\n");
        assert_eq!(
            crate::transpile(&by, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(python)
        );
    }
}
