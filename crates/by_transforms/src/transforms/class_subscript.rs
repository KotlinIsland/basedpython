//! a subscript of a class the runtime may refuse to subscript (basedpython)
//!
//! `zip`, `map`, `filter`, `reversed` and most of `itertools` are generic to the
//! checker and raise `TypeError` when subscripted in python, since a c type is only
//! subscriptable if cpython wrote it a `__class_getitem__`. `map[int]` is valid
//! basedpython, so it lowers to a helper that evaluates to what the subscript would
//! have, had the class allowed it: a `types.GenericAlias` with the same `__origin__`
//! and `__args__`
//!
//! ```by
//! a = map[int]
//! ```
//!
//! →
//!
//! ```python
//! a = _by_subscript(map, int)
//! ```
//!
//! which classes refuse is not in the stubs to be read, so the runtime answers: the
//! helper subscripts the class if the class can be subscripted, which also covers a
//! class that gained the method in a later release than the target (`memoryview`)
//!
//! this rewrites what runs as a value, the value of a `type` statement and the bound
//! or default of a type parameter included, since the polyfills evaluate those. an
//! annotation stays a type expression for whoever reads it, and is quoted instead
//! ([`AutoQuote`](super::auto_quote::AutoQuote))

use ruff_python_ast::Expr;
use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Lowering, PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

pub(crate) struct ClassSubscriptPass;

struct ClassSubscripts<'a> {
    types: &'a dyn TypeInfo,
    edits: Vec<(TextRange, String)>,
}

impl<'ast> Visitor<'ast> for ClassSubscripts<'_> {
    fn visit_annotation(&mut self, _expr: &'ast Expr) {}

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Subscript(subscript) = expr
            && !subscript.slice.is_slice_expr()
            && self.types.class_subscript_is_unattested(&subscript.value)
        {
            let (value, slice) = (subscript.value.range(), subscript.slice.range());
            self.edits.push((
                TextRange::empty(subscript.start()),
                "_by_subscript(".to_owned(),
            ));
            self.edits
                .push((TextRange::new(value.end(), slice.start()), ", ".to_owned()));
            self.edits
                .push((TextRange::new(slice.end(), subscript.end()), ")".to_owned()));
        }
        walk_expr(self, expr);
    }
}

impl TypeAwarePass for ClassSubscriptPass {
    // nothing in a stub is evaluated
    fn runtime_only(&self) -> bool {
        true
    }

    fn lowering(&self) -> Option<Lowering> {
        Some(Lowering::ClassSubscript)
    }

    fn run(&self, stmts: &[ruff_python_ast::Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut subscripts = ClassSubscripts {
            types,
            edits: Vec::new(),
        };
        subscripts.visit_body(stmts);
        if !subscripts.edits.is_empty() {
            ctx.runtime.insert(crate::runtime::SUBSCRIPT);
            ctx.text_edits.extend(subscripts.edits);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    fn out(input: &str) -> String {
        let config = Config {
            min_version: PythonVersion::PY313,
            ..Config::test_default()
        };
        transpile(input, &config).unwrap()
    }

    #[test]
    fn a_value_subscript_of_a_refusing_class_goes_through_the_helper() {
        let out = out("a = map[int]\n");
        assert!(out.contains("a = _by_subscript(map, int)\n"), "{out}");
        assert!(out.contains("def _by_subscript(origin, *args):"), "{out}");
    }

    #[test]
    fn a_subscript_nothing_doubts_is_left_alone() {
        let out = out(indoc! {"
            from collections.abc import Sequence
            a = list[int]
            b = Sequence[int]
        "});
        assert!(!out.contains("_by_subscript"), "{out}");
    }

    #[test]
    fn every_type_argument_stays_an_argument() {
        let out = out(indoc! {"
            from itertools import count
            a = list[map[int]]
            b = map[count[int]]
            c = zip[tuple[int, str]]
            d = map[int?]
        "});
        assert!(out.contains("a = list[_by_subscript(map, int)]\n"), "{out}");
        assert!(
            out.contains("b = _by_subscript(map, _by_subscript(count, int))\n"),
            "{out}"
        );
        assert!(
            out.contains("c = _by_subscript(zip, tuple[int, str])\n"),
            "{out}"
        );
        assert!(
            out.contains("d = _by_subscript(map, int | None)\n"),
            "{out}"
        );
    }

    #[test]
    fn a_base_and_a_call_are_values() {
        let out = out(indoc! {"
            class M(map[int]): ...
            a = map[str](str, [1])
        "});
        assert!(
            out.contains("class M(_by_subscript(map, int)): ...\n"),
            "{out}"
        );
        assert!(
            out.contains("a = _by_subscript(map, str)(str, [1])\n"),
            "{out}"
        );
    }

    #[test]
    fn a_type_statement_is_a_value_the_polyfill_evaluates() {
        let config = Config {
            min_version: PythonVersion::PY311,
            ..Config::test_default()
        };
        let out = transpile("type a = map[int]\n", &config).unwrap();
        assert!(
            out.contains("TypeAliasType(\"a\", _by_subscript(map, int))"),
            "{out}"
        );
    }

    #[test]
    fn a_probe_evaluates_its_target_through_the_helper() {
        let out = out("def f(x: object):\n    return x is zip[int]\n");
        assert!(
            out.contains("return _parametric_is(x, _by_subscript(zip, int), (1,))"),
            "{out}"
        );
    }

    #[test]
    fn an_annotation_stays_a_type_expression() {
        // python evaluates these three as their definitions run, so they are quoted. a local
        // variable's annotation is never evaluated
        let out = out(indoc! {"
            def f(x: map[int]) -> map[int]:
                y: map[int] = x
                return y
            z: zip[tuple[int, str]] = zip([1], [\"a\"])
        "});
        assert!(
            out.contains("def f(x: \"map[int]\") -> \"map[int]\":\n    y: map[int] = x\n"),
            "{out}"
        );
        assert!(out.contains("z: \"zip[tuple[int, str]]\" = zip"), "{out}");
        assert!(!out.contains("_by_subscript"), "{out}");
    }

    #[test]
    fn an_annotation_nothing_says_is_refused_is_not_quoted() {
        // `Generic` is underneath a protocol and under everything `typing` defines, however
        // much of that the stub leaves out
        let out = out(indoc! {"
            from typing import IO, ContextManager
            def f(a: IO[str], b: ContextManager[int]): ...
        "});
        assert!(
            out.contains("def f(a: IO[str], b: ContextManager[int])"),
            "{out}"
        );
    }
}
