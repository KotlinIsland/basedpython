//! Lowering for the `cast` and `cast?` infix operators.
//!
//! Both parse as an `ExprCall` whose `func` is a synthetic `Name("cast")` and
//! whose `arguments` are `[type, value]`; a flag distinguishes them:
//!
//! - **`is_cast`** — `<value> cast <type>`, the *checked* cast. By default
//!   (`Config::checked_cast`) it verifies the value at runtime and raises a
//!   `TypeError` on a mismatch, so `"1" cast int` errors. Its type is `type`.
//!   When checked casts are disabled it degrades to the unchecked
//!   `typing.cast(type, value)`.
//! - **`is_checked_cast`** — `<value> cast? <type>`, the *safe* cast. Always
//!   available: it yields the value when `isinstance(value, type)` holds and
//!   `None` otherwise, so its type is `type | None`.
//!
//! Each is rewritten to a helper call (or plain `cast` when unchecked). The
//! value is a [`Fragment::Src`] passthrough, so lowerings inside it still
//! compose and it is evaluated exactly once.
//!
//! How deeply a *checked* form validates is decided by what the target can
//! prove at runtime ([`TypeInfo::cast_check_plan`]):
//!
//! - a **user generic** whose instances carry `__orig_class__` (stamped by
//!   `A[int](…)`) validates its type arguments too, via `_parametric_is`:
//!   `x cast A[int]` rejects an `A[str]`.
//! - **anything else** collapses to the shallow `isinstance` target
//!   (`list[object]` → `list`, `int | str` → `(int, str)`), because builtins
//!   erase their arguments and a bare `isinstance(v, list[object])` is itself
//!   a runtime error. The dropped argument claim is reported by ty's
//!   `erased-cast-argument` lint.
//!
//! The unchecked `typing.cast` keeps the exact written type (it never reaches
//! `isinstance`).

use std::collections::BTreeSet;

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::parametric_is::{
    PARAMETRIC_IS_RUNTIME, PROTOCOL_IS_RUNTIME, protocol_members_literal, variance_tuple,
};
use crate::type_info::{CastCheck, SoundnessCheck, TypeInfo};

// `<value> cast <type>` with checks on: verify at runtime, raise on mismatch.
const CHECKED_CAST_HELPER: &str = "\
def _checked_cast(_v, _t):
    if not isinstance(_v, _t):
        raise TypeError(
            f\"cast to {getattr(_t, '__name__', _t)} failed: value is {type(_v).__name__}\"
        )
    return _v
";

// `<value> cast? <type>`: yield the value when it matches, else `None`.
const TRY_CAST_HELPER: &str = "\
def _try_cast(_v, _t):
    return _v if isinstance(_v, _t) else None
";

// deep forms, for a user generic whose instances carry `__orig_class__`
// (stamped by `A[int](…)`): validate the base class always, and the reified
// type arguments when the value carries them. a value with no reification
// passes the argument check — its arguments aren't available to check, leaving
// the base `isinstance` as the guarantee. reuses `_parametric_is` from
// `PARAMETRIC_IS_RUNTIME`
const CHECKED_CAST_P_HELPER: &str = "\
def _checked_cast_p(_v, _alias, _variances):
    _origin = getattr(_alias, \"__origin__\", _alias)
    if not isinstance(_v, _origin):
        raise TypeError(
            f\"cast to {_alias} failed: value is {type(_v).__name__}\"
        )
    if getattr(_v, \"__orig_class__\", None) is not None and not _parametric_is(_v, _alias, _variances):
        raise TypeError(
            f\"cast to {_alias} failed: value is {_v.__orig_class__}\"
        )
    return _v
";

const TRY_CAST_P_HELPER: &str = "\
def _try_cast_p(_v, _alias, _variances):
    _origin = getattr(_alias, \"__origin__\", _alias)
    if not isinstance(_v, _origin):
        return None
    if getattr(_v, \"__orig_class__\", None) is not None and not _parametric_is(_v, _alias, _variances):
        return None
    return _v
";

// structural forms, for a *protocol* target whose data members are checked
// against the value's reified class annotations (a protocol has no
// `__orig_class__` to probe). `_members` is the list `_by_protocol_is` takes;
// reuses `_by_protocol_is` from `PROTOCOL_IS_RUNTIME`
const CHECKED_CAST_PROTO_HELPER: &str = "\
def _checked_cast_proto(_v, _members):
    if not _by_protocol_is(_v, _members):
        raise TypeError(
            f\"cast failed: {type(_v).__name__} does not structurally match the protocol\"
        )
    return _v
";

const TRY_CAST_PROTO_HELPER: &str = "\
def _try_cast_proto(_v, _members):
    return _v if _by_protocol_is(_v, _members) else None
";

/// how deeply a checked cast validates: the shallow `isinstance` target, a deep
/// `__orig_class__` probe (user generic), or a structural annotation check
/// (protocol)
#[derive(Clone, Copy)]
enum Depth {
    Shallow,
    Parametric,
    Protocol,
}

/// the runtime helper a `cast` / `cast?` occurrence lowers to
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Helper {
    /// `_checked_cast(value, target)` — shallow, raises on mismatch
    Checked,
    /// `_try_cast(value, target)` — shallow, yields `None` on mismatch
    Try,
    /// `_checked_cast_p(value, alias, variances)` — deep, raises on mismatch
    CheckedParametric,
    /// `_try_cast_p(value, alias, variances)` — deep, yields `None`
    TryParametric,
    /// `_checked_cast_proto(value, members)` — structural, raises on mismatch
    CheckedProtocol,
    /// `_try_cast_proto(value, members)` — structural, yields `None`
    TryProtocol,
    /// `cast(type, value)` — unchecked `typing.cast`
    TypingCast,
}

impl Helper {
    /// whether this helper yields `None` on mismatch (the `cast?` family) rather
    /// than raising (the `cast` family)
    fn is_yielding(self) -> bool {
        matches!(self, Self::Try | Self::TryParametric | Self::TryProtocol)
    }

    /// the variant of this raising/yielding pair at the given depth. only ever
    /// called on the checked (`Checked` / `Try`) helpers — `TypingCast` never
    /// reaches a depth decision
    fn at_depth(self, depth: Depth) -> Self {
        match (depth, self.is_yielding()) {
            (Depth::Shallow, false) => Self::Checked,
            (Depth::Shallow, true) => Self::Try,
            (Depth::Parametric, false) => Self::CheckedParametric,
            (Depth::Parametric, true) => Self::TryParametric,
            (Depth::Protocol, false) => Self::CheckedProtocol,
            (Depth::Protocol, true) => Self::TryProtocol,
        }
    }

    fn open_call(self) -> &'static str {
        match self {
            Self::Checked => "_checked_cast(",
            Self::Try => "_try_cast(",
            Self::CheckedParametric => "_checked_cast_p(",
            Self::TryParametric => "_try_cast_p(",
            Self::CheckedProtocol => "_checked_cast_proto(",
            Self::TryProtocol => "_try_cast_proto(",
            Self::TypingCast => "cast(",
        }
    }

    /// the preamble this helper's call needs
    fn runtime(self) -> &'static str {
        match self {
            Self::Checked => CHECKED_CAST_HELPER,
            Self::Try => TRY_CAST_HELPER,
            Self::CheckedParametric => CHECKED_CAST_P_HELPER,
            Self::TryParametric => TRY_CAST_P_HELPER,
            Self::CheckedProtocol => CHECKED_CAST_PROTO_HELPER,
            Self::TryProtocol => TRY_CAST_PROTO_HELPER,
            Self::TypingCast => "from typing import cast",
        }
    }

    /// deep helpers call `_parametric_is`, so they pull in its runtime too
    fn is_parametric(self) -> bool {
        matches!(self, Self::CheckedParametric | Self::TryParametric)
    }

    /// structural helpers call `_by_protocol_is`, so they pull in its runtime too
    fn is_protocol(self) -> bool {
        matches!(self, Self::CheckedProtocol | Self::TryProtocol)
    }
}

struct CastLower<'a> {
    types: &'a dyn TypeInfo,
    /// `true` when the checked (`cast`) form does its runtime check; `false`
    /// degrades `cast` to unchecked `typing.cast`
    checked: bool,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    used: BTreeSet<Helper>,
}

impl<'a> CastLower<'a> {
    fn new(types: &'a dyn TypeInfo, checked: bool) -> Self {
        Self {
            types,
            checked,
            edits: Vec::new(),
            used: BTreeSet::new(),
        }
    }

    /// the target arguments for a *runtime-checked* cast, and the helper
    /// variant they belong to. a user generic whose instances carry
    /// `__orig_class__` goes deep (`A[int]` plus the variance codes
    /// `_parametric_is` needs); anything else collapses to the shallow
    /// `isinstance` target ty derives (`list[object]` → `list`,
    /// `int | str` → `(int, str)`), since a bare `isinstance(v, list[object])`
    /// is a runtime error. with no faithful test the written type passes
    /// through unchanged
    fn target(&self, type_arg: &Expr, helper: Helper) -> (Helper, Vec<Fragment>) {
        match self.types.cast_check_plan(type_arg) {
            Some(CastCheck::Kind(SoundnessCheck::Parametric { alias, variances })) => (
                helper.at_depth(Depth::Parametric),
                vec![Fragment::Lit(format!(
                    "{alias}, {}",
                    variance_tuple(&variances)
                ))],
            ),
            Some(CastCheck::Kind(SoundnessCheck::Isinstance(target))) => {
                (helper.at_depth(Depth::Shallow), vec![Fragment::Lit(target)])
            }
            Some(CastCheck::Protocol { members }) => (
                helper.at_depth(Depth::Protocol),
                vec![Fragment::Lit(protocol_members_literal(&members))],
            ),
            // `Unchecked` never reaches here — `visit_expr` degrades an
            // unverifiable-protocol target to `typing.cast` before emit — so a
            // target with no faithful check is a plain shallow passthrough
            Some(CastCheck::Unchecked) | None => (
                helper.at_depth(Depth::Shallow),
                vec![Fragment::Src(type_arg.range())],
            ),
        }
    }

    fn emit(&mut self, whole: TextRange, type_arg: &Expr, value_range: TextRange, helper: Helper) {
        // unchecked `typing.cast(type, value)` keeps the exact written type (it
        // never reaches `isinstance`) and takes the type first
        let (helper, first, rest) = if helper == Helper::TypingCast {
            (
                helper,
                Fragment::Src(type_arg.range()),
                vec![Fragment::Src(value_range)],
            )
        } else {
            let (helper, rest) = self.target(type_arg, helper);
            (helper, Fragment::Src(value_range), rest)
        };

        self.used.insert(helper);
        let mut fragments = vec![
            Fragment::Lit(helper.open_call().to_owned()),
            first,
            Fragment::Lit(", ".to_owned()),
        ];
        fragments.extend(rest);
        fragments.push(Fragment::Lit(")".to_owned()));
        self.edits.push((whole, fragments));
    }
}

impl<'ast> Visitor<'ast> for CastLower<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr
            && (call.is_cast || call.is_checked_cast)
            && let [type_arg, value_arg] = &*call.arguments.args
        {
            // a statically-proven upcast needs no runtime check — the probe
            // would always pass, and for a subscripted protocol or builtin
            // target it cannot even run — so it degrades to a plain
            // `typing.cast`, exactly as a disabled checked cast does. a
            // method-bearing protocol target has no faithful runtime check at
            // all, so it degrades the same way rather than emit an `isinstance`
            // against the protocol (a runtime error)
            let redundant = self.types.cast_is_redundant(value_arg, type_arg)
                || self.types.cast_target_is_unverifiable(type_arg);
            let helper = if call.is_checked_cast && !redundant {
                Helper::Try
            } else if self.checked && !redundant {
                Helper::Checked
            } else {
                Helper::TypingCast
            };
            self.emit(expr.range(), type_arg, value_arg.range(), helper);
        }
        walk_expr(self, expr);
    }
}

pub(crate) struct CheckedCastPass {
    checked: bool,
}

impl CheckedCastPass {
    pub(crate) fn new(checked: bool) -> Self {
        Self { checked }
    }
}

impl TypeAwarePass for CheckedCastPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = CastLower::new(types, self.checked);
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        if inner.edits.is_empty() {
            return;
        }
        // `_parametric_is` / `_by_protocol_is` must precede the helpers that
        // call them
        if inner.used.iter().any(|helper| helper.is_parametric()) {
            ctx.required_imports.push(PARAMETRIC_IS_RUNTIME.to_owned());
        }
        if inner.used.iter().any(|helper| helper.is_protocol()) {
            ctx.required_imports.push(PROTOCOL_IS_RUNTIME.to_owned());
        }
        for helper in &inner.used {
            ctx.required_imports.push(helper.runtime().to_owned());
        }
        ctx.template_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};

    /// default config — checked casts on
    fn check(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap()
    }

    fn unchecked_config() -> Config {
        Config {
            checked_cast: false,
            ..Config::test_default()
        }
    }

    #[test]
    fn cast_is_checked_by_default() {
        let out = check("def f(a: object):\n    b = a cast int\n");
        assert!(out.contains("b = _checked_cast(a, int)"), "got:\n{out}");
        assert!(
            out.contains("raise TypeError"),
            "strict helper injected:\n{out}"
        );
    }

    #[test]
    fn cast_disabled_is_typing_cast() {
        let out = transpile(
            "def f(a: object):\n    b = a cast int\n",
            &unchecked_config(),
        )
        .unwrap();
        assert!(out.contains("b = cast(int, a)"), "got:\n{out}");
        assert!(
            out.contains("from typing import cast"),
            "import injected:\n{out}"
        );
        assert!(!out.contains("_checked_cast"), "got:\n{out}");
    }

    #[test]
    fn try_cast_returns_none_and_is_ungated() {
        // `cast?` is always the safe form, independent of the checked-cast config
        for config in [Config::test_default(), unchecked_config()] {
            let out = transpile("def f(a: object):\n    b = a cast? int\n", &config).unwrap();
            assert!(out.contains("b = _try_cast(a, int)"), "got:\n{out}");
            assert!(out.contains("def _try_cast"), "got:\n{out}");
        }
    }

    #[test]
    fn checked_cast_to_union() {
        let out = check("def f(a: object):\n    b = a cast int | str\n");
        assert!(
            out.contains("b = _checked_cast(a, (int, str))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn try_cast_to_union() {
        let out = check("def f(a: object):\n    b = a cast? int | str\n");
        assert!(out.contains("b = _try_cast(a, (int, str))"), "got:\n{out}");
    }

    /// a user generic's instances carry `__orig_class__`, so the cast can
    /// validate the type arguments rather than just the base class
    #[test]
    fn user_generic_target_checks_arguments() {
        let out = check(
            "class A[T]:\n    t: T\n    def __init__(self, t: T): ...\n\ndef f(x: object):\n    b = x cast A[int]\n",
        );
        assert!(
            out.contains("b = _checked_cast_p(x, A[int], (0,))"),
            "got:\n{out}"
        );
        assert!(out.contains("def _checked_cast_p"), "got:\n{out}");
        // the deep helper calls `_parametric_is`, so its runtime comes along
        assert!(out.contains("def _parametric_is"), "got:\n{out}");
    }

    #[test]
    fn user_generic_try_target_checks_arguments() {
        let out = check(
            "class A[T]:\n    t: T\n    def __init__(self, t: T): ...\n\ndef f(x: object):\n    b = x cast? A[int]\n",
        );
        assert!(
            out.contains("b = _try_cast_p(x, A[int], (0,))"),
            "got:\n{out}"
        );
        assert!(out.contains("def _try_cast_p"), "got:\n{out}");
    }

    /// the variance codes come from the target's own type parameters, so an
    /// `out T` matches covariantly at runtime
    #[test]
    fn user_generic_target_carries_variance() {
        let out = check(
            "class A[out T]:\n    t: T\n    def __init__(self, t: T): ...\n\ndef f(x: object):\n    b = x cast A[int]\n",
        );
        assert!(
            out.contains("b = _checked_cast_p(x, A[int], (1,))"),
            "got:\n{out}"
        );
    }

    /// an unchecked cast never reaches a runtime probe, so a user generic
    /// target stays a plain `typing.cast`
    #[test]
    fn user_generic_unchecked_is_typing_cast() {
        let out = transpile(
            "class A[T]:\n    t: T\n    def __init__(self, t: T): ...\n\ndef f(x: object):\n    b = x cast A[int]\n",
            &unchecked_config(),
        )
        .unwrap();
        assert!(out.contains("b = cast(A[int], x)"), "got:\n{out}");
        assert!(!out.contains("_parametric_is"), "got:\n{out}");
    }

    #[test]
    fn parameterized_generic_collapses_to_origin() {
        // regression: `isinstance(v, list[object])` is a runtime error; the
        // checked cast must target the origin `list` instead
        let out = check("def f(a: object):\n    b = a cast list[object]\n");
        assert!(out.contains("b = _checked_cast(a, list)"), "got:\n{out}");
        assert!(
            !out.contains("list[object]"),
            "no parameterized target:\n{out}"
        );
    }

    #[test]
    fn union_of_generics_collapses_each_arm() {
        let out = check("def f(a: object):\n    b = a cast? list[int] | None\n");
        assert!(
            out.contains("b = _try_cast(a, (list, type(None)))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn unchecked_keeps_written_type() {
        // `typing.cast` never reaches `isinstance`, so the exact type is kept
        let out = transpile(
            "def f(a: object):\n    b = a cast list[object]\n",
            &unchecked_config(),
        )
        .unwrap();
        assert!(out.contains("b = cast(list[object], a)"), "got:\n{out}");
    }

    #[test]
    fn value_evaluated_once() {
        let out = check("def f():\n    b = g() cast int\n");
        assert_eq!(out.matches("g()").count(), 1, "single evaluation:\n{out}");
    }

    #[test]
    fn both_forms_in_one_file() {
        let out = check("def f(a: object, b: object):\n    x = a cast int\n    y = b cast? str\n");
        assert!(out.contains("x = _checked_cast(a, int)"), "got:\n{out}");
        assert!(out.contains("y = _try_cast(b, str)"), "got:\n{out}");
    }

    #[test]
    fn collection_literal_value_composes() {
        // a bare collection-literal value passes through the wrap intact
        let out = check("x = [1] cast list[object]\n");
        assert!(out.contains("_checked_cast([1], list)"), "got:\n{out}");
        assert_eq!(
            out.matches('(').count(),
            out.matches(')').count(),
            "balanced parens:\n{out}"
        );
    }

    /// a statically-proven upcast needs no runtime check: casting a value
    /// already known to be the target degrades to a plain `typing.cast`, which
    /// avoids a probe that (for a subscripted builtin) drops the argument claim
    /// and (for a subscripted protocol) would be a runtime error
    #[test]
    fn redundant_upcast_is_typing_cast() {
        let out = check("class B[T](list[T]): ...\n\ndef f():\n    b = B[int]() cast list[int]\n");
        assert!(out.contains("b = cast(list[int], B[int]())"), "got:\n{out}");
        assert!(!out.contains("_checked_cast"), "no runtime probe:\n{out}");
    }

    /// the same upcast through `cast?` also degrades — the value always matches,
    /// so no probe or `None` arm is needed at runtime
    #[test]
    fn redundant_try_upcast_is_typing_cast() {
        let out = check("class B[T](list[T]): ...\n\ndef f():\n    b = B[int]() cast? list[int]\n");
        assert!(out.contains("b = cast(list[int], B[int]())"), "got:\n{out}");
        assert!(!out.contains("_try_cast"), "no runtime probe:\n{out}");
    }

    /// subclassing a subscripted protocol is a redundant upcast too — a bare
    /// `isinstance(v, Sequence[object])` would raise, so it must not be emitted
    #[test]
    fn redundant_upcast_to_protocol_is_typing_cast() {
        let out = check(
            "from collections.abc import Sequence\n\nclass A[T](Sequence[T]):\n    def __getitem__(self, i): ...  # type: ignore\n    def __len__(self): ...\n\ndef f():\n    a = A[int]() cast Sequence[object]\n",
        );
        assert!(
            out.contains("a = cast(Sequence[object], A[int]())"),
            "got:\n{out}"
        );
        assert!(
            !out.contains("_checked_cast") && !out.contains("_parametric_is"),
            "no runtime probe:\n{out}"
        );
    }

    /// a genuine cast whose value is *not* already the target keeps its probe
    #[test]
    fn non_redundant_cast_keeps_probe() {
        let out = check("def f(a: object):\n    b = a cast list[int]\n");
        assert!(out.contains("b = _checked_cast(a, list)"), "got:\n{out}");
    }

    /// a data-member protocol target has no `__orig_class__` to probe, but its
    /// members are checked structurally against the value's reified annotations
    #[test]
    fn data_member_protocol_cast_checks_structurally() {
        let out = check(
            "from typing import Protocol\n\nclass A[T](Protocol):\n    a: T\n\ndef f(x: object):\n    b = x cast A[int]\n",
        );
        assert!(
            out.contains("b = _checked_cast_proto(x, [(\"a\", int, 0)])"),
            "got:\n{out}"
        );
        assert!(out.contains("def _checked_cast_proto"), "got:\n{out}");
        // the structural helper calls `_by_protocol_is`, so its runtime comes along
        assert!(out.contains("def _by_protocol_is"), "got:\n{out}");
    }

    #[test]
    fn data_member_protocol_try_cast_checks_structurally() {
        let out = check(
            "from typing import Protocol\n\nclass A[T](Protocol):\n    a: T\n\ndef f(x: object):\n    b = x cast? A[bool]\n",
        );
        assert!(
            out.contains("b = _try_cast_proto(x, [(\"a\", bool, 0)])"),
            "got:\n{out}"
        );
        assert!(out.contains("def _try_cast_proto"), "got:\n{out}");
    }

    /// a method-bearing protocol has no runtime residue — an `isinstance`
    /// against it would raise — so the checked cast degrades to `typing.cast`
    #[test]
    fn method_protocol_cast_degrades_to_typing_cast() {
        let out = check(
            "from typing import Protocol\n\nclass M[T](Protocol):\n    def get(self) -> T: ...\n\ndef f(x: object):\n    b = x cast M[int]\n",
        );
        assert!(out.contains("b = cast(M[int], x)"), "got:\n{out}");
        assert!(
            !out.contains("_checked_cast") && !out.contains("_by_protocol_is"),
            "no runtime probe against a method protocol:\n{out}"
        );
    }

    /// the same method-protocol degradation applies to `cast?`
    #[test]
    fn method_protocol_try_cast_degrades_to_typing_cast() {
        let out = check(
            "from typing import Protocol\n\nclass M[T](Protocol):\n    def get(self) -> T: ...\n\ndef f(x: object):\n    b = x cast? M[int]\n",
        );
        assert!(out.contains("b = cast(M[int], x)"), "got:\n{out}");
        assert!(!out.contains("_try_cast"), "no probe:\n{out}");
    }

    /// an unchecked config keeps a data-member protocol cast as a plain
    /// `typing.cast` — no structural probe is emitted
    #[test]
    fn data_member_protocol_unchecked_is_typing_cast() {
        let out = transpile(
            "from typing import Protocol\n\nclass A[T](Protocol):\n    a: T\n\ndef f(x: object):\n    b = x cast A[int]\n",
            &unchecked_config(),
        )
        .unwrap();
        assert!(out.contains("b = cast(A[int], x)"), "got:\n{out}");
        assert!(!out.contains("_by_protocol_is"), "got:\n{out}");
    }

    #[test]
    fn cast_identifier_unaffected() {
        // plain `cast(...)` and `cast = ...` stay untouched
        let out = check("cast = 5\nb = cast\n");
        assert!(!out.contains("_checked_cast"), "got:\n{out}");
    }
}
