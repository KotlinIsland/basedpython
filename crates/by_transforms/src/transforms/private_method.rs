//! call sites of a `private` method (basedpython)
//!
//! `private def helper()` in a class body lowers to `def __helper()`, which
//! python name-mangles to `_A__helper` while class `A`'s body is executed. The
//! call sites keep the name the source wrote, so they have to be pointed at the
//! same attribute:
//!
//! ```by
//! class A:
//!     private def helper(self) -> int:
//!         return 1
//!
//!     def use(self) -> int:
//!         return self.helper()
//! ```
//!
//! →
//!
//! ```python
//! class A:
//!     def __helper(self) -> int:
//!         return 1
//!
//!     def use(self) -> int:
//!         return self._A__helper()
//! ```
//!
//! the mangled name is written out rather than left to python: python mangles
//! lexically, so `self.__helper` would mean `_B__helper` in a subclass's body
//! and `__helper` outside a class altogether, while `_A__helper` names the same
//! attribute from every one of those places

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::Ranged;

use super::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

pub(crate) struct PrivateMethodPass;

impl TypeAwarePass for PrivateMethodPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut renamer = Renamer {
            types,
            edits: Vec::new(),
        };
        for stmt in stmts {
            renamer.visit_stmt(stmt);
        }
        ctx.text_edits.extend(renamer.edits);
    }
}

struct Renamer<'a> {
    types: &'a dyn TypeInfo,
    edits: Vec<(ruff_text_size::TextRange, String)>,
}

impl<'ast> Visitor<'ast> for Renamer<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Attribute(attribute) = expr
            && let Some(mangled) = self.types.private_method_name(attribute)
        {
            self.edits.push((attribute.attr.range(), mangled));
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;

    fn out(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap()
    }

    #[test]
    fn a_call_reaches_the_mangled_definition() {
        let out = out(indoc! {"
            class A:
                private def helper(self) -> int:
                    return 1

                def use(self) -> int:
                    return self.helper()
        "});
        assert!(out.contains("def __helper(self) -> int:"), "got:\n{out}");
        assert!(out.contains("return self._A__helper()"), "got:\n{out}");
    }

    #[test]
    fn a_call_from_a_subclass_reaches_the_declaring_class() {
        // python would mangle `self.__helper` written here to `_B__helper`,
        // which names nothing — the declaring class is what the name records
        let out = out(indoc! {"
            class A:
                private def helper(self) -> int:
                    return 1

            class B(A):
                def use(self) -> int:
                    return self.helper()
        "});
        assert!(out.contains("return self._A__helper()"), "got:\n{out}");
    }

    #[test]
    fn an_ordinary_method_is_untouched() {
        let out = out(indoc! {"
            class A:
                def helper(self) -> int:
                    return 1

                def use(self) -> int:
                    return self.helper()
        "});
        assert!(out.contains("return self.helper()"), "got:\n{out}");
    }

    #[test]
    fn a_same_named_method_on_another_class_is_untouched() {
        let out = out(indoc! {"
            class A:
                private def helper(self) -> int:
                    return 1

            class B:
                def helper(self) -> int:
                    return 2

                def use(self) -> int:
                    return self.helper()
        "});
        assert!(out.contains("return self.helper()"), "got:\n{out}");
    }
}
