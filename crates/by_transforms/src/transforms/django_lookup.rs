//! Lowering for django lookups written as expressions.
//!
//! `Book.objects.filter(author.name == "Ursula", published > date(1970, 1, 1))`
//! lowers to `Book.objects.filter(author__name="Ursula",
//! published__gt=date(1970, 1, 1))`.
//!
//! Which arguments are lookups is ty's answer, not this pass's: the names in a
//! lookup path resolve to model fields rather than to anything in scope, and the
//! checker and the lowering read that from one query, so the query the file was
//! checked against is the query it lowers to. Every argument ty did not read as
//! a lookup passes through untouched.
//!
//! The value is re-emitted as a source span, so lowerings inside it still apply.

use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{Expr, Stmt};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

struct DjangoLookupLower<'a> {
    types: &'a dyn TypeInfo,
    edits: Vec<(ruff_text_size::TextRange, Vec<Fragment>)>,
}

impl<'ast> Visitor<'ast> for DjangoLookupLower<'_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            for lookup in self.types.django_lookup_arguments(call) {
                self.edits.push((
                    lookup.argument,
                    vec![
                        Fragment::Lit(format!("{}=", lookup.key)),
                        Fragment::Src(lookup.value),
                    ],
                ));
            }
        }
        walk_expr(self, expr);
    }
}

pub(crate) struct DjangoLookupPass;

impl TypeAwarePass for DjangoLookupPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = DjangoLookupLower {
            types,
            edits: Vec::new(),
        };
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        ctx.template_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use crate::{Config, transpile};

    /// A file with no django in it must come out untouched. The lowering is
    /// keyed entirely on ty resolving the callee to a lookup method on a model,
    /// so a comparison argument to any other call is an ordinary argument —
    /// which is what makes the rewrite additive rather than a rule about the
    /// word `filter`.
    #[test]
    fn a_comparison_argument_to_an_ordinary_call_is_untouched() {
        for source in [
            "def filter(*args): ...\nfilter(1 == 2)\n",
            "class M:\n    def filter(self, *args): ...\n\nM().filter(1 == 2)\n",
            indoc! {"
                def use(objects) -> None:
                    objects.filter(objects == 1)
            "},
        ] {
            let out = transpile(source, &Config::test_default()).expect("transpile should succeed");
            assert!(out.contains("== "), "for `{source}` got:\n{out}");
        }
    }
}
