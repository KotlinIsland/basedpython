use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Stmt, StmtImportFrom};
use ruff_text_size::Ranged;

use crate::config::Config;
use ty_python_semantic::{basedpython_typing_added_in, basedpython_warnings_added_in};

/// Rewrites `from typing import X` (and `from warnings import deprecated`) to
/// `from typing_extensions import X` for names not yet in the stdlib at the
/// configured minimum Python version.
pub(crate) struct TypingRedirect<'src> {
    source: &'src str,
    config: Config,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> TypingRedirect<'src> {
    pub(crate) fn new(source: &'src str, config: Config) -> Self {
        Self {
            source,
            config,
            edits: Vec::new(),
        }
    }

    fn process_import(&mut self, node: &StmtImportFrom) {
        // Skip relative imports and star imports.
        if node.level > 0 {
            return;
        }

        let Some(module) = &node.module else {
            return;
        };
        let module_str = module.id.as_str();

        let added_in = match module_str {
            "typing" => basedpython_typing_added_in,
            "warnings" => basedpython_warnings_added_in,
            _ => return,
        };

        let mut keep: Vec<String> = Vec::new();
        let mut redirect: Vec<String> = Vec::new();

        for alias in &node.names {
            let name = alias.name.id.as_str();
            let formatted = match &alias.asname {
                Some(asname) => format!("{name} as {}", asname.id.as_str()),
                None => name.to_owned(),
            };

            if let Some(added) = added_in(name) {
                if added > self.config.min_version {
                    redirect.push(formatted);
                } else {
                    keep.push(formatted);
                }
            } else {
                keep.push(formatted);
            }
        }

        if redirect.is_empty() {
            return;
        }

        let mut parts: Vec<String> = Vec::new();
        if !keep.is_empty() {
            parts.push(format!("from {module_str} import {}", keep.join(", ")));
        }
        parts.push(format!(
            "from typing_extensions import {}",
            redirect.join(", ")
        ));

        // Preserve the original line's indentation so a split import keeps
        // the indent of any enclosing block (e.g. inside `if sys.version_info`).
        let stmt_start = usize::from(node.range().start());
        let line_start = self.source[..stmt_start]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let indent = &self.source[line_start..stmt_start];
        let separator = format!("\n{indent}");

        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            parts.join(&separator),
            node.range(),
        )));
    }
}

impl<'ast> Visitor<'ast> for TypingRedirect<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::ImportFrom(node) = stmt {
            self.process_import(node);
        }
        walk_stmt(self, stmt);
    }
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
    fn redirects_self_on_310() {
        check(
            "from typing import Self\n",
            "from typing_extensions import Self\n",
        );
    }

    #[test]
    fn keeps_typevar_redirects_self() {
        check(
            "from typing import TypeVar, Self\n",
            indoc! {"
                from typing import TypeVar
                from typing_extensions import Self
            "},
        );
    }

    #[test]
    fn no_redirect_when_already_available() {
        check(
            "from typing import TypeVar, Optional\n",
            "from typing import TypeVar, Optional\n",
        );
    }

    #[test]
    fn redirects_warnings_deprecated() {
        check(
            "from warnings import deprecated\n",
            "from typing_extensions import deprecated\n",
        );
    }

    #[test]
    fn redirects_311_names() {
        check(
            "from typing import Never, LiteralString, Unpack\n",
            "from typing_extensions import Never, LiteralString, Unpack\n",
        );
    }

    #[test]
    fn redirects_312_override() {
        check(
            "from typing import override\n",
            "from typing_extensions import override\n",
        );
    }

    #[test]
    fn split_import_preserves_indent() {
        // When splitting an indented import (e.g. inside an `if` block) into a
        // kept half and a redirected half, both lines must keep the original
        // indentation, otherwise the second line dedents and breaks parsing.
        check(
            indoc! {"
                import sys
                if sys.version_info >= (3, 14):
                    from typing import TypeVar, Self
            "},
            indoc! {"
                import sys
                if sys.version_info >= (3, 14):
                    from typing import TypeVar
                    from typing_extensions import Self
            "},
        );
    }

    #[test]
    fn no_redirect_on_matching_version() {
        let config = Config {
            min_version: ruff_python_ast::PythonVersion::PY311,
            ..Config::test_default()
        };
        assert_eq!(
            transpile("from typing import Self\n", &config).unwrap(),
            "from typing import Self\n",
        );
    }
}
