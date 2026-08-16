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
//! - a bare name in a trailing lambda block that resolves through the block's
//!   receiver → the block's receiver parameter, either on its own (`self`) or
//!   as the object a member is read off (`<receiver>.<name>`)
//!
//! Both are narrow edits nested inside the trailing-lambda template's `Src`
//! spans, so they compose with the block lowering.
//!
//! [`callable`]: super::callable

use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{Expr, ExprAttribute, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::ImplicitReceiverReference;

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::extension::{arguments_span, spine_has_optional};
use super::trailing_lambda::RECEIVER_PARAMETER;
use crate::type_info::TypeInfo;

struct ImplicitReceiverLower<'a> {
    types: &'a dyn TypeInfo,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    errors: Vec<String>,
    /// attribute ranges already rewritten as part of an enclosing call, so the
    /// bare-access arm doesn't rewrite them a second time
    handled: Vec<TextRange>,
    needs_functools: bool,
    /// precise imports for backing functions declared in another module
    imports: std::collections::BTreeSet<String>,
}

impl<'ast> Visitor<'ast> for ImplicitReceiverLower<'_> {
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
            // a bare assignment to one of the receiver's members writes the
            // member, not a name of the block's own — `href = "/x"` is
            // `self.href = "/x"`. only a member answers here: an extension adds
            // behaviour rather than state, and `self` is the receiver itself
            Expr::Name(name)
                if name.ctx.is_store()
                    && matches!(
                        self.types.implicit_receiver_name(name),
                        Some(ImplicitReceiverReference::Member)
                    ) =>
            {
                self.edits.push((
                    name.range(),
                    vec![Fragment::Lit(format!("{RECEIVER_PARAMETER}.{}", name.id))],
                ));
            }
            // the receiver of a trailing lambda block, or one of its members,
            // used unqualified inside the block
            Expr::Name(name) => {
                if name.ctx.is_load()
                    && let Some(reference) = self.types.implicit_receiver_name(name)
                {
                    let fragments = match reference {
                        ImplicitReceiverReference::Receiver => {
                            vec![Fragment::Lit(RECEIVER_PARAMETER.to_owned())]
                        }
                        ImplicitReceiverReference::Member => {
                            vec![Fragment::Lit(format!("{RECEIVER_PARAMETER}.{}", name.id))]
                        }
                        // an extension supplies the member, so there is nothing
                        // to read off the receiver — the reference is its
                        // backing function bound to the block's receiver
                        ImplicitReceiverReference::ExtensionMember(info) => {
                            if let Some(module) = &info.import_from {
                                self.imports
                                    .insert(format!("from {module} import {}", info.function));
                            }
                            let (fragments, needs_functools) =
                                super::extension::member_reference_fragments(
                                    &info,
                                    &[Fragment::Lit(RECEIVER_PARAMETER.to_owned())],
                                );
                            self.needs_functools |= needs_functools;
                            fragments
                        }
                    };
                    self.edits.push((name.range(), fragments));
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
            needs_functools: false,
            imports: std::collections::BTreeSet::new(),
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
        ctx.required_imports.extend(inner.imports);
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
    fn trailing_block_members_bind_to_the_receiver_parameter() {
        let out = check(indoc! {"
            def f(fn: int.() -> None) -> None:
                fn(1)

            f:
                print(imag)
        "});
        assert!(out.contains("print(_by_self.imag)"), "got:\n{out}");
        assert!(
            out.contains("def _trailing_lambda_0(_by_self=None, it=None):"),
            "got:\n{out}"
        );
    }

    #[test]
    fn a_trailing_block_spells_its_receiver_self() {
        let out = check(indoc! {"
            def f(fn: str.(int) -> None) -> None:
                fn(\"a\", 1)

            f:
                print(upper(), it, self)
        "});
        assert!(
            out.contains("print(_by_self.upper(), it, _by_self)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn the_receiver_outranks_an_enclosing_self() {
        // the block's receiver sits nearer than anything outside the block, so
        // `self` in the body is the receiver rather than the enclosing method's
        let out = check(indoc! {"
            def f(fn: str.() -> None) -> None:
                fn(\"a\")

            class C:
                def m(self) -> None:
                    f:
                        print(self, upper())
        "});
        assert!(
            out.contains("print(_by_self, _by_self.upper())"),
            "got:\n{out}"
        );
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
    fn a_block_local_shadows_the_receiver_spelling() {
        // a block that binds `self` itself keeps that binding — the receiver's
        // members are read off the block's own parameter, so they are unaffected
        let out = check(indoc! {"
            def f(fn: str.() -> None) -> None:
                fn(\"abc\")

            f:
                self = 5
                print(self, upper())
        "});
        assert!(out.contains("print(self, _by_self.upper())"), "got:\n{out}");
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
    fn the_receiver_outranks_a_module_global() {
        let out = check(indoc! {"
            def f(fn: int.() -> None) -> None:
                fn(1)

            imag = 2

            f:
                print(imag)
        "});
        assert!(out.contains("print(_by_self.imag)"), "got:\n{out}");
    }

    #[test]
    fn a_block_declaration_outranks_the_receiver() {
        // the only level of the scope tower inside the receiver is the block
        // itself, so a name the body *declares* keeps its own value
        let out = check(indoc! {"
            def f(fn: int.() -> None) -> None:
                fn(1)

            f:
                let imag = 2
                print(imag)
        "});
        assert!(out.contains("print(imag)"), "got:\n{out}");
        assert!(!out.contains("_by_self.imag"), "got:\n{out}");
    }

    #[test]
    fn a_bare_block_assignment_writes_the_receivers_member() {
        // a bare assignment declares nothing, so it writes the member — and the
        // reads around it go on meaning the member too
        let out = check(indoc! {"
            class Tag:
                var href: str

                def __init__(self) -> None:
                    self.href = \"\"

            def f(fn: Tag.() -> None) -> None: ...

            f:
                href = \"/x\"
                print(href)
        "});
        assert!(out.contains("_by_self.href = \"/x\""), "got:\n{out}");
        assert!(out.contains("print(_by_self.href)"), "got:\n{out}");
        // an attribute write binds no name, so the closure captures nothing
        assert!(!out.contains("nonlocal href"), "got:\n{out}");
        assert!(!out.contains("href = None"), "got:\n{out}");
    }

    #[test]
    fn a_bare_block_assignment_the_receiver_has_no_member_for_is_a_local() {
        let out = check(indoc! {"
            def f(fn: int.() -> None) -> None:
                fn(1)

            f:
                unrelated = 2
                print(unrelated)
        "});
        assert!(out.contains("print(unrelated)"), "got:\n{out}");
        assert!(!out.contains("_by_self.unrelated"), "got:\n{out}");
    }

    #[test]
    fn a_call_the_receiver_cannot_take_reaches_past_it() {
        // the receiver's `emit` takes one argument, so the two-argument call
        // walks on out to the module-level function of that name
        let out = check(indoc! {"
            class Repeater:
                def emit(self, times: int) -> None: ...

            def f(fn: Repeater.() -> None) -> None: ...

            def emit(label: str, times: int) -> None: ...

            f:
                emit(2)
                emit(\"a\", 2)
        "});
        assert!(out.contains("_by_self.emit(2)"), "got:\n{out}");
        assert!(out.contains("\n    emit(\"a\", 2)"), "got:\n{out}");
    }
}
