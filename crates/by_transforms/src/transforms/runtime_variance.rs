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
//! the decorator goes on top, since a decorator beneath it may replace the class
//!
//! a class no transpiled module defines has no definition to wear one, and what the checker read
//! of it is in no object the runtime holds, the bases `class str(Sequence[str])` gives it above
//! all. so this collects what the runtime is to be told of each class the module reads as a type,
//! for whatever holds the program's helpers to hold
//!
//! a use-site projection is a fact about one type argument rather than about the class. it
//! stays in the argument, where the keyword is blanked out of everything else:
//! `Box[out object]` is `Box[_by_out(object)]`. an annotation keeps the plain form for whoever
//! reads it, and nothing evaluates a type parameter's bound in a way a test could see

use std::cell::RefCell;
use std::collections::BTreeMap;

use ruff_python_ast::helpers::{UseSiteVariance, use_site_variance_marker};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_semantic::{ArgVariance, MethodReceiver};

use super::ast_driver::{Fragment, Lowering, PassContext, TypeAwarePass};
use super::repeated_underscore::WrittenNames;
use super::source_util::{PrologueStatement, body_prologue, line_indent, line_start};
use crate::type_info::TypeInfo;

pub(crate) struct RuntimeVariancePass<'src> {
    pub(crate) written: WrittenNames<'src>,
    pub(crate) source: &'src str,
    pub(crate) class_variance: bool,
    pub(crate) told_classes: &'src RefCell<BTreeMap<String, String>>,
}

/// what the comment on a method's opening binding of its receiver's class starts with: what the
/// reverse direction reads the source's `Self` back from
pub(crate) const SELF_MARKER: &str = "# basedpython: self is ";

/// the binding of the class of a method's receiver, which is what its body reads for `Self`
struct SelfBinding {
    name: String,
    read: String,
    /// how the source writes `Self`
    written: String,
}

impl PrologueStatement for SelfBinding {
    fn push(&self, frags: &mut Vec<Fragment>, _indent: &str) {
        let Self {
            name,
            read,
            written,
        } = self;
        frags.push(Fragment::Lit(format!(
            "{name} = {read}  {SELF_MARKER}{written}"
        )));
    }
}

struct Telling<'a> {
    written: WrittenNames<'a>,
    source: &'a str,
    class_variance: bool,
    told_classes: &'a RefCell<BTreeMap<String, String>>,
    /// what the source's `Self` is read as where the visitor is, in a method: the name a method's
    /// opening binds its receiver's class to, which nothing the body binds can get in the way of
    receiver: Option<String>,
    /// the first `Self` the method being visited reads, as the source writes it
    self_read: Option<String>,
    /// the opening of each method that reads `Self`
    prologues: Vec<(TextSize, Vec<Fragment>)>,
    /// whether the statement being visited is directly in a class's body
    in_class_body: bool,
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
        self.tell(expr);
        let in_type = std::mem::replace(&mut self.in_type, true);
        self.visit_expr(expr);
        self.in_type = in_type;
    }

    fn tell(&self, expr: &Expr) {
        if self.class_variance {
            self.told_classes
                .borrow_mut()
                .extend(self.types.told_classes(expr));
        }
    }

    /// what the method `function` opens by reading `Self` as: its receiver's class, which is the
    /// receiver itself in a `classmethod`. python has no value for `Self` to hold, and a test can
    /// only ask about what is called through. the builtin `type` is written as the module reads
    /// it, a parameter of that name included
    fn receiver_of(&self, function: &ast::StmtFunctionDef) -> Option<String> {
        let receiver = self.types.method_receiver(function)?;
        // `def m(*args)` takes its receiver as the first of them
        let name = match function.parameters.iter_non_variadic_params().next() {
            Some(first) => first.parameter.name.to_string(),
            None => format!("{}[0]", function.parameters.vararg.as_ref()?.name),
        };
        Some(match receiver {
            MethodReceiver::Class => name,
            MethodReceiver::Instance => format!("{}({name})", self.written.builtin("type")),
        })
    }

    /// visit the body of `function`, directly in a class, and have it open by reading the class
    /// of its receiver if anything in it reads `Self`
    fn visit_method_body(&mut self, function: &'_ ast::StmtFunctionDef) {
        let receiver = self.receiver_of(function);
        let outer = std::mem::replace(
            &mut self.receiver,
            receiver.is_some().then(|| self.written.fresh("_by_self")),
        );
        let outer_read = self.self_read.take();
        self.visit_body(&function.body);
        if let (Some(written), Some(read), Some(name)) =
            (self.self_read.take(), receiver, self.receiver.take())
            && let Some(opening) = body_prologue(
                self.source,
                function,
                &[SelfBinding {
                    name,
                    read,
                    written,
                }],
            )
        {
            self.prologues.extend(opening);
        }
        self.receiver = outer;
        self.self_read = outer_read;
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
        // the runtime takes a parameter it was told nothing about for an invariant one. the python
        // written for `class C[in out T]` shows that no more than a covariant one's does, and it is
        // the decorator that is read back as the parameter's variance
        let declared_invariant = || {
            let parameters = class.type_params.as_deref()?;
            parameters
                .iter()
                .any(|parameter| {
                    matches!(
                        parameter,
                        ast::TypeParam::TypeVar(ast::TypeParamTypeVar {
                            variance: Some(ast::Variance::Invariant),
                            ..
                        })
                    )
                })
                .then(|| parameters.iter().map(|_| ArgVariance::Invariant).collect())
        };
        let Some(variances) = self
            .types
            .class_variances(class)
            .or_else(declared_invariant)
        else {
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
        if let Stmt::FunctionDef(function) = stmt {
            let in_class_body = std::mem::replace(&mut self.in_class_body, false);
            for decorator in &function.decorator_list {
                self.visit_decorator(decorator);
            }
            self.visit_parameters(&function.parameters);
            if in_class_body {
                self.visit_method_body(function);
            } else {
                self.visit_body(&function.body);
            }
            self.in_class_body = in_class_body;
            return;
        }
        let Stmt::ClassDef(class) = stmt else {
            walk_stmt(self, stmt);
            return;
        };
        self.class(class);
        if let Some(arguments) = &class.arguments {
            if self.class_variance {
                for base in &arguments.args {
                    self.told_classes
                        .borrow_mut()
                        .extend(self.types.told_classes_of_base(base));
                }
            }
        }
        for decorator in &class.decorator_list {
            self.visit_decorator(decorator);
        }
        if let Some(arguments) = &class.arguments {
            for keyword in &arguments.keywords {
                self.visit_expr(&keyword.value);
            }
        }
        let in_class_body = std::mem::replace(&mut self.in_class_body, true);
        let receiver = self.receiver.take();
        self.visit_body(&class.body);
        self.receiver = receiver;
        self.in_class_body = in_class_body;
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if self.in_type && is_written_whole(expr) {
            return;
        }
        if self.in_type
            && matches!(expr, Expr::Name(_) | Expr::Attribute(_))
            && let Some(receiver) = &self.receiver
            && self.types.reads_typing_self(expr)
        {
            self.wrappers
                .push((expr.range(), vec![Fragment::Lit(receiver.clone())]));
            self.self_read
                .get_or_insert_with(|| self.source[expr.range()].to_owned());
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
            if call.cast_kind.is_some() || self.types.is_typing_cast(&call.func) {
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
        if let Expr::Subscript(subscript) = expr
            && self.types.subscript_specializes(&subscript.value)
        {
            self.tell(&subscript.value);
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
            told_classes: self.told_classes,
            receiver: None,
            self_read: None,
            prologues: Vec::new(),
            in_class_body: false,
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
        ctx.statement_inserts.extend(telling.prologues);
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
    fn a_class_no_transpiled_module_defines_is_told_by_what_holds_the_helpers() {
        let out = out(indoc! {"
            from collections.abc import Sequence, Mapping

            def f[T](x: object) -> bool:
                return x is T

            f[Sequence[object]](1)
            a = Mapping[str, object]
        "});
        assert!(out.contains("f[Sequence[object]](1)"), "{out}");
        assert!(out.contains("a = Mapping[str, object]"), "{out}");
        assert!(!out.contains("_by_variance("), "{out}");
        // this module tests something and has nowhere else to say so
        let told = out
            .lines()
            .find(|line| line.starts_with("_by_told("))
            .expect("the module tests something");
        assert!(
            told.contains("(\"_collections_abc:Sequence\", (...,), None, "),
            "{told}"
        );
    }

    #[test]
    fn a_module_that_tests_nothing_says_nothing_of_the_classes_it_reads() {
        let out = out(
            "from collections.abc import Sequence\na = Sequence[int]\nclass A(list[str]): ...\n",
        );
        assert!(out.contains("a = Sequence[int]\n"), "{out}");
        assert!(out.contains("class A(list[str]): ..."), "{out}");
        assert!(!out.contains("_by_told"), "{out}");
        assert!(!out.contains("_by_variance"), "{out}");
    }

    #[test]
    fn a_module_with_a_file_of_helpers_says_nothing_of_the_classes_it_reads() {
        let out = transpile(
            indoc! {"
                from collections.abc import Sequence

                def f[T](x: object) -> bool:
                    return x is T

                f[Sequence[object]](1)
            "},
            &Config {
                min_version: PythonVersion::PY313,
                class_variance: true,
                runtime_module: Some("_by_runtime".to_owned()),
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(!out.contains("_by_told"), "{out}");
    }

    /// a type that names itself with a type argument that grows has no end to read
    #[test]
    fn an_alias_that_grows_each_time_it_is_opened_is_read_once() {
        for source in [
            "type G[T] = list[G[list[T]]]\nprint(1)\n",
            "type A[T] = list[B[T]]\ntype B[T] = list[A[list[T]]]\nprint(1)\n",
        ] {
            assert!(out(source).contains("print(1)"), "{source}");
        }
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
    /// never in conflict with it: each of these transpiles `written` after a shared fixture, on a
    /// target with native generics and on one without, and finds `expected` in what it writes
    fn transpiles_keeping_what_it_tells(cases: &[(&str, &str)]) {
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
        for &(written, expected) in cases {
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
    fn a_type_alias_keeps_what_it_tells() {
        transpiles_keeping_what_it_tells(&[
            ("type X = Box[out object]\n", "Box[_by_out(object)]"),
            (
                "from typing import TypeAlias\nX: TypeAlias = (Box[out int], Sequence[int])\n",
                "tuple[",
            ),
            ("type X = Out[Sequence[object]]\n", "Sequence[object]"),
            ("type X[A] = Box[out A]\n", "Box[_by_out("),
            (
                "type X = (Box[out int]) -> Sequence[int]\n",
                "Box[_by_out(int)]",
            ),
            ("X = Box[out object] | None\n", "Box[_by_out(object)]"),
        ]);
    }

    #[test]
    fn a_type_argument_of_a_value_subscript_keeps_what_it_tells() {
        transpiles_keeping_what_it_tells(&[
            (
                "a = list[(Box[out int], Sequence[int])]()\n",
                "_by_out(int)",
            ),
            ("a = list[(x: Box[out int], y: Sequence[int])]()\n", ""),
            ("a = list[Box[out int]?]()\n", "_by_out(int)"),
            ("a = list[Sequence[int]?]()\n", "Sequence[int]"),
            ("a = list[protocol(a: Sequence[int])]()\n", ""),
            ("a = list[{\"a\": Sequence[int]}]()\n", ""),
            (
                "a = list[1 | \"a\" | None]()\n",
                "list[_by_lit(1) | _by_lit(",
            ),
            ("a = list[(1, \"a\")]()\n", ""),
        ]);
    }

    #[test]
    fn a_class_header_and_a_default_keep_what_they_tell() {
        transpiles_keeping_what_it_tells(&[
            (
                "class K(list[Sequence[int]]): ...\n",
                "class K(list[Sequence[int]])",
            ),
            ("class K[A: Sequence[int]]: ...\n", "Sequence[int]"),
            ("def g(a=list[Sequence[int]]()): ...\n", "Sequence[int]"),
        ]);
    }

    #[test]
    fn a_cast_and_a_new_type_keep_what_they_tell() {
        transpiles_keeping_what_it_tells(&[
            ("a = cast(Sequence[int], [])\n", "cast(Sequence[int], [])"),
            (
                "I = NewType(\"I\", Sequence[int])\n",
                "NewType(\"I\", Sequence[int])",
            ),
            ("a = v(1) cast! Out[Sequence[object]]\n", "Sequence[object]"),
            ("a = v(1) cast? Box[out object]\n", "Box[_by_out(object)]"),
        ]);
    }

    #[test]
    fn a_type_tested_with_is_keeps_what_it_tells() {
        transpiles_keeping_what_it_tells(&[
            ("a = v(1) is (Box[out int], str)\n", ""),
            ("a = v(1) is Out[(Sequence[int], str)]\n", ""),
            (
                "a = v(1) is Out[Box[out int]] | Sequence[int]\n",
                "_by_out(int)",
            ),
        ]);
    }

    #[test]
    fn a_type_read_as_a_value_keeps_what_it_tells() {
        transpiles_keeping_what_it_tells(&[
            ("a = f\"{Sequence[int]}\"\n", "Sequence[int]"),
            ("a = f\"{Box[out int]}\"\n", "_by_out(int)"),
            ("a = [Sequence[int] for _ in [1]]\n", "Sequence[int]"),
            ("a = lambda: Box[out int]\n", "_by_out(int)"),
        ]);
    }

    #[test]
    fn a_reified_type_argument_keeps_what_it_tells() {
        transpiles_keeping_what_it_tells(&[
            ("a = holds[Box[out int]?](1)\n", "_by_out(int)"),
            ("a = holds[(Sequence[int], str)](1)\n", ""),
            ("a = holds[(int) -> Sequence[int]](1)\n", "Sequence[int]"),
            ("a = holds[(1, \"a\")](1)\n", ""),
        ]);
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
            out.contains("f[Callable[[Box[_by_out(int)], Sequence[int]], str]](1)"),
            "{out}"
        );
    }
}
