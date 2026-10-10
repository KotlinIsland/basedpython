//! type reification (basedpython)
//!
//! at runtime, `A(1)` constructs an `A` whose specialization is invisible —
//! nothing records that this was an `A[int]`. the transpiler makes the
//! inferred specialization of a *user-defined generic* constructor explicit
//! in the generated python:
//!
//! ```by
//! class A[T]:
//!     def __init__(self, t: T): ...
//!
//! a = A(1)
//! ```
//!
//! →
//!
//! ```python
//! a = A[int](1)
//! ```
//!
//! `A[int](…)` routes through `types.GenericAlias.__call__`, which stamps
//! `__orig_class__` on the constructed instance, so the specialization is
//! observable at runtime. builtin collection literals are *not* reified: a
//! `list` / `dict` / `set` / `tuple` silently drops the `__orig_class__`
//! stamp, so `list[int]([1, 2])` is runtime-identical to `[1, 2]` — the wrap
//! would be pure bloat.
//!
//! reification is best-effort: it fires only when ty solved the
//! specialization to types with a runtime spelling (see ty's `reified_infer`
//! module) — dynamic, unsolved or scope-local arguments leave the call as
//! written. it also fires only where the class accepts a subscript at runtime:
//! `zip` and `map` are generic in the stub but unsubscriptable in cpython, so
//! `zip(a, b)` reaches the output as it was written
//!
//! type positions never reify (annotations, type parameter lists,
//! `type X = …` values, type-context subscript slices such as legacy
//! `Callable[[int], str]` parameter lists), and dunders that static readers
//! require to stay literal displays (`__all__`, `__slots__`, `__match_args__`)
//! are skipped. pep 585 makes the builtins subscriptable at runtime in 3.9, so
//! the pass is inert below that target, and stubs have no runtime to observe,
//! so stub sources are left alone
//!
//! # TODO: a subscript the *source* wrote on an unsubscriptable class
//!
//! this pass only declines to *invent* a subscript. one the source writes
//! itself is copied through, and for a class the runtime won't subscript that
//! is python which raises on the line it appears:
//!
//! ```text
//! a = zip[int]                # a = zip[int]
//! type a = zip[int]           # TypeAliasType("a", zip[int]) — the polyfill
//!                             #   evaluates its value, unlike native pep 695
//! def f(x: zip[int]): ...     # crashes on a 3.10–3.13 target, where no
//!                             #   `from __future__ import annotations` is emitted
//! ```
//!
//! this is faithful — the same three lines raise in plain python — so it is a
//! missing diagnostic rather than a bad lowering. but a lowering is available
//! and is probably the better answer: emit a generic *alias* object instead of
//! subscripting the class, the way `typing.List` stood in for a `list` nobody
//! could subscript before 3.9. the alias carries the same `__origin__` and
//! `__args__` a real `zip[int]` would have, so every static reader and the
//! `_parametric_is` probe keep working, and nothing evaluates a subscript the
//! class does not support
//!
//! two things to settle first. which spelling — a `typing` alias where one
//! exists, else a constructed `types.GenericAlias(zip, (int,))`, which is
//! exactly the object the subscript would have produced. and which positions
//! are worth rewriting, since an annotation is already inert wherever the
//! `from __future__ import annotations` preamble is emitted, and pep 649 makes
//! it inert on 3.14+ regardless
//!
//! either way it needs to know *which* classes the runtime refuses, which is
//! the open question on `RuntimeSubscript::Unknown` in ty's `reified_infer`

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, PythonVersion, Stmt};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{PassContext, TypeAwarePass};
use super::repeated_underscore::WrittenNames;
use crate::type_info::{TypeInfo, trailing_name};

/// dunder assignments whose values static readers (linters, type checkers,
/// dataclass machinery) require to stay literal displays
fn is_static_literal_dunder(target: &Expr) -> bool {
    matches!(
        target,
        Expr::Name(name) if matches!(name.id.as_str(), "__all__" | "__slots__" | "__match_args__")
    )
}

/// `sys.version_info` (or a slice of it) — comparisons against it are a
/// structural idiom every static reader (including ty on the generated
/// python) must see with a literal tuple operand
fn is_version_info(expr: &Expr) -> bool {
    match expr {
        Expr::Attribute(attribute) => {
            attribute.attr.as_str() == "version_info"
                && matches!(attribute.value.as_ref(), Expr::Name(name) if name.id.as_str() == "sys")
        }
        Expr::Subscript(subscript) => is_version_info(&subscript.value),
        _ => false,
    }
}

struct Reifier<'ti> {
    types: &'ti dyn TypeInfo,
    written: WrittenNames<'ti>,
    edits: Vec<(TextRange, String)>,
}

impl<'ast> Visitor<'ast> for Reifier<'_> {
    // type positions never reify
    fn visit_annotation(&mut self, _expr: &'ast Expr) {}
    fn visit_type_params(&mut self, _type_params: &'ast ast::TypeParams) {}

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            // a `type X = …` value is a type expression
            Stmt::TypeAlias(_) => {}
            Stmt::Assign(assign) if assign.targets.iter().any(is_static_literal_dunder) => {}
            Stmt::AugAssign(assign) if is_static_literal_dunder(&assign.target) => {}
            // a function's parameter list never reifies: annotations are type
            // positions, scalar defaults contain no displays, and every
            // non-scalar default is consumed whole by the mutable-defaults
            // lowering (swapped for `_MISSING`, re-evaluated verbatim in a
            // body guard). lambda parameters keep their defaults and walk
            // normally
            Stmt::FunctionDef(function) => {
                for decorator in &function.decorator_list {
                    self.visit_decorator(decorator);
                }
                self.visit_body(&function.body);
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Call(call) => {
                // bare constructor call of a generic class: make the solved
                // specialization explicit. an explicit `A[int](…)` (subscript
                // callee) already carries it
                if !matches!(call.func.as_ref(), Expr::Subscript(_))
                    && let Some(arguments) = self.types.constructor_specialization(call)
                {
                    self.edits.push((
                        TextRange::empty(call.func.range().end()),
                        format!("[{}]", self.written.spelled(&arguments)),
                    ));
                }
                walk_expr(self, expr);
            }
            Expr::Compare(compare)
                if is_version_info(&compare.left)
                    || compare.comparators.iter().any(is_version_info) => {}
            Expr::Subscript(subscript) => {
                // in a type-context subscript (`dict[str, int]`, legacy
                // `Callable[[int], str]`) the slice is a type expression, and
                // a display there is type syntax, not a value
                let type_context = trailing_name(subscript.value.as_ref()).is_some()
                    && self
                        .types
                        .subscript_is_type_context(subscript.value.as_ref());
                if type_context {
                    self.visit_expr(&subscript.value);
                } else {
                    self.visit_expr(&subscript.value);
                    self.visit_expr(&subscript.slice);
                }
            }
            _ => walk_expr(self, expr),
        }
    }
}

pub(crate) struct TypeReificationPass<'src> {
    written: WrittenNames<'src>,
    min_version: PythonVersion,
}

impl<'src> TypeReificationPass<'src> {
    pub(crate) fn new(written: WrittenNames<'src>, min_version: PythonVersion) -> Self {
        Self {
            written,
            min_version,
        }
    }
}

impl TypeAwarePass for TypeReificationPass<'_> {
    // the specialization is spelled out so the value a call constructs
    // carries it as the program runs
    fn runtime_only(&self) -> bool {
        true
    }

    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        if self.min_version < PythonVersion::PY39 {
            return;
        }
        let mut reifier = Reifier {
            types,
            written: self.written,
            edits: Vec::new(),
        };
        for stmt in stmts {
            reifier.visit_stmt(stmt);
        }
        ctx.text_edits.extend(reifier.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile, transpile_typed};
    use indoc::indoc;
    use ruff_db::files::system_path_to_file;
    use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
    use ruff_python_ast::PythonVersion;
    use ty_project::{ProjectMetadata, TestDb};

    fn out(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap()
    }

    /// transpile against a typeshed resolved at `version`, so a member that
    /// typeshed gates on `sys.version_info` — such as the `__class_getitem__`
    /// that makes a class subscriptable at runtime — answers for that target
    fn out_checked_at(input: &str, version: PythonVersion) -> String {
        let mut db = TestDb::new(ProjectMetadata::new(
            ruff_python_ast::name::Name::new_static(""),
            SystemPathBuf::from("/"),
        ));
        db.write_file("/input.by", input)
            .expect("write file failed");
        db.init_program_with_python_version(version)
            .expect("program init failed");
        let file = system_path_to_file(&db, "/input.by").expect("file not in db");
        transpile_typed(
            &db,
            file,
            &Config {
                min_version: version,
                ..Config::test_default()
            },
            None,
        )
        .expect("transpile failed")
    }

    fn out_at(input: &str, version: PythonVersion) -> String {
        transpile(
            input,
            &Config {
                min_version: version,
                ..Config::test_default()
            },
        )
        .unwrap()
    }

    #[test]
    fn generic_constructor_gets_inferred_specialization() {
        let out = out_at(
            indoc! {"
                class A[T]:
                    def __init__(self, t: T): ...
                a = A(1)
                b = A(\"x\")
            "},
            PythonVersion::PY312,
        );
        assert!(
            out.contains("a = A[int](1)"),
            "int ctor should inject: {out}"
        );
        assert!(
            out.contains("b = A[str](\"x\")"),
            "str ctor should inject: {out}"
        );
    }

    #[test]
    fn explicit_constructor_specialization_unchanged() {
        let out = out_at(
            indoc! {"
                class A[T]:
                    def __init__(self, t: T): ...
                a = A[int](1)
            "},
            PythonVersion::PY312,
        );
        assert!(
            out.contains("a = A[int](1)"),
            "explicit spec should be untouched: {out}"
        );
        assert!(!out.contains("A[int][int]"), "no double injection: {out}");
    }

    #[test]
    fn declared_variance_beside_an_inferred_parameter() {
        // a parameter used only by `__init__` is bivariant, which constrains
        // nothing — its argument must still reach the specialization
        let out = out_at(
            indoc! {"
                class A[out T, U]:
                    def __init__(self, t: T, u: U): ...
                a = A(1, \"x\")
            "},
            PythonVersion::PY312,
        );
        assert!(
            out.contains("a = A[int, str](1, \"x\")"),
            "both arguments should reify: {out}"
        );
    }

    /// `float` in basedpython is just `float`, so it is what a construction is specialized
    /// with, not python's `int | float`. a covariant argument goes through the promotion
    /// that would apply python's special case
    #[test]
    fn a_float_argument_specializes_as_just_float() {
        let out = out_at(
            indoc! {"
                class A[out T]:
                    def __init__(self, t: T): ...
                def f(y: float):
                    a = A(1.5)
                    b = A(y)
            "},
            PythonVersion::PY312,
        );
        assert!(out.contains("a = A[float](1.5)"), "got: {out}");
        assert!(out.contains("b = A[float](y)"), "got: {out}");
    }

    /// backlog N15 #1, constructor calls still widen `float` in `.by`: pinned so a fix shows up
    /// here. the checker solves an invariant parameter from a `float` variable through python's
    /// numeric special case, so the construction is typed `A[int | float]` and specialized that
    /// way. the literal is right
    #[test]
    fn an_invariant_float_argument_still_specializes_as_int_or_float() {
        let out = out_at(
            indoc! {"
                class A[T]:
                    def __init__(self, t: T):
                        self.t = t
                def f(y: float):
                    a = A(1.5)
                    b = A(y)
            "},
            PythonVersion::PY312,
        );
        assert!(out.contains("a = A[float](1.5)"), "got: {out}");
        assert!(out.contains("b = A[int | float](y)"), "got: {out}");
    }

    #[test]
    fn generic_builtin_the_runtime_never_subscripts_stays_bare() {
        // a `zip` object is generic to the type checker, but no release of
        // cpython gives `zip` a `__class_getitem__` — `zip[…](…)` raises
        // `TypeError`, and ordinary python is full of `zip` calls
        let out = out(indoc! {"
            pairs = zip([1, 2], [True, False])
            names = map(str, [1])
            kept = filter(None, [1])
            backwards = reversed([1])
        "});
        for expected in [
            "pairs = zip([1, 2], [True, False])",
            "names = map(str, [1])",
            "kept = filter(None, [1])",
            "backwards = reversed([1])",
        ] {
            assert!(
                out.contains(expected),
                "{expected:?} should stay bare: {out}"
            );
        }
    }

    #[test]
    fn generic_builtin_the_runtime_does_subscript_reifies() {
        // `enumerate` is the same shape as `zip` to the checker, but cpython
        // does give it a `__class_getitem__`, which typeshed writes down
        let out = out("labelled = enumerate([\"a\"])\n");
        assert!(
            out.contains("labelled = enumerate[str]([\"a\"])"),
            "a subscriptable builtin reifies: {out}"
        );
    }

    #[test]
    fn class_not_subscriptable_at_runtime_stays_bare() {
        // `array.array` is generic to the type checker on every version, but the
        // runtime only grew a `__class_getitem__` in 3.12 — below that,
        // `array.array[int](…)` raises `TypeError` when the emitted python runs
        let out = out_checked_at(
            "import array\nxs = array.array(\"i\", [1, 2])\n",
            PythonVersion::PY311,
        );
        assert!(
            out.contains("xs = array.array(\"i\", [1, 2])"),
            "an unsubscriptable class stays bare: {out}"
        );
    }

    #[test]
    fn class_subscriptable_at_runtime_reifies() {
        // the same class on the version that gave it a `__class_getitem__`
        let out = out_checked_at(
            "import array\nxs = array.array(\"i\", [1, 2])\n",
            PythonVersion::PY312,
        );
        assert!(
            out.contains("xs = array.array[int](\"i\", [1, 2])"),
            "a subscriptable class reifies: {out}"
        );
    }

    #[test]
    fn non_generic_constructor_untouched() {
        let out = out(indoc! {"
            class A:
                def __init__(self, t: int): ...
            a = A(1)
        "});
        assert!(out.contains("a = A(1)"), "non-generic stays bare: {out}");
    }

    #[test]
    fn unsolvable_constructor_stays_bare() {
        let out = out_at(
            indoc! {"
                class A[T]:
                    def __init__(self) -> None: ...
                a = A()
            "},
            PythonVersion::PY312,
        );
        assert!(out.contains("a = A()"), "unsolved ctor stays bare: {out}");
    }

    #[test]
    fn a_builtin_that_rejects_a_subscript_stays_bare() {
        // `zip`, `map` and `filter` are generic in the stub and unsubscriptable
        // at runtime — `zip[tuple[int, bool]]` is a `TypeError`, so the call has
        // to reach the output as it was written
        for (src, expected) in [
            ("xs = zip([1], [True])\n", "xs = zip([1], [True])"),
            ("xs = map(str, [1])\n", "map(str, [1])"),
            ("xs = filter(None, [1])\n", "filter(None, [1])"),
        ] {
            let out = out_at(src, PythonVersion::PY312);
            assert!(out.contains(expected), "{src:?} should stay bare: {out}");
        }
    }

    #[test]
    fn a_builtin_that_accepts_a_subscript_is_reified() {
        // `enumerate` declares its own `__class_getitem__`, and `deque`
        // inherits one; both accept the subscript at runtime
        let out = out_at("xs = enumerate([\"a\"])\n", PythonVersion::PY312);
        assert!(out.contains("enumerate[str]([\"a\"])"), "got: {out}");

        let out = out_at(
            indoc! {"
                from collections import deque
                xs = deque([1])
            "},
            PythonVersion::PY312,
        );
        assert!(out.contains("deque[int]([1])"), "got: {out}");
    }

    #[test]
    fn a_stub_generic_inheriting_a_generic_base_is_reified() {
        // `ChainMap` declares no `__class_getitem__`, but it inherits
        // `MutableMapping[Key, Value]`, which puts `Generic` in its runtime
        // bases — so the subscript evaluates
        let out = out_at(
            indoc! {"
                from collections import ChainMap
                m = ChainMap({\"a\": 1})
            "},
            PythonVersion::PY312,
        );
        assert!(out.contains("ChainMap[str, int]("), "got: {out}");
    }

    #[test]
    fn collection_literals_not_reified() {
        // a builtin collection erases its type arguments at runtime, so
        // wrapping a display in `list[int](…)` would be pure bloat — the
        // literals stay bare
        for (src, expected) in [
            ("xs = [1, 2]\n", "xs = [1, 2]"),
            ("xs = {1, 2}\n", "xs = {1, 2}"),
            ("xs = {\"a\": 1}\n", "xs = {\"a\": 1}"),
            ("t = 1, \"x\"\n", "t = 1, \"x\""),
            ("t = (1, \"x\")\n", "t = (1, \"x\")"),
            ("xs = [1, \"x\"]\n", "xs = [1, \"x\"]"),
            ("xs = [[1], [2]]\n", "xs = [[1], [2]]"),
        ] {
            let out = out(src);
            assert!(out.contains(expected), "{src:?} should stay bare: {out}");
            assert!(
                !out.contains("list[int]") && !out.contains("set[int]"),
                "no collection wrap: {out}"
            );
        }
    }

    #[test]
    fn function_defaults_left_for_mutable_defaults_lowering() {
        // a non-scalar default is swapped for `_MISSING` and re-evaluated in
        // a body guard — the default expression must not carry a wrap
        let out = out(indoc! {"
            def f(x=[1, 2]):
                pass
        "});
        assert!(
            out.contains("def f(x=_MISSING):"),
            "default must stay a bare marker: {out}"
        );
        assert!(
            !out.contains("](_MISSING"),
            "no reification wrapper may leak around the sentinel: {out}"
        );
    }

    #[test]
    fn lambda_default_not_reified() {
        let out = out("f = lambda xs=[1]: xs\n");
        assert!(
            out.contains("lambda xs=[1]: xs"),
            "lambda default stays bare: {out}"
        );
    }

    #[test]
    fn version_check_tuple_untouched() {
        let out = out(indoc! {"
            import sys
            if sys.version_info >= (3, 14):
                pass
            if sys.version_info[:2] == (3, 12):
                pass
        "});
        assert!(
            out.contains("sys.version_info >= (3, 14)"),
            "version gates must keep a literal tuple: {out}"
        );
        assert!(
            out.contains("sys.version_info[:2] == (3, 12)"),
            "sliced version gates too: {out}"
        );
    }

    #[test]
    fn empty_literals_untouched() {
        let out = out(indoc! {"
            a = []
            b = {}
            c = ()
        "});
        assert!(out.contains("a = []"), "empty list stays bare: {out}");
        assert!(out.contains("b = {}"), "empty dict stays bare: {out}");
        assert!(out.contains("c = ()"), "empty tuple stays bare: {out}");
    }

    #[test]
    fn annotation_and_value_both_bare() {
        let out = out("x: list[int] = [1]\n");
        assert!(
            out.contains("x: list[int] = [1]"),
            "annotation stays, value is not reified: {out}"
        );
    }

    #[test]
    fn type_context_subscript_slice_untouched() {
        // `str, int` in a value-position type subscript is a type expression,
        // not a tuple display
        let out = out("X = dict[str, int]\n");
        assert!(
            out.contains("X = dict[str, int]"),
            "type-context slice stays: {out}"
        );
    }

    #[test]
    fn value_subscript_key_untouched() {
        // a tuple key is a structural index, not a constructed value
        let out = out(indoc! {"
            d = {}
            d[(1, 2)]
        "});
        assert!(
            out.contains("d[(1, 2)]"),
            "keys pass through verbatim: {out}"
        );
    }

    #[test]
    fn dunder_all_untouched() {
        let out = out(indoc! {"
            __all__ = [\"a\"]
            a = 1
        "});
        assert!(
            out.contains("__all__ = [\"a\"]"),
            "__all__ must stay a literal display: {out}"
        );
    }

    #[test]
    fn shadowed_builtin_skips_wrap() {
        let out = out(indoc! {"
            def list(): ...
            xs = [1, 2]
        "});
        assert!(
            out.contains("xs = [1, 2]"),
            "shadowed builtin cannot be spelled: {out}"
        );
    }

    #[test]
    fn explicit_wrapper_not_double_wrapped() {
        let out = out("xs = list[int]([1, 2])\n");
        assert!(
            out.contains("xs = list[int]([1, 2])"),
            "explicit wrapper is already reified: {out}"
        );
    }

    #[test]
    fn unspellable_element_stays_bare() {
        // the element class is function-local, so its bare name has no
        // module-scope runtime spelling
        let out = out(indoc! {"
            def f():
                class C: ...
                return [C()]
        "});
        assert!(
            out.contains("return [C()]"),
            "scope-local element class stays bare: {out}"
        );
    }

    /// a class imported only under `if TYPE_CHECKING:` is a name the running program does
    /// not have, so a specialization naming it is left out, as an annotated assignment and as
    /// a write to a declared attribute
    #[test]
    fn a_class_imported_only_for_the_checker_is_left_out() {
        let out = out(indoc! {"
            from typing import TYPE_CHECKING
            if TYPE_CHECKING:
                from decimal import Decimal

            class Ledger:
                entries: set[Decimal]

                def __init__(self) -> None:
                    self.entries = set()
                    kept: set[Decimal] = set()

            totals: set[Decimal] = set()
        "});
        assert!(!out.contains("set[Decimal]()"), "got: {out}");
        assert_eq!(out.matches("= set()").count(), 3, "got: {out}");
    }

    /// a block on the constant under another name, or read off the module, is a type-checking
    /// block all the same, while a block on a condition that holds whenever the program runs
    /// is not
    #[test]
    fn a_class_imported_under_an_aliased_type_checking_block_is_left_out() {
        let out = out(indoc! {"
            import typing as t
            from typing import TYPE_CHECKING as TC

            CHECKING = TC
            ALWAYS = True

            if CHECKING:
                from decimal import Decimal
            if t.TYPE_CHECKING:
                from fractions import Fraction
            if ALWAYS:
                from ipaddress import IPv4Address

            class Ledger:
                entries: set[Decimal]
                fractions: set[Fraction]
                addresses: set[IPv4Address]

                def __init__(self) -> None:
                    self.entries = set()
                    self.fractions = set()
                    self.addresses = set()
        "});
        assert!(out.contains("self.entries = set()"), "got: {out}");
        assert!(out.contains("self.fractions = set()"), "got: {out}");
        assert!(
            out.contains("self.addresses = set[IPv4Address]()"),
            "got: {out}"
        );
    }

    /// a condition that reads the constant in any way — here a module's own `TYPE_CHECKING`,
    /// which the checker reads as `True` as it does `typing`'s, inside an `or` — may be one the
    /// program does not take
    #[test]
    fn a_class_past_a_condition_that_reads_type_checking_is_left_out() {
        let out = out(indoc! {"
            import os

            TYPE_CHECKING = False

            if TYPE_CHECKING or os.environ.get('NEVER'):
                from decimal import Decimal

            class Ledger:
                entries: set[Decimal]

                def __init__(self) -> None:
                    self.entries = set()
        "});
        assert!(out.contains("self.entries = set()"), "got: {out}");
    }

    /// what the running program binds above the statement a read is in is spelled, and what
    /// it binds only further down is not: a method's body runs when it is called, which can be
    /// before the module reaches a class declared below it (`Ledger()` above `class Later`)
    #[test]
    fn a_class_declared_above_is_spelled_in_a_method_and_one_below_is_not() {
        let out = out(indoc! {"
            from decimal import Decimal

            class Ledger:
                entries: set[Decimal]
                later: set[Later]

                def __init__(self) -> None:
                    self.entries = set()
                    self.later = set()
                    kept: set[Decimal] = set()

            class Later: ...
        "});
        assert!(out.contains("self.entries = set[Decimal]()"), "got: {out}");
        assert!(out.contains("self.later = set()"), "got: {out}");
        assert!(
            out.contains("kept: set[Decimal] = set[Decimal]()"),
            "got: {out}"
        );
    }

    /// the module's own statements run in order: one above a class's declaration runs before
    /// the class exists, one below it after
    #[test]
    fn a_class_is_spelled_below_its_declaration_and_not_above() {
        let out = out(indoc! {"
            early: set[Later] = set()

            class Later: ...

            late: set[Later] = set()
        "});
        assert!(out.contains("early: \"set[Later]\" = set()"), "got: {out}");
        assert!(
            out.contains("late: set[Later] = set[Later]()"),
            "got: {out}"
        );
    }

    /// the checker resolves a module-level name from every binding the module makes, so a
    /// statement that runs while an earlier binding of the name still holds — the module binds
    /// it again further down — may read another class than the one the checker resolved
    #[test]
    fn a_name_bound_again_further_down_is_left_out_above_it() {
        let out = out(indoc! {"
            from decimal import Decimal

            kept: set[Decimal] = set()

            from decimal import Decimal
        "});
        assert!(out.contains("kept: set[Decimal] = set()"), "got: {out}");
    }

    /// a class the module's global scope does not bind, as its own class, has no spelling at
    /// all: a type parameter, a class local to a function, a class nested in another
    #[test]
    fn a_class_the_module_does_not_bind_is_left_out() {
        let out = out(indoc! {"
            class Box[T]:
                items: set[T]

                def __init__(self):
                    self.items = set()

            def make():
                class Local: ...
                kept: set[Local] = set()
                return kept

            class Outer:
                class Inner: ...
                inners: set[Inner] = set()
        "});
        assert!(out.contains("self.items = set()"), "got: {out}");
        assert!(out.contains("kept: set[Local] = set()"), "got: {out}");
        assert!(out.contains("inners: set[Inner] = set()"), "got: {out}");
    }

    /// a `del` unbinds a name by the end of the module, and a binding made in one branch of a
    /// conditional expression leaves it unbound on the other
    #[test]
    fn a_name_the_module_may_have_unbound_is_left_out() {
        let out = out(indoc! {"
            import os
            from decimal import Decimal
            from fractions import Fraction as _F

            entries: set[Decimal] = set()
            del Decimal

            _ = (Fraction := _F) if os.environ.get(\"UNSET\") else None
            fractions: set[Fraction] = set()
        "});
        assert!(!out.contains("set[Decimal]()"), "got: {out}");
        assert!(!out.contains("set[Fraction]()"), "got: {out}");
        assert_eq!(out.matches("= set()").count(), 2, "got: {out}");
    }

    /// a function that rebinds a module-level name through `global` can run at any time, so
    /// the name may hold whatever it bound
    #[test]
    fn a_name_a_function_rebinds_through_global_is_left_out() {
        let out = out(indoc! {"
            from decimal import Decimal

            class Ledger:
                entries: set[Decimal]

                def __init__(self) -> None:
                    self.entries = set()

            def forget() -> None:
                global Decimal
                del Decimal
        "});
        assert!(out.contains("self.entries = set()"), "got: {out}");
    }

    /// a module the checker finds never ends, here one that stops with `sys.exit()`, has no
    /// end where every binding is made; a name only statements at its top bind, above the
    /// read, is bound wherever the read runs all the same
    #[test]
    fn a_class_bound_above_is_spelled_in_a_module_that_never_ends() {
        let out = out(indoc! {"
            import sys
            from decimal import Decimal

            class Ledger:
                entries: set[Decimal]

                def __init__(self) -> None:
                    self.entries = set()

            totals: set[Decimal] = set()
            sys.exit(0)
        "});
        assert!(out.contains("self.entries = set[Decimal]()"), "got: {out}");
        assert!(
            out.contains("totals: set[Decimal] = set[Decimal]()"),
            "got: {out}"
        );
    }

    #[test]
    fn inert_below_39() {
        let out = out_at("xs = [1, 2]\n", PythonVersion::PY38);
        assert!(out.contains("xs = [1, 2]"), "no pep 585 below 3.9: {out}");
    }
}
