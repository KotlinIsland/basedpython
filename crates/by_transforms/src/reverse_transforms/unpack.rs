//! reverse of `crate::transforms::unpack`:
//!   `*args: Unpack[T]` → `*args: *T`
//!
//! only fires on vararg annotations when `Unpack` resolves to the typing import

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::type_info::TypeInfo;

pub(crate) struct UnpackReverse<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> UnpackReverse<'src> {
    pub(crate) fn new(source: &'src str, types: &'src dyn TypeInfo) -> Self {
        Self {
            source,
            types,
            edits: Vec::new(),
        }
    }

    fn is_unpack_name(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Name(n) => n.id.as_str() == "Unpack" && self.types.subscript_is_type_context(n),
            Expr::Attribute(a) => {
                a.attr.id.as_str() == "Unpack"
                    && matches!(a.value.as_ref(), Expr::Name(n) if self.types.attr_base_is_type_context(n))
            }
            _ => false,
        }
    }

    fn process_vararg_annotation(&mut self, ann: &Expr) {
        let Expr::Subscript(s) = ann else {
            return;
        };
        if !self.is_unpack_name(&s.value) {
            return;
        }
        // only the `Unpack[` and its `]` are rewritten. the inner type is a type
        // expression another reverse transform may have edited, and re-rendering
        // it from raw source would silently undo that
        let value_end = usize::from(s.value.end());
        let open_end =
            TextSize::try_from(value_end + self.source[value_end..].find('[').map_or(0, |i| i + 1))
                .unwrap_or_else(|_| s.slice.range().start());
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            "*".to_owned(),
            TextRange::new(ann.range().start(), open_end),
        )));
        self.edits
            .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                ann.range().end() - TextSize::from(1),
                ann.range().end(),
            ))));
    }
}

impl<'ast> Visitor<'ast> for UnpackReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(f) = stmt {
            if let Some(vararg) = &f.parameters.vararg {
                if let Some(ann) = &vararg.annotation {
                    self.process_vararg_annotation(ann);
                }
            }
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
    fn basic_unpack() {
        check(
            indoc! {"
                from typing import Unpack
                def f(*args: Unpack[tuple[int, ...]]): ...
            "},
            indoc! {"
                from typing import Unpack
                def f(*args: *tuple[int, ...])
            "},
        );
    }

    #[test]
    fn nested_function() {
        check(
            indoc! {"
                from typing import Unpack
                class A:
                    def method(self, *args: Unpack[tuple[str, ...]]): ...
            "},
            indoc! {"
                from typing import Unpack
                class A:
                    def method(self, *args: *tuple[str, ...])
            "},
        );
    }

    #[test]
    fn regular_arg_unchanged_by_unpack() {
        // unpack reverse leaves it alone; empty-declarations strips `: ...`
        check("def f(x: int): ...\n", "def f(x: int)\n");
    }

    /// the inner type keeps an edit another reverse transform made inside it —
    /// a whole-expression replacement rendered from raw source would undo it
    #[test]
    fn keeps_a_nested_rewrite() {
        check(
            indoc! {"
                from typing import Unpack
                def f(*args: Unpack[tuple[int, tuple[str, bytes]]]): ...
            "},
            indoc! {"
                from typing import Unpack
                def f(*args: *(int, (str, bytes)))
            "},
        );
    }

    #[test]
    fn shadowed_unchanged() {
        check(
            indoc! {"
                Unpack = object()
                def f(*args: Unpack[tuple[int, ...]]): ...
            "},
            indoc! {"
                Unpack = object()
                def f(*args: Unpack[tuple[int, ...]])
            "},
        );
    }
}
