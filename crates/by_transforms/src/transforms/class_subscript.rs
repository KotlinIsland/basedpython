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
//!
//! a class that answers a subscript for itself, through a metaclass `__getitem__` or a
//! `__class_getitem__` of its own, is the other way round: as a value, `U[1]` is a call of
//! that, and stays as it was written. read as a type, `chk[U[int]]` and `x is U[int]` mean the
//! specialization whatever the call would return, which goes through a helper that builds it
//!
//! ```python
//! chk[_by_specialization(U, int)]
//! ```

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt, walk_type_params};
use ruff_python_ast::{CmpOp, Expr, ExprSubscript, PythonVersion, Stmt, TypeParams};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Fragment, Lowering, PassContext, TypeAwarePass};
use super::repeated_underscore::WrittenNames;
use crate::type_info::{TypeInfo, trailing_name};

pub(crate) struct ClassSubscriptPass<'src> {
    pub(crate) written: WrittenNames<'src>,
    pub(crate) min_version: PythonVersion,
}

struct ClassSubscripts<'a> {
    written: WrittenNames<'a>,
    types: &'a dyn TypeInfo,
    min_version: PythonVersion,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    runtime: Vec<crate::runtime::Helper>,
    /// whether the expression being visited is read as a type: a type argument of a
    /// subscript that specializes its value, or the type a test asks about
    in_type: bool,
}

impl ClassSubscripts<'_> {
    fn visit_reading(&mut self, expr: &Expr, in_type: bool) {
        let previous = std::mem::replace(&mut self.in_type, in_type);
        self.visit_expr(expr);
        self.in_type = previous;
    }

    fn visit_subscript(&mut self, subscript: &ExprSubscript) {
        let specializes = self.types.subscript_specializes(&subscript.value);
        let calls_the_class = self.types.subscript_calls_the_class(&subscript.value);
        let helper = if subscript.slice.is_slice_expr() {
            None
        } else if self.in_type && specializes && calls_the_class {
            Some(crate::runtime::SPECIALIZATION)
        } else if self
            .types
            .class_subscript_is_unattested(&subscript.value, self.min_version)
        {
            Some(crate::runtime::SUBSCRIPT)
        } else {
            None
        };
        if let Some(helper) = helper {
            self.runtime.push(helper);
            // written as a whole, and without any parentheses around the value, so that a type
            // lowering writing this subscript itself leaves nothing of it behind
            self.edits.push((
                subscript.range(),
                vec![
                    Fragment::Lit(format!("{}(", self.written.helper(helper))),
                    Fragment::Src(subscript.value.range()),
                    Fragment::Lit(", ".to_owned()),
                    Fragment::Src(subscript.slice.range()),
                    Fragment::Lit(")".to_owned()),
                ],
            ));
        }
        self.visit_reading(&subscript.value, false);
        match trailing_name(&subscript.value) {
            Some("Literal") => {}
            Some("Annotated") => match subscript.slice.as_ref() {
                Expr::Tuple(tuple) if !tuple.parenthesized => {
                    for (index, element) in tuple.elts.iter().enumerate() {
                        self.visit_reading(element, specializes && index == 0);
                    }
                }
                slice => self.visit_reading(slice, specializes),
            },
            _ => self.visit_reading(&subscript.slice, specializes),
        }
    }
}

impl<'ast> Visitor<'ast> for ClassSubscripts<'_> {
    fn visit_annotation(&mut self, _expr: &'ast Expr) {}

    fn visit_type_params(&mut self, type_params: &'ast TypeParams) {
        let in_type = std::mem::replace(&mut self.in_type, true);
        walk_type_params(self, type_params);
        self.in_type = in_type;
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::TypeAlias(alias) => {
                self.visit_expr(&alias.name);
                if let Some(type_params) = &alias.type_params {
                    self.visit_type_params(type_params);
                }
                self.visit_reading(&alias.value, true);
            }
            Stmt::AnnAssign(assign) if self.types.is_type_alias_annotation(&assign.annotation) => {
                if let Some(value) = &assign.value {
                    self.visit_reading(value, true);
                }
            }
            stmt => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Subscript(subscript) => self.visit_subscript(subscript),
            // a type is written with these, and each operand is a type if it is
            Expr::BinOp(_)
            | Expr::BoolOp(_)
            | Expr::UnaryOp(_)
            | Expr::Tuple(_)
            | Expr::List(_)
            | Expr::Starred(_)
            | Expr::CallableType(_)
            | Expr::ProtocolType(_)
            | Expr::Dict(_) => walk_expr(self, expr),
            Expr::Compare(compare) => {
                self.visit_reading(compare.first_operand(), false);
                for (op, comparator) in compare.ops.iter().zip(compare.comparators()) {
                    let test =
                        matches!(op, CmpOp::Is | CmpOp::IsNot) && comparator.is_subscript_expr();
                    self.visit_reading(comparator, test);
                }
            }
            _ => {
                let in_type = std::mem::replace(&mut self.in_type, false);
                walk_expr(self, expr);
                self.in_type = in_type;
            }
        }
    }
}

impl TypeAwarePass for ClassSubscriptPass<'_> {
    // nothing in a stub is evaluated
    fn runtime_only(&self) -> bool {
        true
    }

    fn lowering(&self) -> Option<Lowering> {
        Some(Lowering::ClassSubscript)
    }

    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut subscripts = ClassSubscripts {
            written: self.written,
            types,
            min_version: self.min_version,
            edits: Vec::new(),
            runtime: Vec::new(),
            in_type: false,
        };
        subscripts.visit_body(stmts);
        ctx.runtime.extend(subscripts.runtime);
        ctx.template_edits.extend(subscripts.edits);
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
    fn a_parenthesized_value_keeps_its_parentheses_balanced() {
        let out = out("a = (map)[int]\nb = ((map))[int]\n");
        assert!(out.contains("a = _by_subscript(map, int)\n"), "{out}");
        assert!(out.contains("b = _by_subscript(map, int)\n"), "{out}");
    }

    #[test]
    fn a_class_the_stub_gates_above_the_target_goes_through_the_helper() {
        let source = "a = memoryview[int]\n";
        let at = |min_version| {
            transpile(
                source,
                &Config {
                    min_version,
                    ..Config::test_default()
                },
            )
            .unwrap()
        };
        let below = at(PythonVersion::PY313);
        assert!(
            below.contains("a = _by_subscript(memoryview, int)\n"),
            "{below}"
        );
        let from = at(PythonVersion::PY314);
        assert!(from.contains("a = memoryview[int]\n"), "{from}");
    }

    #[test]
    fn a_class_subscript_inside_a_type_the_callable_lowering_writes_goes_through_the_helper() {
        let out = out(indoc! {"
            type X = () -> map[int]
            type Y = (int, map[int])
            y = list[() -> map[int]]()
            class K(list[() -> map[int]]): ...
            z = map[() -> map[int]]
        "});
        assert!(
            out.contains("type X = Callable[[], _by_subscript(map, int)]\n"),
            "{out}"
        );
        assert!(
            out.contains("y = list[Callable[[], _by_subscript(map, int)]]()\n"),
            "{out}"
        );
        assert!(
            out.contains("class K(list[Callable[[], _by_subscript(map, int)]]): ...\n"),
            "{out}"
        );
        assert!(
            out.contains("z = _by_subscript(map, Callable[[], _by_subscript(map, int)])\n"),
            "{out}"
        );
    }

    #[test]
    fn a_class_subscript_inside_a_polyfilled_alias_goes_through_the_helper() {
        let out = transpile(
            "type X = () -> map[int]\n",
            &Config {
                min_version: PythonVersion::PY311,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains("Callable[[], _by_subscript(map, int)]"),
            "{out}"
        );
    }

    #[test]
    fn a_class_subscript_inside_an_annotation_is_quoted_with_it() {
        let out = out("def f(x: () -> map[int]) -> None: ...\n");
        assert!(!out.contains("_by_subscript"), "{out}");
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

    #[test]
    fn a_union_holding_a_class_subscript_keeps_its_parentheses_balanced() {
        let out = out("a = list[map[int] | None]\n");
        assert!(
            out.contains("a = list[_by_subscript(map, int) | None]\n"),
            "{out}"
        );
    }

    /// the checker reads a value subscript of a generic class as a specialization whatever
    /// method of its own answers it, so it is lowered as one, and that method is handed the result
    #[test]
    fn a_value_subscript_of_a_generic_class_that_answers_it_is_lowered_as_a_specialization() {
        let lowered = |header: &str| {
            out(&format!(
                "{header}\na = G[1 | 2]\nb = G[\"x\"]\nc = G[(1, 2)]\nd = G[int?]\n"
            ))
            .lines()
            .skip_while(|line| !line.starts_with("a = "))
            .collect::<Vec<_>>()
            .join("\n")
        };
        let plain = lowered("class G[T]: ...");
        assert!(plain.contains("a = G[_by_lit(1) | _by_lit(2)]"), "{plain}");
        let own =
            lowered("class G[T]:\n    def __class_getitem__(cls, item):\n        return item");
        let metaclass = lowered(
            "class M(type):\n    def __getitem__(cls, item):\n        return item\nclass G[T](metaclass=M): ...",
        );
        assert_eq!(own, plain);
        assert_eq!(metaclass, plain);
    }

    #[test]
    fn a_type_read_from_a_class_that_answers_a_subscript_is_built_without_asking_it() {
        let out = out(indoc! {"
            class G[T]:
                def __class_getitem__(cls, item):
                    return item
            def f[T](x: object) -> object:
                return T
            a = f[G[int]](1)
            b = f[G[1]](1)
            c = list[G[int]]
            d = list[G[int] | None]
            e = list[() -> G[int]]
            g = list[(G[int], str)]
            type H = G[int]
            def i(x: object) -> bool:
                return x is G[int]
        "});
        for written in [
            "a = f[_by_specialization(G, int)](1)\n",
            "b = f[_by_specialization(G, _by_lit(1))](1)\n",
            "c = list[_by_specialization(G, int)]\n",
            "d = list[_by_specialization(G, int) | None]\n",
            "e = list[Callable[[], _by_specialization(G, int)]]\n",
            "g = list[tuple[_by_specialization(G, int), str]]\n",
            "type H = _by_specialization(G, int)\n",
            "_parametric_is(x, _by_specialization(G, int), (",
        ] {
            assert!(
                out.contains(written),
                "`{written}` should be written in:\n{out}"
            );
        }
    }

    #[test]
    fn an_annotation_of_a_class_that_answers_a_subscript_is_quoted() {
        let out = out(indoc! {"
            class G[T]:
                def __class_getitem__(cls, item):
                    return cls
            def f(x: G[int]) -> None: ...
        "});
        assert!(out.contains("def f(x: \"G[int]\") -> None"), "{out}");
    }

    #[test]
    fn a_stub_that_says_nothing_of_what_a_subscript_returns_is_read_as_specializing() {
        let out = out("from multiprocessing.managers import ValueProxy\na = ValueProxy[1]\n");
        assert!(out.contains("a = ValueProxy[_by_lit(1)]\n"), "{out}");
    }

    #[test]
    fn a_class_stub_that_returns_a_specialization_specializes() {
        let out = out("a = list[1]\nfrom collections.abc import Sequence\nb = Sequence[1]\n");
        assert!(out.contains("a = list[_by_lit(1)]\n"), "{out}");
        assert!(out.contains("b = Sequence[_by_lit(1)]\n"), "{out}");
    }

    #[test]
    fn the_subscript_of_a_value_is_left_alone() {
        let source = indoc! {"
            import re
            x = range(3)
            a = x[1]
            b = \"abc\"[1]
            c = [1][0]
            d = {\"k\": 1}[\"k\"]
            def f(k: type[int], v: type):
                return (k[1], v[1])
            class K:
                @classmethod
                def m(cls):
                    return cls[1]
        "};
        let out = out(source);
        for written in ["a = x[1]\n", "return (k[1], v[1])", "return cls[1]"] {
            assert!(out.contains(written), "{out}");
        }
        assert!(!out.contains("_by_lit("), "{out}");
    }
}
