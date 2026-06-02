//! reverse of `crate::transforms::reified_generic`:
//!   `@generic  # basedpython: reified\ndef f[T]: …` → `def f[T]: …`
//!
//! the forward transform tags the decorator line it synthesizes with the
//! [`REIFIED_MARKER`](crate::transforms::reified_generic::REIFIED_MARKER)
//! comment — provenance that this `@generic` came from reification, not from a
//! user's own decorator. only a `@generic` carrying that marker is unwrapped;
//! a hand-written `@generic` (no marker) is left untouched. the `generic`
//! polyfill class and its `dataclasses` / `types` imports are dead once the
//! wrapper is removed, and `prune_imports` drops them on the way out.

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Decorator, Expr, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::transforms::reified_generic::REIFIED_MARKER;
use crate::transforms::source_util::line_start;

pub(crate) struct ReifiedGenericReverse<'src> {
    source: &'src str,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> ReifiedGenericReverse<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            edits: Vec::new(),
        }
    }

    /// the source position the `def`/`async` header begins at — either the next
    /// decorator's start, or the header keyword following this decorator
    fn next_header_start(&self, decorators: &[Decorator], idx: usize) -> Option<TextSize> {
        if let Some(next) = decorators.get(idx + 1) {
            return Some(next.range().start());
        }
        let after_dec = usize::from(decorators[idx].range().end());
        let rest = &self.source[after_dec..];
        // skip the marker comment to the newline, then to the header keyword
        let offset = rest.find("def")?;
        Some(TextSize::from(u32::try_from(after_dec + offset).ok()?))
    }

    /// whether the decorator is a bare `@generic` whose line carries the
    /// reified-provenance marker comment. the marker lives in trivia (not the
    /// AST), so it is matched against the source slice from the decorator's end
    /// to the end of its physical line
    fn is_marked_generic(&self, decorator: &Decorator) -> bool {
        if !matches!(&decorator.expression, Expr::Name(n) if n.id.as_str() == "generic") {
            return false;
        }
        let after = usize::from(decorator.range().end());
        let rest = &self.source[after..];
        let line = rest.split('\n').next().unwrap_or(rest);
        line.contains(REIFIED_MARKER.trim_start())
    }

    fn unwrap_function(&mut self, function: &StmtFunctionDef) {
        let decorators = &function.decorator_list;
        for (idx, decorator) in decorators.iter().enumerate() {
            if !self.is_marked_generic(decorator) {
                continue;
            }
            // delete from the decorator's `@` through to the next header start
            // (the following decorator, or the `def` keyword). this removes the
            // marker comment and the line break with it
            let Some(next_start) = self.next_header_start(decorators, idx) else {
                continue;
            };
            let start = line_start(self.source, decorator.range().start());
            self.edits
                .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                    start, next_start,
                ))));
        }
    }
}

impl<'ast> Visitor<'ast> for ReifiedGenericReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(function) = stmt {
            self.unwrap_function(function);
        }
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::transforms::reified_generic::REIFIED_MARKER;
    use crate::{Config, reverse_transpile};
    use ruff_python_ast::PythonVersion;

    fn rev(source: &str) -> String {
        reverse_transpile(source, &Config::test_default()).unwrap()
    }

    #[test]
    fn marked_generic_is_unwrapped() {
        let src = format!("@generic{REIFIED_MARKER}\ndef f[T]():\n    print(T)\n");
        let out = rev(&src);
        assert!(
            !out.contains("@generic"),
            "wrapper should be removed: {out}"
        );
        assert!(out.contains("def f[T]():"), "def should remain: {out}");
    }

    #[test]
    fn handwritten_generic_is_preserved() {
        // no marker — a user's own `@generic` decorator stays put
        let src = "@generic\ndef f(x):\n    return x\n";
        let out = rev(src);
        assert!(
            out.contains("@generic"),
            "hand-written decorator must be preserved: {out}"
        );
    }

    #[test]
    fn round_trip_rewraps() {
        // reverse then forward reproduces the wrapper
        let src = format!("@generic{REIFIED_MARKER}\ndef f[T]():\n    print(T)\n");
        let bare = rev(&src);
        let config = Config {
            min_version: PythonVersion::PY312,
            ..Config::test_default()
        };
        let forward = crate::transpile(&bare, &config).unwrap();
        assert!(
            forward.contains("@generic  # basedpython: reified"),
            "forward should re-wrap the bare reified def: {forward}"
        );
    }
}
