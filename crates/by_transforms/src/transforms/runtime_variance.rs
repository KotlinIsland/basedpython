//! what the runtime is told about variance (basedpython)
//!
//! a type test against a specialization, `x is Out[object]`, and the same test through a reified
//! cell, `x is T`, ask the runtime whether one type argument is a subtype of another, which turns
//! on the variance of the class. python records none for a pep 695 parameter, and `class Out[out T]`
//! lowers to one, so the checker's answer is written into the program
//!
//! a class told once is told for every test, whichever module runs it and however its target was
//! built, so a class says so where it is defined
//!
//! ```by
//! class Out[out T]: ...
//! ```
//!
//! →
//!
//! ```python
//! @_by_variance(1)
//! class Out[T]: ...
//! ```
//!
//! the decorator goes on top, since a decorator beneath it may replace the class. a class no
//! transpiled module defines has no definition to wear one, and is told wherever a module reads
//! it as a type: `Sequence[object]` is `_by_variance(1)(Sequence)[object]`. the base list of a
//! class is left out, since what a base says is read through its arguments, and a static reader
//! of the emitted python needs to be able to read a base
//!
//! a use-site projection is a fact about one type argument rather than about the class. it
//! stays in the argument, where the keyword is blanked out of everything else:
//! `Box[out object]` is `Box[_by_out(object)]`. an annotation keeps the plain form for whoever
//! reads it, and nothing evaluates a type parameter's bound in a way a test could see

use ruff_python_ast::helpers::{UseSiteVariance, use_site_variance_marker};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, PythonVersion, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_semantic::ArgVariance;

use super::ast_driver::{Fragment, Lowering, PassContext, TypeAwarePass};
use super::repeated_underscore::WrittenNames;
use super::source_util::{line_indent, line_start};
use super::type_expr_walker::is_cast_name;
use crate::type_info::TypeInfo;

pub(crate) struct RuntimeVariancePass<'src> {
    pub(crate) written: WrittenNames<'src>,
    pub(crate) source: &'src str,
    pub(crate) class_variance: bool,
    pub(crate) min_version: PythonVersion,
}

struct Telling<'a> {
    written: WrittenNames<'a>,
    source: &'a str,
    class_variance: bool,
    min_version: PythonVersion,
    types: &'a dyn TypeInfo,
    decorators: Vec<(TextRange, String)>,
    wrappers: Vec<(TextRange, Vec<Fragment>)>,
    runtime: Vec<crate::runtime::Helper>,
    /// whether the expression being visited is read as a type
    in_type: bool,
}

fn variance_call(written: WrittenNames, variances: &[ArgVariance]) -> String {
    let codes = variances
        .iter()
        .map(|variance| variance.code().to_string())
        .collect::<Vec<_>>();
    format!(
        "{}({})",
        written.helper(crate::runtime::VARIANCE),
        codes.join(", ")
    )
}

/// whether `expr`, read as a type, is one a lowering of its own writes whole: a tuple type or an
/// anonymous named tuple, a typed dict literal, an inline protocol. those that run as a value
/// write what is told themselves, and those hoisted to a class of their own annotate it
fn is_written_whole(expr: &Expr) -> bool {
    match expr {
        Expr::Tuple(tuple) => tuple.parenthesized,
        Expr::Dict(_) | Expr::ProtocolType(_) => true,
        _ => false,
    }
}

impl Telling<'_> {
    fn visit_type(&mut self, expr: &Expr) {
        let in_type = std::mem::replace(&mut self.in_type, true);
        self.visit_expr(expr);
        self.in_type = in_type;
    }

    fn wrap(&mut self, range: TextRange, opening: String) {
        self.wrappers.push((
            range,
            vec![
                Fragment::Lit(opening),
                Fragment::Src(range),
                Fragment::Lit(")".to_owned()),
            ],
        ));
    }

    fn class(&mut self, class: &ast::StmtClassDef) {
        if !self.class_variance || class.is_extension() {
            return;
        }
        let Some(variances) = self.types.class_variances(class) else {
            return;
        };
        let top = class
            .decorator_list
            .first()
            .map_or(class.start(), Ranged::start);
        let indent = line_indent(self.source, top);
        let anchor = line_start(self.source, top) + TextSize::of(indent);
        self.decorators.push((
            TextRange::empty(anchor),
            format!("@{}\n{indent}", variance_call(self.written, &variances)),
        ));
        self.runtime.push(crate::runtime::VARIANCE);
    }
}

impl<'ast> Visitor<'ast> for Telling<'_> {
    fn visit_annotation(&mut self, _expr: &'ast Expr) {}
    fn visit_type_params(&mut self, _type_params: &'ast ast::TypeParams) {}

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::TypeAlias(alias) = stmt {
            self.visit_type(&alias.value);
            return;
        }
        if let Stmt::AnnAssign(assign) = stmt
            && let Some(value) = &assign.value
            && self.types.is_type_alias_annotation(&assign.annotation)
        {
            self.visit_type(value);
            return;
        }
        let Stmt::ClassDef(class) = stmt else {
            walk_stmt(self, stmt);
            return;
        };
        self.class(class);
        for decorator in &class.decorator_list {
            self.visit_decorator(decorator);
        }
        if let Some(arguments) = &class.arguments {
            for keyword in &arguments.keywords {
                self.visit_expr(&keyword.value);
            }
        }
        self.visit_body(&class.body);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if self.in_type && is_written_whole(expr) {
            return;
        }
        if let Some((variance, inner)) = use_site_variance_marker(expr) {
            let helper = match variance {
                UseSiteVariance::Out => Some(crate::runtime::OUT),
                UseSiteVariance::In => Some(crate::runtime::IN),
                UseSiteVariance::InOut => None,
            };
            if let Some(helper) = helper {
                self.wrap(inner.range(), format!("{}(", self.written.helper(helper)));
                self.runtime.push(helper);
            }
            self.visit_expr(inner);
            return;
        }
        if let Expr::Call(call) = expr {
            // what a `cast` target and a typing construct's type arguments spell is only read
            // by what reads the construct. a checked cast's probe spells its own target
            let mut type_arguments = self.types.call_type_expression_arguments(call);
            if is_cast_name(&call.func) {
                type_arguments.extend(call.arguments.args.first());
            }
            if !type_arguments.is_empty() {
                self.visit_expr(&call.func);
                for argument in call
                    .arguments
                    .args
                    .iter()
                    .chain(call.arguments.keywords.iter().map(|keyword| &keyword.value))
                {
                    if !type_arguments
                        .iter()
                        .any(|written| argument.range().contains_range(written.range()))
                    {
                        self.visit_expr(argument);
                    }
                }
                return;
            }
        }
        // a keyword subscript is written whole, by a lowering that reads its fields as source
        if let Expr::Subscript(subscript) = expr
            && match subscript.slice.as_ref() {
                Expr::Named(named) => named.label().is_some(),
                Expr::Tuple(tuple) => tuple.elts.iter().any(
                    |element| matches!(element, Expr::Named(named) if named.label().is_some()),
                ),
                _ => false,
            }
        {
            return;
        }
        // a subscript the runtime may refuse has the helper written for it tell
        if let Expr::Subscript(subscript) = expr
            && !self
                .types
                .class_subscript_is_unattested(&subscript.value, self.min_version)
            && let Some(variances) = self.types.variances_told_at_a_subscript(&subscript.value)
        {
            self.wrap(
                subscript.value.range(),
                format!("{}(", variance_call(self.written, &variances)),
            );
            self.runtime.push(crate::runtime::VARIANCE);
        }
        if let Expr::Subscript(subscript) = expr
            && self.types.subscript_specializes(&subscript.value)
        {
            self.visit_expr(&subscript.value);
            match subscript.slice.as_ref() {
                Expr::Tuple(tuple) if !tuple.parenthesized => {
                    for element in &tuple.elts {
                        self.visit_type(element);
                    }
                }
                slice => self.visit_type(slice),
            }
            return;
        }
        walk_expr(self, expr);
    }
}

impl TypeAwarePass for RuntimeVariancePass<'_> {
    // nothing in a stub runs
    fn runtime_only(&self) -> bool {
        true
    }

    fn lowering(&self) -> Option<Lowering> {
        Some(Lowering::RuntimeVariance)
    }

    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut telling = Telling {
            written: self.written,
            source: self.source,
            class_variance: self.class_variance,
            min_version: self.min_version,
            types,
            decorators: Vec::new(),
            wrappers: Vec::new(),
            runtime: Vec::new(),
            in_type: false,
        };
        telling.visit_body(stmts);
        ctx.runtime.extend(telling.runtime);
        ctx.text_edits.extend(telling.decorators);
        ctx.template_edits.extend(telling.wrappers);
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
            class_variance: true,
            ..Config::test_default()
        };
        transpile(input, &config).unwrap()
    }

    #[test]
    fn a_class_that_varies_says_so_where_it_is_defined() {
        let out = out(indoc! {"
            class Out[out T]: ...
            class In[in T]: ...
            class Both[A, out B]:
                a: A
        "});
        assert!(out.contains("@_by_variance(1)\nclass Out[T]: ..."), "{out}");
        assert!(out.contains("@_by_variance(2)\nclass In[T]: ..."), "{out}");
        assert!(
            out.contains("@_by_variance(0, 1)\nclass Both[A, B]:"),
            "{out}"
        );
        assert!(out.contains("def _by_variance("), "{out}");
    }

    #[test]
    fn inferred_variance_is_told_as_declared_variance_is() {
        let out = out(indoc! {"
            class Inf[T]:
                def get(self) -> T: ...
        "});
        assert!(out.contains("@_by_variance(1)\nclass Inf[T]:"), "{out}");
    }

    #[test]
    fn an_invariant_class_has_nothing_to_tell() {
        let out = out(indoc! {"
            class Box[T]:
                value: T
        "});
        assert!(!out.contains("_by_variance"), "{out}");
    }

    #[test]
    fn the_decorator_is_the_outermost_one() {
        let out = out(indoc! {"
            from dataclasses import dataclass

            @dataclass(slots=True)
            class Out[out T]:
                pass
            def f():
                @dataclass
                class In[in T]:
                    pass
        "});
        assert!(
            out.contains("@_by_variance(1)\n@dataclass(slots=True)\nclass Out[T]:"),
            "{out}"
        );
        assert!(
            out.contains("    @_by_variance(2)\n    @dataclass\n    class In[T]:"),
            "{out}"
        );
    }

    #[test]
    fn every_kind_of_class_says_so() {
        for (written, expected) in [
            ("private class Out[out T]: ...\n", "class _Out[T]"),
            ("sealed class Out[out T]: ...\n", "class Out[T]"),
            ("abstract class Out[out T]: ...\n", "class Out[T]"),
            ("data class Out[out T]:\n    a: T\n", "class Out[T]"),
            ("final class Out[out T]: ...\n", "class Out[T]"),
            ("class Outer:\n    class Out[out T]: ...\n", "class Out[T]"),
            (
                "from typing import Protocol\nclass Out[out T](Protocol): ...\n",
                "class Out[T]",
            ),
            (
                "from typing import NamedTuple\nclass Out[out T](NamedTuple):\n    a: T\n",
                "class Out[T]",
            ),
            ("if True:\n    class Out[out T]: ...\n", "class Out[T]"),
        ] {
            let out = out(written);
            assert!(out.contains("@_by_variance(1)"), "{written}: {out}");
            assert!(out.contains(expected), "{written}: {out}");
        }
    }

    #[test]
    fn a_polyfilled_class_says_so_too() {
        let out = transpile(
            "class Out[out T]: ...\n",
            &Config {
                min_version: PythonVersion::PY311,
                class_variance: true,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains("@_by_variance(1)\nclass Out(Generic[_T]): ..."),
            "{out}"
        );
    }

    #[test]
    fn a_class_no_transpiled_module_defines_is_told_where_it_is_subscripted() {
        let out = out(indoc! {"
            from collections.abc import Sequence, Mapping

            def f[T](x: object) -> bool:
                return x is T

            f[Sequence[object]](1)
            a = Mapping[str, object]
        "});
        assert!(
            out.contains("f[_by_variance(1)(Sequence)[object]](1)"),
            "{out}"
        );
        assert!(
            out.contains("a = _by_variance(1, 1)(Mapping)[str, object]"),
            "{out}"
        );
        assert!(out.contains("def _by_variance("), "{out}");
    }

    #[test]
    fn a_base_and_an_annotation_are_left_for_their_readers() {
        let out = out(indoc! {"
            from collections.abc import Sequence

            class Seq[T](Sequence[T]): ...
            x: Sequence[int] = []
        "});
        assert!(
            out.contains("@_by_variance(1)\nclass Seq[T](Sequence[T]): ..."),
            "{out}"
        );
        assert!(out.contains("x: Sequence[int] = []"), "{out}");
    }

    #[test]
    fn a_projection_stays_in_its_argument() {
        let out = out(indoc! {"
            class Box[T]:
                value: T

            def f[T](x: object) -> bool:
                return x is T

            f[Box[out object]](1)
            f[Box[in object]](1)
            f[Box[in out object]](1)
            a = list[Box[out object]]
            type X = Box[out object]
            y: Box[out object] | None = None
        "});
        assert!(out.contains("f[Box[_by_out(object)]](1)"), "{out}");
        assert!(out.contains("f[Box[_by_in(object)]](1)"), "{out}");
        assert!(out.contains("f[Box[object]](1)"), "{out}");
        assert!(out.contains("a = list[Box[_by_out(object)]]"), "{out}");
        assert!(out.contains("type X = Box[_by_out(object)]"), "{out}");
        assert!(out.contains("y: Box[object] | None = None"), "{out}");
    }

    /// a lowering that writes a type whole has what is told written inside it or left out, and
    /// never in conflict with it
    #[test]
    fn every_place_a_type_is_written_transpiles() {
        const FIXTURE: &str = indoc! {"
            from collections.abc import Sequence
            from typing import NamedTuple, TypedDict, NewType, cast

            class Box[T]:
                value: T

            class Out[out T]: ...

            def holds[T](x: object) -> bool:
                return x is T

            def v(x: object) -> object:
                return x

        "};
        for (written, expected) in [
            ("type X = Box[out object]\n", "Box[_by_out(object)]"),
            (
                "from typing import TypeAlias\nX: TypeAlias = (Box[out int], Sequence[int])\n",
                "tuple[",
            ),
            (
                "type X = Out[Sequence[object]]\n",
                "_by_variance(1)(Sequence)[object]",
            ),
            ("type X[A] = Box[out A]\n", "Box[_by_out("),
            (
                "type X = (Box[out int]) -> Sequence[int]\n",
                "Box[_by_out(int)]",
            ),
            ("X = Box[out object] | None\n", "Box[_by_out(object)]"),
            (
                "a = list[(Box[out int], Sequence[int])]()\n",
                "_by_out(int)",
            ),
            ("a = list[(x: Box[out int], y: Sequence[int])]()\n", ""),
            ("a = list[Box[out int]?]()\n", "_by_out(int)"),
            (
                "a = list[Sequence[int]?]()\n",
                "_by_variance(1)(Sequence)[int]",
            ),
            ("a = list[protocol(a: Sequence[int])]()\n", ""),
            ("a = list[{\"a\": Sequence[int]}]()\n", ""),
            (
                "class K(list[Sequence[int]]): ...\n",
                "class K(list[Sequence[int]])",
            ),
            ("class K[A: Sequence[int]]: ...\n", "Sequence[int]"),
            (
                "def g(a=list[Sequence[int]]()): ...\n",
                "_by_variance(1)(Sequence)[int]",
            ),
            ("a = cast(Sequence[int], [])\n", "cast(Sequence[int], [])"),
            (
                "I = NewType(\"I\", Sequence[int])\n",
                "NewType(\"I\", Sequence[int])",
            ),
            (
                "a = v(1) cast! Out[Sequence[object]]\n",
                "_by_variance(1)(Sequence)[object]",
            ),
            ("a = v(1) cast? Box[out object]\n", "Box[_by_out(object)]"),
            ("a = v(1) is (Box[out int], str)\n", ""),
            ("a = v(1) is Out[(Sequence[int], str)]\n", ""),
            (
                "a = v(1) is Out[Box[out int]] | Sequence[int]\n",
                "_by_out(int)",
            ),
            (
                "a = f\"{Sequence[int]}\"\n",
                "_by_variance(1)(Sequence)[int]",
            ),
            ("a = f\"{Box[out int]}\"\n", "_by_out(int)"),
            (
                "a = [Sequence[int] for _ in [1]]\n",
                "_by_variance(1)(Sequence)[int]",
            ),
            ("a = lambda: Box[out int]\n", "_by_out(int)"),
            ("a = holds[Box[out int]?](1)\n", "_by_out(int)"),
            ("a = holds[(Sequence[int], str)](1)\n", ""),
            (
                "a = holds[(int) -> Sequence[int]](1)\n",
                "_by_variance(1)(Sequence)[int]",
            ),
            (
                "a = list[1 | \"a\" | None]()\n",
                "list[_by_lit(1) | _by_lit(",
            ),
            ("a = holds[(1, \"a\")](1)\n", ""),
            ("a = list[(1, \"a\")]()\n", ""),
        ] {
            for min_version in [PythonVersion::PY311, PythonVersion::PY313] {
                // a reified function needs its own syntax
                if min_version < PythonVersion::PY312 && written.contains("holds") {
                    continue;
                }
                let config = Config {
                    min_version,
                    class_variance: true,
                    ..Config::test_default()
                };
                let source = format!("{FIXTURE}{written}");
                let source = if min_version < PythonVersion::PY312 {
                    source.replace(
                        "def holds[T](x: object) -> bool:\n    return x is T\n\n",
                        "",
                    )
                } else {
                    source
                };
                let out = transpile(&source, &config)
                    .unwrap_or_else(|error| panic!("{written} at {min_version}: {error}"));
                assert!(out.contains(expected), "{written} at {min_version}: {out}");
            }
        }
    }

    #[test]
    fn a_type_written_in_a_callable_keeps_what_it_tells() {
        let out = out(indoc! {"
            from collections.abc import Sequence

            class Box[T]:
                value: T

            def f[T](x: object) -> bool:
                return x is T

            f[(Box[out int], Sequence[int]) -> str](1)
        "});
        assert!(
            out.contains(
                "f[Callable[[Box[_by_out(int)], _by_variance(1)(Sequence)[int]], str]](1)"
            ),
            "{out}"
        );
    }
}
