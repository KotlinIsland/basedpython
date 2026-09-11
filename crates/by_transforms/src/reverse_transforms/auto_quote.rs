//! reverse of `crate::transforms::auto_quote` for the positions that are not
//! annotations:
//!   `"ClassName"` string in a base's subscript slice, or in a value-position
//!   subscript in the class body (`list["A"]()`) → bare name
//!
//! a base and a value subscript evaluate while the class is being built, so the
//! forward transpile quotes only the class's own name there, and only that is
//! unquoted: any other string stays one. annotation strings are unquoted before
//! any reverse transform runs, by [`super::forward_references`]

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtClassDef};
use ruff_text_size::Ranged;

pub(crate) struct AutoQuoteReverse {
    pub(crate) edits: Vec<Fix>,
}

impl AutoQuoteReverse {
    pub(crate) fn new(_source: &str) -> Self {
        Self { edits: Vec::new() }
    }

    fn process_class(&mut self, class: &StmtClassDef) {
        let class_name = class.name.id.as_str();

        if let Some(args) = &class.arguments {
            for base in &args.args {
                self.find_quoted_refs_in_annotation(base, class_name);
            }
        }

        self.unquote_self_refs_in_body(&class.body, class_name);
    }

    fn find_quoted_refs_in_annotation(&mut self, expr: &Expr, class_name: &str) {
        match expr {
            Expr::Subscript(s) => {
                self.unquote_in_type_arg(&s.slice, class_name);
            }
            Expr::BinOp(b) => {
                self.find_quoted_refs_in_annotation(&b.left, class_name);
                self.find_quoted_refs_in_annotation(&b.right, class_name);
            }
            _ => {}
        }
    }

    fn unquote_in_type_arg(&mut self, expr: &Expr, class_name: &str) {
        match expr {
            Expr::StringLiteral(s) if s.value.to_str() == class_name => {
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    class_name.to_owned(),
                    expr.range(),
                )));
            }
            Expr::Tuple(t) => {
                for e in &t.elts {
                    self.unquote_in_type_arg(e, class_name);
                }
            }
            Expr::Subscript(s) => {
                self.unquote_in_type_arg(&s.slice, class_name);
            }
            Expr::BinOp(b) => {
                self.unquote_in_type_arg(&b.left, class_name);
                self.unquote_in_type_arg(&b.right, class_name);
            }
            _ => {}
        }
    }

    fn walk_expr_for_subscripts(&mut self, expr: &Expr, class_name: &str) {
        match expr {
            Expr::Subscript(s) => {
                self.unquote_in_type_arg(&s.slice, class_name);
                self.walk_expr_for_subscripts(&s.value, class_name);
            }
            Expr::Call(c) => {
                self.walk_expr_for_subscripts(&c.func, class_name);
            }
            Expr::Attribute(a) => {
                self.walk_expr_for_subscripts(&a.value, class_name);
            }
            _ => {}
        }
    }

    fn unquote_self_refs_in_body(&mut self, stmts: &[Stmt], class_name: &str) {
        for stmt in stmts {
            match stmt {
                Stmt::Expr(e) => self.walk_expr_for_subscripts(&e.value, class_name),
                Stmt::Assign(a) => self.walk_expr_for_subscripts(&a.value, class_name),
                Stmt::AnnAssign(a) => {
                    if let Some(value) = &a.value {
                        self.walk_expr_for_subscripts(value, class_name);
                    }
                }
                _ => {}
            }
        }
    }
}

impl<'ast> Visitor<'ast> for AutoQuoteReverse {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::ClassDef(c) = stmt {
            self.process_class(c);
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
    fn simple_self_reference() {
        check("class A(list[\"A\"]): ...\n", "class A(list[A])\n");
    }

    #[test]
    fn self_ref_in_union() {
        check(
            "class A(list[\"A\" | None]): ...\n",
            "class A(list[A | None])\n",
        );
    }

    #[test]
    fn other_strings_unchanged() {
        check("class A(list[\"B\"]): ...\n", "class A(list[\"B\"])\n");
    }

    #[test]
    fn body_ann_assign() {
        check(
            indoc! {"
                class A(list[\"A\"]):
                    x: list[\"A\"] = list[\"A\"]()
            "},
            indoc! {"
                class A(list[A]):
                    x: list[A] = list[A]()
            "},
        );
    }

    #[test]
    fn body_method_annotations() {
        check(
            indoc! {"
                class A(list[\"A\"]):
                    def method(self, x: list[\"A\"]) -> list[\"A\"]: ...
            "},
            indoc! {"
                class A(list[A]):
                    def method(self, x: list[A]) -> list[A]: ...
            "},
        );
    }

    #[test]
    fn nested_class_own_name() {
        check(
            indoc! {"
                class Outer:
                    class Inner(list[\"Inner\"]): ...
            "},
            indoc! {"
                class Outer:
                    class Inner(list[Inner])
            "},
        );
    }
}
