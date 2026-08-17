//! reverse of `crate::transforms::type_is`:
//!   `def f(a) -> TypeIs[T]:` → `def f(a) -> a is T:`
//!
//! the rewrite needs the first parameter name to reconstruct the basedpython
//! `name is T` form. `TypeIs` from `typing` or `typing_extensions` is
//! recognized

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::type_info::{TypeInfo, trailing_name};

pub(crate) struct TypeIsReverse<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> TypeIsReverse<'src> {
    pub(crate) fn new(source: &'src str, types: &'src dyn TypeInfo) -> Self {
        Self {
            source,
            types,
            edits: Vec::new(),
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    fn is_type_is(&self, expr: &Expr) -> bool {
        trailing_name(expr) == Some("TypeIs") && self.types.subscript_is_type_context(expr)
    }
}

impl<'ast> Visitor<'ast> for TypeIsReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(f) = stmt {
            if let Some(ret) = &f.returns
                && let Expr::Subscript(s) = ret.as_ref()
                && self.is_type_is(&s.value)
            {
                let first_name = f
                    .parameters
                    .posonlyargs
                    .first()
                    .map(|p| p.parameter.name.id.as_str())
                    .or_else(|| {
                        f.parameters
                            .args
                            .first()
                            .map(|p| p.parameter.name.id.as_str())
                    })
                    .unwrap_or("a");
                let inner = self.src(s.slice.range());
                // a multi-line union relied on the `TypeIs[...]` brackets for line
                // continuation; once the brackets are gone the bare `name is ...`
                // form must parenthesize it to stay a single valid expression
                // (e.g. `inspect.isroutine`). single-line types need no parens.
                let wrap = inner.contains('\n');
                // only the brackets are rewritten, so the inner type keeps both
                // its layout and any edit another transform emitted inside it —
                // rendering the whole return annotation from raw source would
                // silently undo those
                let value_end = usize::from(s.value.end());
                let open_end = TextSize::try_from(
                    value_end + self.source[value_end..].find('[').map_or(0, |i| i + 1),
                )
                .unwrap_or_else(|_| s.slice.range().start());
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    if wrap {
                        format!("{first_name} is (")
                    } else {
                        format!("{first_name} is ")
                    },
                    TextRange::new(ret.range().start(), open_end),
                )));
                let close =
                    TextRange::new(ret.range().end() - TextSize::from(1), ret.range().end());
                self.edits.push(Fix::safe_edit(if wrap {
                    Edit::range_replacement(")".to_owned(), close)
                } else {
                    Edit::range_deletion(close)
                }));
            }
            for s in &f.body {
                self.visit_stmt(s);
            }
            return;
        }
        walk_stmt(self, stmt);
    }
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

    #[test]
    fn typeis_param_annotation() {
        check(
            indoc! {"
                from typing import TypeIs
                def f(x) -> TypeIs[int]: ...
            "},
            indoc! {"
                from typing import TypeIs
                def f(x) -> x is int
            "},
        );
    }

    #[test]
    fn typeis_return_annotation_module_scope() {
        check(
            indoc! {"
                from typing import TypeIs
                def is_str(x: object) -> TypeIs[str]:
                    return isinstance(x, str)
            "},
            indoc! {"
                from typing import TypeIs
                def is_str(x: object) -> x is str:
                    return x is str
            "},
        );
    }

    #[test]
    fn unrelated_subscript_left_alone() {
        check("x: list[int]\n", "x: list[int]\n");
    }

    #[test]
    fn typeis_multiline_union_parenthesized() {
        // a multi-line union inside `TypeIs[...]` relied on the brackets for line
        // continuation; the bare `name is ...` form must parenthesize it (e.g.
        // `inspect.isroutine`), otherwise the continuation lines don't parse.
        // only the brackets are rewritten, so the union keeps its layout
        check(
            indoc! {"
                from typing import TypeIs
                def f(
                    x: object,
                ) -> TypeIs[
                    int
                    | str
                    | bytes
                ]: ...
            "},
            indoc! {"
                from typing import TypeIs
                def f(
                    x: object,
                ) -> x is (
                    int
                    | str
                    | bytes
                )
            "},
        );
    }

    /// the inner type keeps an edit another reverse transform made inside it —
    /// a whole-expression replacement rendered from raw source would undo it
    #[test]
    fn typeis_keeps_a_nested_rewrite() {
        check(
            indoc! {"
                from typing import Callable, TypeIs
                def f(x) -> TypeIs[Callable[[int], str]]: ...
            "},
            indoc! {"
                from typing import Callable, TypeIs
                def f(x) -> x is (int) -> str
            "},
        );
    }
}
