//! Strips basedpython `local` / `once` parameter modifiers.
//!
//! `local x` marks a non-escaping (borrowed) parameter and `once fn` a callback
//! that must be called exactly once. Both are compile-time markers enforced by
//! ty's escape analysis — they carry no runtime meaning, so lowering to python
//! deletes the keyword and nothing else.
//!
//! The parser folds the keywords into `Parameter.range` without an AST field
//! (mirroring the `let` init prefix), so
//! [`parameter_modifiers`](ruff_python_ast::helpers::parameter_modifiers)
//! recovers them from the source span and yields the ranges to delete. Deleting
//! is all this pass does; when another AST pass re-renders the whole enclosing
//! statement, the keyword is dropped for free (it was never in the AST) and this
//! edit is skipped by the splice's overlap dedup.

use ruff_python_ast::helpers::parameter_modifiers;
use ruff_python_ast::visitor::{Visitor, walk_parameter};
use ruff_python_ast::{ModModule, Parameter};
use ruff_text_size::TextRange;

use super::ast_driver::{AstPass, PassContext};

struct LocalOnceStrip<'src> {
    source: &'src str,
    edits: Vec<(TextRange, String)>,
}

impl<'ast> Visitor<'ast> for LocalOnceStrip<'_> {
    fn visit_parameter(&mut self, parameter: &'ast Parameter) {
        for range in parameter_modifiers(self.source, parameter).strip_ranges {
            self.edits.push((range, String::new()));
        }
        walk_parameter(self, parameter);
    }
}

pub(crate) struct LocalOncePass<'src> {
    source: &'src str,
}

impl<'src> LocalOncePass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl AstPass for LocalOncePass<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut inner = LocalOnceStrip {
            source: self.source,
            edits: Vec::new(),
        };
        for stmt in &module.body {
            inner.visit_stmt(stmt);
        }
        ctx.text_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn local_param_stripped() {
        check(
            "def f(local fn: () -> None):\n    fn()\n",
            indoc! {"
                from typing import Callable
                def f(fn: Callable[[], None]):
                    fn()
            "},
        );
    }

    #[test]
    fn once_param_stripped() {
        check(
            "def f(once fn: () -> None):\n    fn()\n",
            indoc! {"
                from typing import Callable
                def f(fn: Callable[[], None]):
                    fn()
            "},
        );
    }

    #[test]
    fn local_on_plain_annotated_param() {
        check(
            "def f(local x: int) -> int:\n    return x\n",
            "def f(x: int) -> int:\n    return x\n",
        );
    }

    #[test]
    fn combined_once_local_stripped() {
        // both modifiers, in the documented `once local` order
        check(
            "def f(once local fn: () -> None):\n    fn()\n",
            indoc! {"
                from typing import Callable
                def f(fn: Callable[[], None]):
                    fn()
            "},
        );
    }

    #[test]
    fn let_local_prefix_strips_cleanly() {
        // `let` (an init-method modifier, kept for that transform) and `local`
        // (stripped here) edit the same parameter prefix; the two strips compose
        // without clobbering each other. (the combination is itself contradictory
        // — `let` stores on `self`, `local` forbids escape — so ty separately
        // reports `escaping-local`; that is a diagnostic, not a lowering concern)
        check(
            indoc! {"
                class A:
                    init(self, let local x: int)
            "},
            indoc! {"
                class A:
                    def __init__(self, x: int):
                        self.x: int = x
            "},
        );
    }

    #[test]
    fn modifiers_on_multiple_params() {
        check(
            "def f(local a: int, b: int, once cb: () -> None):\n    cb()\n",
            indoc! {"
                from typing import Callable
                def f(a: int, b: int, cb: Callable[[], None]):
                    cb()
            "},
        );
    }

    #[test]
    fn bare_local_param_name_untouched() {
        // a parameter literally named `local` is not a modifier
        check("def f(local): ...\n", "def f(local): ...\n");
        check("def f(once): ...\n", "def f(once): ...\n");
    }

    #[test]
    fn local_once_inside_callable_type() {
        // modifiers inside a callable-type parameter list strip to a plain
        // `Callable`, on both the first and subsequent elements
        check(
            "def g(fn: (local int) -> None):\n    fn(1)\n",
            indoc! {"
                from typing import Callable
                def g(fn: Callable[[int], None]):
                    fn(1)
            "},
        );
        check(
            "def h(fn: (local list[int], once str) -> bool): ...\n",
            indoc! {"
                from typing import Callable
                def h(fn: Callable[[list[int], str], bool]): ...
            "},
        );
    }

    #[test]
    fn callable_modifier_does_not_touch_calls_or_names() {
        // `once(x)` is a call and `(local)` a bare name — the skip only fires on
        // `local`/`once` directly followed by a name, so neither is disturbed
        let call = transpile("def f(x):\n    return once(x)\n", &Config::test_default()).unwrap();
        assert!(call.contains("once(x)"), "call mangled:\n{call}");
        let name = transpile(
            "def f(local):\n    return (local)\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            name.contains("def f(local):"),
            "param name mangled:\n{name}"
        );
        assert!(name.contains("local"), "name dropped:\n{name}");
    }

    #[test]
    fn modifier_survives_body_rerender() {
        // a body construct that forces the whole statement to be re-rendered
        // (here a `??` coalesce) must still drop the parameter modifier — the
        // keyword is never in the AST, so the re-render omits it
        let out = transpile(
            "def f(local x: int | None) -> int:\n    return x ?? 0\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(!out.contains("local"), "modifier leaked:\n{out}");
        assert!(out.contains("def f(x: int | None)"), "got:\n{out}");
    }
}
