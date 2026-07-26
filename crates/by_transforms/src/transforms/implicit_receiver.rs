//! Lowering for implicit receivers (`int.() -> str`).
//!
//! The receiver of a receiver callable is its leading positional parameter, so
//! the callable itself needs no runtime support — the type lowers to a plain
//! `Callable[[int], str]` (see [`callable`]). What this pass lowers are the two
//! forms that read the receiver back out, both resolved by ty:
//!
//! - `x.fn()` → `fn(x)`, an attribute access ty resolved to a receiver callable
//!   in scope rather than to a member of `x`. An unapplied `x.fn` becomes a
//!   `functools.partial`, matching how a bound method would have carried the
//!   receiver
//! - a bare name in a trailing lambda block that resolves to a member of the
//!   block's receiver → `it.<name>`, where `it` is the block's implicit
//!   parameter (the receiver itself)
//!
//! Both are narrow edits nested inside the trailing-lambda template's `Src`
//! spans, so they compose with the block lowering.
//!
//! [`callable`]: super::callable

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, ExprAttribute, ExprContext, Stmt};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::extension::{arguments_span, spine_has_optional};
use crate::type_info::TypeInfo;

struct ImplicitReceiverLower<'a> {
    types: &'a dyn TypeInfo,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    errors: Vec<String>,
    /// attribute ranges already rewritten as part of an enclosing call, so the
    /// bare-access arm doesn't rewrite them a second time
    handled: Vec<TextRange>,
    /// the enclosing trailing lambda blocks, innermost last, and whether each
    /// rebinds `it` — the name the receiver members are read from
    blocks: Vec<Block>,
    needs_functools: bool,
}

/// An enclosing trailing lambda block, for the `it`-rebinding check
struct Block {
    rebinds_it: bool,
    reported: bool,
}

/// Whether a block assigns `it` anywhere in its body. The lowering reads the
/// receiver's members off that parameter, so a rebinding would silently redirect
/// them to the new value. Nested scopes count too: this only decides whether to
/// reject, and over-rejecting is the safe direction.
fn rebinds_it(body: &[Stmt]) -> bool {
    struct RebindsIt {
        found: bool,
    }

    impl<'ast> Visitor<'ast> for RebindsIt {
        fn visit_expr(&mut self, expr: &'ast Expr) {
            if let Expr::Name(name) = expr
                && name.id.as_str() == "it"
                && matches!(name.ctx, ExprContext::Store | ExprContext::Del)
            {
                self.found = true;
            }
            walk_expr(self, expr);
        }
    }

    let mut visitor = RebindsIt { found: false };
    for stmt in body {
        visitor.visit_stmt(stmt);
    }
    visitor.found
}

impl<'ast> Visitor<'ast> for ImplicitReceiverLower<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(function) = stmt
            && function.is_trailing_lambda
        {
            self.blocks.push(Block {
                rebinds_it: rebinds_it(&function.body),
                reported: false,
            });
            walk_stmt(self, stmt);
            self.blocks.pop();
            return;
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            // `x.fn(a)` → `fn(x, a)`
            Expr::Call(call) => {
                if let Expr::Attribute(attr) = call.func.as_ref()
                    && attr.ctx.is_load()
                    && self.types.is_implicit_receiver_attribute(attr)
                {
                    self.handled.push(attr.range());
                    if is_chained(attr) {
                        self.errors.push(chain_error(&attr.attr, "called"));
                    } else {
                        self.rewrite_receiver_call(call, attr);
                    }
                }
            }
            // an unapplied reference: bind the receiver the way a bound method
            // would have
            Expr::Attribute(attr) => {
                if attr.ctx.is_load()
                    && !self.handled.contains(&attr.range())
                    && self.types.is_implicit_receiver_attribute(attr)
                {
                    if is_chained(attr) {
                        self.errors.push(chain_error(&attr.attr, "accessed"));
                    } else {
                        self.needs_functools = true;
                        self.edits.push((
                            attr.range(),
                            vec![
                                Fragment::Lit(format!("functools.partial({}, ", attr.attr)),
                                Fragment::Src(attr.value.range()),
                                Fragment::Lit(")".to_owned()),
                            ],
                        ));
                    }
                }
            }
            // a receiver member used unqualified inside a trailing lambda block
            Expr::Name(name) => {
                if name.ctx.is_load() && self.types.is_implicit_receiver_name(name) {
                    if self.block_rebinds_it() {
                        self.report_rebound_it();
                    } else {
                        self.edits
                            .push((name.range(), vec![Fragment::Lit(format!("it.{}", name.id))]));
                    }
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

impl ImplicitReceiverLower<'_> {
    /// `x.fn(a)` → `fn(x, a)`. The receiver and the arguments pass through as
    /// source spans, so lowerings nested inside them still apply
    fn rewrite_receiver_call(&mut self, call: &ruff_python_ast::ExprCall, attr: &ExprAttribute) {
        let mut fragments = vec![
            Fragment::Lit(format!("{}(", attr.attr)),
            Fragment::Src(attr.value.range()),
        ];
        if let Some(span) = arguments_span(&call.arguments) {
            fragments.push(Fragment::Lit(", ".to_owned()));
            fragments.push(Fragment::Src(span));
        }
        fragments.push(Fragment::Lit(")".to_owned()));
        self.edits.push((call.range(), fragments));
    }

    /// whether the innermost enclosing block rebinds `it`, which the member
    /// rewrite reads the receiver from
    fn block_rebinds_it(&self) -> bool {
        self.blocks.last().is_some_and(|block| block.rebinds_it)
    }

    /// report the rebinding once per block, however many members it uses
    fn report_rebound_it(&mut self) {
        let Some(block) = self.blocks.last_mut() else {
            return;
        };
        if block.reported {
            return;
        }
        block.reported = true;
        self.errors.push(
            "a trailing lambda block that rebinds `it` cannot use its receiver's members \
             unqualified yet — the lowering reads them from `it`"
                .to_owned(),
        );
    }
}

/// whether the access is a link of a `?.` chain, which the receiver rewrite
/// cannot yet be spliced into — the chain lowers to its own conditional
fn is_chained(attribute: &ExprAttribute) -> bool {
    attribute.optional || spine_has_optional(&attribute.value)
}

fn chain_error(name: &str, verb: &str) -> String {
    format!("implicit receiver `{name}` cannot be {verb} through an optional chain yet")
}

pub(crate) struct ImplicitReceiverPass;

impl TypeAwarePass for ImplicitReceiverPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = ImplicitReceiverLower {
            types,
            edits: Vec::new(),
            handled: Vec::new(),
            errors: Vec::new(),
            blocks: Vec::new(),
            needs_functools: false,
        };
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        ctx.errors.extend(inner.errors);
        if inner.edits.is_empty() {
            return;
        }
        if inner.needs_functools {
            ctx.required_imports.push("import functools".to_owned());
        }
        ctx.template_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use crate::{Config, transpile};

    fn check(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap()
    }

    #[test]
    fn receiver_callable_lowers_to_leading_parameter() {
        let out = check("def f(fn: int.() -> str) -> None: ...\n");
        assert!(out.contains("fn: Callable[[int], str]"), "got:\n{out}");
    }

    #[test]
    fn call_through_the_receiver() {
        let out = check(indoc! {"
            def f(fn: int.() -> str) -> None:
                receiver = 1
                receiver.fn()
        "});
        assert!(out.contains("fn(receiver)"), "got:\n{out}");
    }

    #[test]
    fn call_through_the_receiver_keeps_arguments() {
        let out = check(indoc! {"
            def f(fn: int.(str) -> str) -> None:
                receiver = 1
                receiver.fn(\"a\")
        "});
        assert!(out.contains("fn(receiver, \"a\")"), "got:\n{out}");
    }

    #[test]
    fn unapplied_reference_becomes_partial() {
        let out = check(indoc! {"
            def f(fn: int.() -> str) -> None:
                receiver = 1
                g = receiver.fn
        "});
        assert!(
            out.contains("g = functools.partial(fn, receiver)"),
            "got:\n{out}"
        );
        assert!(out.contains("import functools"), "got:\n{out}");
    }

    #[test]
    fn trailing_block_members_bind_to_it() {
        let out = check(indoc! {"
            def f(fn: int.() -> None) -> None:
                fn(1)

            f:
                print(imag)
        "});
        assert!(out.contains("print(it.imag)"), "got:\n{out}");
    }

    #[test]
    fn optional_chain_is_rejected() {
        let error = transpile(
            indoc! {"
                def f(fn: int.() -> str, a: int?) -> None:
                    print(a?.fn())
            "},
            &Config::test_default(),
        )
        .expect_err("an optional-chained receiver call should be rejected");
        assert!(error.contains("optional chain"), "got:\n{error}");
    }

    #[test]
    fn rebinding_it_is_rejected() {
        let error = transpile(
            indoc! {"
                def f(fn: str.() -> None) -> None:
                    fn(\"abc\")

                f:
                    it = 5
                    print(upper())
            "},
            &Config::test_default(),
        )
        .expect_err("a block that rebinds `it` should be rejected");
        assert!(error.contains("rebinds `it`"), "got:\n{error}");
    }

    #[test]
    fn a_binding_shadows_a_receiver_callable() {
        // the first scope that gives the name a value decides it, so the local
        // string wins and nothing is rewritten
        let out = check(indoc! {"
            renderer: int.() -> str

            def use() -> None:
                renderer = \"shadowed\"
                x = 1
                print(x.renderer)
        "});
        assert!(out.contains("print(x.renderer)"), "got:\n{out}");
        assert!(!out.contains("renderer(x)"), "got:\n{out}");
    }

    #[test]
    fn receiver_parameter_name_avoids_collisions() {
        let out = check("f: int.(_receiver: str) -> None\n");
        assert!(
            out.contains("def __call__(self, _receiver_: \"int\", /, _receiver: \"str\")"),
            "got:\n{out}"
        );
    }

    #[test]
    fn receiver_emits_one_positional_only_marker() {
        // the receiver is positional-only, but so is any `/` the arguments
        // themselves emit — explicit, or implied by a bare positional followed
        // by a named parameter. two of them is a `SyntaxError`
        for (source, expected) in [
            (
                "f: int.(name: bytes) -> None\n",
                "self, _receiver: \"int\", /, name: \"bytes\"",
            ),
            (
                "f: int.(str, /, name: bytes) -> None\n",
                "self, _receiver: \"int\", _0: \"str\", /, name: \"bytes\"",
            ),
            (
                "f: str.(int, *, name: bytes) -> None\n",
                "self, _receiver: \"str\", _0: \"int\", /, *, name: \"bytes\"",
            ),
            (
                "f: int.(*args: str) -> None\n",
                "self, _receiver: \"int\", /, *args: \"str\"",
            ),
        ] {
            let out = check(source);
            assert!(out.contains(expected), "for `{source}` got:\n{out}");
        }
    }

    #[test]
    fn trailing_block_leaves_resolvable_names_alone() {
        let out = check(indoc! {"
            def f(fn: int.() -> None) -> None:
                fn(1)

            imag = 2

            f:
                print(imag)
        "});
        assert!(out.contains("print(imag)"), "got:\n{out}");
    }
}
