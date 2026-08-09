//! Reverse of a type mapping in a type parameter list:
//!   `class Foo[T: (int, str)]:` → `class Foo[T in (int, str)]:`
//!
//! In Python, `T: (int, str)` declares positional `TypeVar` constraints.
//! In basedpython the same `:` declares a tuple *bound*, so the constraint set
//! is written as a type mapping instead.

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, Stmt, TypeParam};
use ruff_text_size::{Ranged, TextRange};

pub(crate) struct TypeMappingReverse {
    pub(crate) edits: Vec<Fix>,
}

impl TypeMappingReverse {
    pub(crate) fn new() -> Self {
        Self { edits: Vec::new() }
    }

    fn process_type_params(&mut self, params: &[TypeParam]) {
        for param in params {
            if let TypeParam::TypeVar(tv) = param {
                if let Some(bound) = &tv.bound
                    && let Expr::Tuple(t) = bound.as_ref()
                    && t.parenthesized
                {
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        " in ".to_owned(),
                        TextRange::new(tv.name.range().end(), bound.range().start()),
                    )));
                }
            }
        }
    }
}

impl<'ast> Visitor<'ast> for TypeMappingReverse {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        let type_params = match stmt {
            Stmt::ClassDef(c) => c.type_params.as_deref(),
            Stmt::FunctionDef(f) => f.type_params.as_deref(),
            Stmt::TypeAlias(a) => a.type_params.as_deref(),
            _ => None,
        };
        if let Some(tp) = type_params {
            self.process_type_params(&tp.type_params);
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
    fn class_constraints_reversed() {
        // empty_class reverse transform also strips `...` body
        check(
            "class Foo[T: (int, str)]: ...\n",
            "class Foo[T in (int, str)]\n",
        );
    }

    #[test]
    fn function_constraints_reversed() {
        check(
            indoc! {"
                def f[T: (int, str)](x: T) -> T:
                    return x
            "},
            indoc! {"
                def f[T in (int, str)](x: T) -> T:
                    return x
            "},
        );
    }

    #[test]
    fn bound_unchanged() {
        // `T: int` is a bound, not a mapping — leave alone
        check("class Foo[T: int]: ...\n", "class Foo[T: int]\n");
    }

    #[test]
    fn no_bound_unchanged() {
        check("class Foo[T]: ...\n", "class Foo[T]\n");
    }

    #[test]
    fn multiple_params_only_mapping_rewritten() {
        check(
            "class Foo[T: (int, str), S: int]: ...\n",
            "class Foo[T in (int, str), S: int]\n",
        );
    }
}
