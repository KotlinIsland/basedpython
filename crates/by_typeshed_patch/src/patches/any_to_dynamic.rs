//! `Any` → `dynamic`
//!
//! `dynamic` is basedpython's spelling of `typing.Any`. reverse-transpile
//! rewrites most `Any` references, but leaves some (nested in subscripts,
//! variadics, arrow returns, `TypeVar` bounds, ...). this converts every
//! remaining `Any` reference — a bare `Any` name or a `typing.Any` attribute —
//! so no `Any` survives in the stubs. strings, comments, and docstrings are
//! untouched (they are not name references)

use std::path::Path;

use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr};
use ruff_python_ast::{Expr, ModModule};
use ruff_python_parser::Parsed;
use ruff_text_size::{Ranged, TextRange};

use crate::{Edit, Patch};

pub struct AnyToDynamic;

impl Patch for AnyToDynamic {
    fn name(&self) -> &'static str {
        "any-to-dynamic"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, parsed: &Parsed<ModModule>, _source: &str) -> Vec<Edit> {
        let mut collector = AnyRefs { ranges: Vec::new() };
        for stmt in &parsed.syntax().body {
            collector.visit_stmt(stmt);
        }
        collector
            .ranges
            .into_iter()
            .map(|range| Edit {
                start: range.start().to_usize(),
                end: range.end().to_usize(),
                replacement: "dynamic".to_string(),
            })
            .collect()
    }
}

struct AnyRefs {
    ranges: Vec<TextRange>,
}

impl<'a> SourceOrderVisitor<'a> for AnyRefs {
    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            // a bare `Any` reference
            Expr::Name(name) if name.ctx.is_load() && name.id == "Any" => {
                self.ranges.push(name.range());
                return;
            }
            // `typing.Any` / `t.Any` / `typing_extensions.Any`
            Expr::Attribute(attr)
                if attr.attr.as_str() == "Any" && is_typing_module(&attr.value) =>
            {
                self.ranges.push(attr.range());
                return;
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

/// whether `expr` names the `typing` / `typing_extensions` module (possibly via
/// a short alias like `t`)
fn is_typing_module(expr: &Expr) -> bool {
    matches!(expr, Expr::Name(name) if matches!(name.id.as_str(), "typing" | "typing_extensions" | "t" | "te"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn run(src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = AnyToDynamic.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn converts_bare_any_everywhere() {
        assert_eq!(
            run("x: Coroutine[Any, Any, int]\n"),
            "x: Coroutine[dynamic, dynamic, int]\n"
        );
        assert_eq!(run("def f() -> Any\n"), "def f() -> dynamic\n");
        assert_eq!(run("x: list[Any] | Any\n"), "x: list[dynamic] | dynamic\n");
    }

    #[test]
    fn converts_typing_any_attribute() {
        assert_eq!(run("x: typing.Any\n"), "x: dynamic\n");
        assert_eq!(run("x: t.Any\n"), "x: dynamic\n");
    }

    #[test]
    fn leaves_anystr_and_other_names() {
        let src = "x: AnyStr\ny: SupportsAny\n";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_any_attribute_on_non_typing_base() {
        // only the typing module's `.Any` converts; `.Any` on anything else stays
        let src = "x: config.Any\ny: mod.sub.Any\n";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_any_in_strings_and_docstrings() {
        let src = "class C:\n    \"\"\"Any data is fine.\"\"\"\n    x: \"Any\"\n";
        // a string annotation `"Any"` is a Literal string, not a reference
        assert_eq!(run(src), src);
    }
}
