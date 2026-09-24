//! AST pass that rewrites `a is T` narrowing-predicate syntax in type
//! positions to `typing.TypeIs[T]`.
//!
//! `def f(a) -> a is int: ...` → `def f(a) -> TypeIs[int]: ...`
//!
//! basedpython surface syntax for narrowing predicates names the parameter
//! being narrowed alongside its target type. The runtime semantics are
//! identical to PEP 742 `TypeIs[T]`; the parameter name is lost in
//! lowering since `TypeIs` doesn't carry it.
//!
//! traversal is delegated to [`type_expr_walker`] (with `types = None`): the
//! `a is T` written in a *return guard* is what this rewrites, and the same
//! pair written in a body is a type test that [`parametric_is`] lowers. this
//! pass claims the guard first, so the two never rewrite the same span. the
//! walker hands every other pass the `T` in the guard's place, as the type it
//! is, so `TypeIs[...]` passes it through with everything they lower inside it

use ruff_python_ast::helpers::{ReturnGuardForm, negated_narrowing_predicate, return_guards};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, ExprCompare, ModModule, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, Fragment, PassContext};
use super::repeated_underscore::WrittenNames;
use super::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_type_positions};

pub(crate) struct TypeIs<'src> {
    src: &'src str,
    written: WrittenNames<'src>,
}

impl<'src> TypeIs<'src> {
    pub(crate) fn new(src: &'src str, written: WrittenNames<'src>) -> Self {
        Self { src, written }
    }
}

impl AstPass for TypeIs<'_> {
    /// a return guard it rewrites into `TypeIs[T]` is the `is` pair the parametric lowering
    /// would otherwise have tested
    fn subsumes(&self) -> &'static [super::ast_driver::Lowering] {
        &[super::ast_driver::Lowering::ParametricIs]
    }

    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut state = State {
            type_is: self.written.imported("typing", "TypeIs"),
            templates: Vec::new(),
            needs_import: false,
        };
        let body: &[Stmt] = &module.body;

        // narrowing annotations that name a place python can't: an assertion guard, and a
        // predicate on something other than a parameter. both lower to what the function
        // returns, and the type walk below skips them
        let mut guards = ReturnGuards {
            src: self.src,
            bool_: self.written.builtin("bool"),
            edits: Vec::new(),
            errors: Vec::new(),
        };
        for stmt in body {
            guards.visit_stmt(stmt);
        }
        ctx.text_edits.append(&mut guards.edits);
        ctx.errors.append(&mut guards.errors);

        walk_type_positions(body, None, &mut state);
        ctx.template_edits.extend(state.templates);
        if state.needs_import {
            // typing.TypeIs landed in 3.13 (PEP 742). on older runtimes the
            // typing_redirect pass switches the import to typing_extensions
            ctx.required_imports
                .push(self.written.import_from("typing", &["TypeIs"]));
        }
    }
}

struct State {
    /// the name `typing.TypeIs` is written under
    type_is: String,
    /// `TypeIs[T]` over each predicate, passing `T`'s own source through so the
    /// lowerings inside it apply
    templates: Vec<(TextRange, Vec<Fragment>)>,
    needs_import: bool,
}

/// a narrowing return annotation with no `TypeIs` spelling, which is lowered to what the
/// function returns
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplacedGuard {
    /// `def f(x) -> asserts x` raises when the assertion doesn't hold, and returns `None`
    /// when it does
    Asserts,
    /// `def f() -> a is int` narrows a place rather than an argument, and
    /// `-> self.data is str` narrows a member of one. `TypeIs` can only name a bare
    /// parameter, so these lower to the `bool` the function returns
    Predicate,
}

/// how `function`'s return annotation is replaced whole, when it is a guard `TypeIs` cannot
/// spell. nothing of such an annotation is a type in the output
pub(crate) fn replaced_guard(function: &StmtFunctionDef) -> Option<ReplacedGuard> {
    function.returns.as_ref()?;
    if function.is_asserts_return {
        return Some(ReplacedGuard::Asserts);
    }
    let guards = return_guards(function)?;
    let [guard] = guards.as_slice() else {
        return None;
    };
    if !matches!(guard.form, ReturnGuardForm::Predicate { .. }) {
        return None;
    }
    let (name, members) = guard.place_parts();
    let narrows_a_parameter = members.is_empty()
        && function
            .parameters
            .iter()
            .any(|parameter| parameter.name().id == *name);
    (!narrows_a_parameter).then_some(ReplacedGuard::Predicate)
}

/// lowers the narrowing return annotations that have no `TypeIs` spelling
struct ReturnGuards<'src> {
    src: &'src str,
    /// the name the builtin `bool` is written under
    bool_: String,
    edits: Vec<(TextRange, String)>,
    errors: Vec<String>,
}

impl ReturnGuards<'_> {
    fn function(&mut self, function: &StmtFunctionDef) {
        let Some(returns) = function.returns.as_deref() else {
            return;
        };
        // nothing narrows to everything but a type, so there is no annotation to lower it to
        if !function.is_asserts_return && negated_narrowing_predicate(returns).is_some() {
            self.errors.push(format!(
                "`{}` of `{}` cannot be lowered: a narrowing predicate cannot be negated, \
                 since `TypeIs` cannot narrow to everything but a type. write the predicate \
                 as `is` and negate the call where it is used",
                &self.src[returns.range()],
                function.name,
            ));
            return;
        }
        match replaced_guard(function) {
            // the keyword is not part of `returns`, so the edit starts at the keyword itself
            Some(ReplacedGuard::Asserts) => {
                let keyword_start = self.src[..usize::from(returns.range().start())]
                    .rfind("asserts")
                    .map(|offset| TextSize::try_from(offset).expect("offset fits u32"))
                    .unwrap_or_else(|| returns.range().start());
                self.edits.push((
                    TextRange::new(keyword_start, returns.range().end()),
                    "None".to_owned(),
                ));
            }
            Some(ReplacedGuard::Predicate) => {
                self.edits.push((returns.range(), self.bool_.clone()));
            }
            None => {}
        }
    }
}

impl<'ast> Visitor<'ast> for ReturnGuards<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(function) = stmt {
            self.function(function);
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::CallableType(callable) = expr
            && negated_narrowing_predicate(&callable.returns).is_some()
        {
            self.errors.push(format!(
                "`{}` cannot be lowered: a narrowing predicate cannot be negated, since \
                 `TypeIs` cannot narrow to everything but a type. write the predicate as `is` \
                 and negate the call where it is used",
                &self.src[callable.range()],
            ));
        }
        walk_expr(self, expr);
    }
}

impl TypeExprVisitor for State {
    fn visit(&mut self, _expr: &Expr, _pos: TypePos) -> Recurse {
        Recurse::Descend
    }

    fn visit_predicate(&mut self, predicate: &ExprCompare) {
        let [target] = &*predicate.comparators else {
            return;
        };
        self.needs_import = true;
        self.templates.push((
            predicate.range(),
            vec![
                Fragment::Lit(format!("{}[", self.type_is)),
                Fragment::Src(target.range()),
                Fragment::Lit("]".to_owned()),
            ],
        ));
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, PythonVersion, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    /// `TypeIs` narrows to the type it names, and nothing in python's typing narrows to
    /// everything but one, so a negated predicate is refused rather than lowered to the
    /// `not isinstance(...)` a negated type test in a body is. the assertion form negates
    #[test]
    fn a_negated_predicate_is_refused() {
        let error = transpile(
            "def not_int(a: object) -> a is not int:\n    return not isinstance(a, int)\n",
            &Config::test_default(),
        )
        .unwrap_err();
        assert!(
            error.contains("a narrowing predicate cannot be negated"),
            "got: {error}"
        );
        let out = transpile(
            "def check(a: object) -> asserts a is not int:\n    assert not isinstance(a, int)\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("-> None:"), "got: {out}");
    }

    /// a callable type's predicate cannot be negated either
    #[test]
    fn a_negated_predicate_a_callable_type_returns_is_refused() {
        let error = transpile(
            "def f(check: (x: object) -> (x is not int)): ...\n",
            &Config::test_default(),
        )
        .unwrap_err();
        assert!(
            error.contains("a narrowing predicate cannot be negated"),
            "got: {error}"
        );
    }

    fn check_py312(input: &str, expected: &str) {
        let config = Config {
            min_version: PythonVersion::PY312,
            ..Config::test_default()
        };
        assert_eq!(
            transpile(input, &config).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn simple() {
        check(
            "def f(a) -> a is int: ...\n",
            indoc! {"
                from typing_extensions import TypeIs
                def f(a) -> TypeIs[int]: ...
            "},
        );
    }

    #[test]
    fn lowerings_inside_the_target_apply() {
        // the target keeps its own source, so what is lowered inside it, like an
        // optional's `?`, is lowered here too
        check_py312(
            "def f(a) -> a is int?: ...\n",
            indoc! {"
                from typing_extensions import TypeIs
                def f(a) -> TypeIs[int | None]: ...
            "},
        );
        let below_310 = Config {
            min_version: PythonVersion::PY39,
            ..Config::test_default()
        };
        let out = transpile("def f(a) -> a is int?: ...\n", &below_310).unwrap();
        assert!(
            out.contains("def f(a) -> TypeIs[Union[int, None]]: ..."),
            "the optional is spelled without `|`: {out}"
        );
    }

    #[test]
    fn every_type_form_in_the_target_is_lowered() {
        // the target is lowered as the type it is, as it would be written in any annotation
        check_py312(
            indoc! {"
                def f(a) -> a is (int) -> str: ...
                def g(a) -> a is (int, str): ...
                def h(a) -> a is \"x\": ...
                def k(a) -> a is list[(int) -> str]?: ...
            "},
            indoc! {"
                from typing import Callable, Literal
                from typing_extensions import TypeIs
                def f(a) -> TypeIs[Callable[[int], str]]: ...
                def g(a) -> TypeIs[tuple[int, str]]: ...
                def h(a) -> TypeIs[Literal[\"x\"]]: ...
                def k(a) -> TypeIs[list[Callable[[int], str]] | None]: ...
            "},
        );
    }

    #[test]
    fn a_guard_lowered_to_what_it_returns_holds_no_type() {
        // the whole annotation is replaced, so nothing inside it is lowered
        check(
            indoc! {"
                a = 1
                def f() -> a is (int) -> str:
                    return True
            "},
            indoc! {"
                a = 1
                def f() -> bool:
                    return True
            "},
        );
    }

    #[test]
    fn other_param_name() {
        check(
            "def is_str(x) -> x is str: ...\n",
            indoc! {"
                from typing_extensions import TypeIs
                def is_str(x) -> TypeIs[str]: ...
            "},
        );
    }

    #[test]
    fn asserts_returns_none() {
        check(
            "def check(x: int | None) -> asserts x:\n    if x is None:\n        raise ValueError\n",
            indoc! {"
                def check(x: int | None) -> None:
                    if x is None:
                        raise ValueError
            "},
        );
    }

    #[test]
    fn negated_asserts_returns_none() {
        check(
            "def check(x: int | None) -> asserts not x: ...\n",
            indoc! {"
                def check(x: int | None) -> None: ...
            "},
        );
    }

    #[test]
    fn typed_asserts_returns_none() {
        check(
            "def check(x: int | None) -> asserts x is int: ...\n",
            indoc! {"
                def check(x: int | None) -> None: ...
            "},
        );
        check(
            "def check(x: int | None) -> asserts x is not None: ...\n",
            indoc! {"
                def check(x: int | None) -> None: ...
            "},
        );
    }

    #[test]
    fn member_guards_lower_to_what_they_return() {
        // `TypeIs` can only name a bare parameter, so a member predicate lowers to `bool`
        check(
            "class C:\n    data: str | None = None\n    def ensure(self) -> asserts self.data is not None: ...\n    def loaded(self) -> self.data is str: ...\n",
            indoc! {"
                class C:
                    data: str | None = None
                    def ensure(self) -> None: ...
                    def loaded(self) -> bool: ...
            "},
        );
    }

    #[test]
    fn several_asserted_places_return_none() {
        check(
            "def check(a: int | None, b: str | None) -> asserts a is int and b: ...\n",
            indoc! {"
                def check(a: int | None, b: str | None) -> None: ...
            "},
        );
    }

    #[test]
    fn asserts_method_returns_none() {
        check(
            "class C:\n    def check(self, y: int | None) -> asserts y: ...\n",
            indoc! {"
                class C:
                    def check(self, y: int | None) -> None: ...
            "},
        );
    }

    #[test]
    fn non_parameter_predicate_returns_bool() {
        // `TypeIs` can only name a parameter, so a predicate on a place lowers to `bool`
        check(
            "a = 1\ndef f() -> a is int:\n    return True\n",
            indoc! {"
                a = 1
                def f() -> bool:
                    return True
            "},
        );
    }

    #[test]
    fn body_is_value_unchanged() {
        unchanged("def f(a):\n    return a is None\n");
    }

    #[test]
    fn predicate_in_param_annotation() {
        // walker now exposes the predicate syntax in any type position. param
        // annotations are unusual but consistent — `x: a is int` lowers
        check(
            "def f(x: a is int): ...\n",
            indoc! {"
                from typing_extensions import TypeIs
                def f(x: TypeIs[int]): ...
            "},
        );
    }

    #[test]
    fn predicate_in_ann_assign() {
        check(
            "b: a is int\n",
            indoc! {"
                from typing_extensions import TypeIs
                b: TypeIs[int]
            "},
        );
    }

    #[test]
    fn predicate_in_type_alias_rhs() {
        check_py312(
            "type Pred = a is int\n",
            indoc! {"
                from typing_extensions import TypeIs
                type Pred = TypeIs[int]
            "},
        );
    }
}
