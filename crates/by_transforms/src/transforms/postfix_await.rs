//! lowers the postfix `expr.await` form to a prefix `await (expr)`.
//!
//! the parser tags `expr.await` as an `ExprAwait` carrying `postfix: true`,
//! semantically identical to a prefix `await expr`. this pass rewrites the
//! surface with minimal edits that leave the operand source untouched: it
//! inserts `await ` (or `(await `) before the operand and drops the trailing
//! `.await` (or turns it into `)`). parentheses are added exactly where python
//! precedence needs them — when the awaited value is the spine of an
//! attribute / call / subscript or the operand of another `await` — so a
//! chain like `g().await.bar().await` lowers to `await (await g()).bar()`.
//!
//! keeping the operand source verbatim lets nested rewrites (`?.`, `cast`, …)
//! compose: their edits land inside the operand, clear of this pass's edits.

use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{Expr, ModModule};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, PassContext};

pub(crate) struct PostfixAwait<'src> {
    source: &'src str,
}

impl<'src> PostfixAwait<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl AstPass for PostfixAwait<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut state = State {
            edits: Vec::new(),
            source: self.source,
        };
        for stmt in &module.body {
            state.visit_stmt(stmt);
        }
        ctx.text_edits.extend(state.edits);
    }
}

struct State<'src> {
    edits: Vec<(TextRange, String)>,
    source: &'src str,
}

impl State<'_> {
    /// `needs_parens` is `true` when `expr` sits in a position that binds
    /// tighter than a prefix `await` — the spine of an attribute / call /
    /// subscript, or the operand of an `await`. a postfix `.await` found there
    /// must be parenthesised so the surrounding access applies to its result.
    fn lower(&mut self, expr: &Expr, needs_parens: bool) {
        match expr {
            Expr::Await(a) if a.postfix => {
                let node = a.range();
                let operand = a.value.range();
                let (prefix, suffix) = if needs_parens {
                    ("(await ", ")")
                } else {
                    ("await ", "")
                };
                // insert before the operand's *surface* start — the node range,
                // which includes any wrapping parens the operand AST range omits.
                // dropping into `operand.start()` left a dangling `(` for
                // `(expr).await`
                self.edits
                    .push((TextRange::empty(node.start()), prefix.to_owned()));
                // the `.await` suffix begins at the first `.` after the operand
                // (only closing parens / whitespace can intervene), so a
                // parenthesised operand keeps its closing paren
                let tail_start = usize::from(operand.end());
                let dot = self.source[tail_start..usize::from(node.end())]
                    .find('.')
                    .map_or(operand.end(), |off| {
                        operand.end() + TextSize::try_from(off).unwrap_or_default()
                    });
                self.edits
                    .push((TextRange::new(dot, node.end()), suffix.to_owned()));
                // a prefix `await` renders its operand at the highest precedence,
                // so a postfix `.await` nested in the operand needs its own parens
                self.lower(&a.value, true);
            }
            Expr::Await(a) => self.lower(&a.value, true),
            Expr::Attribute(a) => self.lower(&a.value, true),
            Expr::Subscript(s) => {
                self.lower(&s.value, true);
                self.lower(&s.slice, false);
            }
            Expr::Call(c) => {
                self.lower(&c.func, true);
                for arg in &c.arguments.args {
                    self.lower(arg, false);
                }
                for kw in &c.arguments.keywords {
                    self.lower(&kw.value, false);
                }
            }
            // every other expression renders its children at a precedence no
            // tighter than `await`, so nested `.await` there needs no parens.
            // recurse via `walk_expr`, which re-enters `visit_expr` per child
            _ => walk_expr(self, expr),
        }
    }
}

impl<'ast> Visitor<'ast> for State<'_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        // entry from `walk_expr` / `walk_stmt`: a child in a non-spine position
        self.lower(expr, false);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(transpile(input, &Config::test_default()).unwrap(), expected);
    }

    #[test]
    fn single() {
        check(
            indoc! {"
                async def f():
                    g().await
            "},
            indoc! {"
                async def f():
                    await g()
            "},
        );
    }

    #[test]
    fn chain() {
        check(
            indoc! {"
                async def f():
                    g().await.bar().await
            "},
            indoc! {"
                async def f():
                    await (await g()).bar()
            "},
        );
    }

    #[test]
    fn parenthesized_operand() {
        // the operand AST range omits wrapping parens; awaiting them must keep
        // the closing paren rather than dropping it into a dangling `(`
        check(
            indoc! {"
                async def f(c):
                    (c).await
            "},
            indoc! {"
                async def f(c):
                    await (c)
            "},
        );
    }

    #[test]
    fn parenthesized_operand_as_attribute_spine() {
        check(
            indoc! {"
                async def f():
                    (g()).await.bar
            "},
            indoc! {"
                async def f():
                    (await (g())).bar
            "},
        );
    }

    #[test]
    fn attribute_after_await() {
        check(
            indoc! {"
                async def f():
                    g().await.bar
            "},
            indoc! {"
                async def f():
                    (await g()).bar
            "},
        );
    }

    #[test]
    fn assigned_value() {
        check(
            indoc! {"
                async def f():
                    x = g().await
            "},
            indoc! {"
                async def f():
                    x = await g()
            "},
        );
    }

    #[test]
    fn as_call_argument() {
        check(
            indoc! {"
                async def f():
                    h(g().await)
            "},
            indoc! {"
                async def f():
                    h(await g())
            "},
        );
    }

    #[test]
    fn binary_operand_needs_no_parens() {
        check(
            indoc! {"
                async def f():
                    g().await + h().await
            "},
            indoc! {"
                async def f():
                    await g() + await h()
            "},
        );
    }

    #[test]
    fn subscript_after_await() {
        check(
            indoc! {"
                async def f():
                    g().await[0]
            "},
            indoc! {"
                async def f():
                    (await g())[0]
            "},
        );
    }

    #[test]
    fn call_after_await() {
        check(
            indoc! {"
                async def f():
                    g().await()
            "},
            indoc! {"
                async def f():
                    (await g())()
            "},
        );
    }

    #[test]
    fn nested_await_in_arguments() {
        // the inner `.await` sits in a call argument (a non-spine position), so
        // it needs no parens; the outer one wraps the whole call. operand source
        // is preserved, so both lower independently
        check(
            indoc! {"
                async def f():
                    outer(a().await, b).await
            "},
            indoc! {"
                async def f():
                    await outer(await a(), b)
            "},
        );
    }

    #[test]
    fn double_postfix() {
        check(
            indoc! {"
                async def f():
                    g().await.await
            "},
            indoc! {"
                async def f():
                    await (await g())
            "},
        );
    }

    #[test]
    fn prefix_await_unchanged() {
        unchanged("async def f():\n    await g()\n");
    }

    #[test]
    fn prefix_await_chain_unchanged() {
        unchanged("async def f():\n    await (await g()).bar()\n");
    }
}
