//! parametric type tests (basedpython)
//!
//! `x is list[int]` (keyword form) tests a value against a *specialization*,
//! which plain `isinstance` cannot do — `isinstance(x, list[int])` is a
//! runtime `TypeError`, and the builtins erase their type arguments anyway.
//! the test is resolved rust-style, from static types at compile time, with
//! a runtime residue only where one is needed:
//!
//! - the value's type is fully known → the test folds to `True` / `False`
//!   (invariant, exact arguments — `list[object]` is never `list[int]`)
//! - the value's type mentions reified type parameters → the test unifies
//!   structurally and lowers to equality checks of the reified cells:
//!   `x: T` is `list[int]` → `T == list[int]`; `x: list[T]` is `list[int]`
//!   → `T == int`
//! - the value is undecidable statically (a mixed union, a dynamic value) →
//!   probe at runtime. the probe ([`PARAMETRIC_IS_RUNTIME`]) unwinds the value:
//!   its reified `__orig_class__` (stamped by `A[int](…)`) and every generic
//!   base declared across `type(value).__mro__` (`__orig_bases__`). a concrete
//!   subclass that fixes the arguments is checkable this way even against a
//!   builtin or abc target — `class B(list[int])` records `list[int]`, so
//!   `B() is list[int]` and `B() is Sequence[int]` both answer True; a bare
//!   `list` records nothing and answers False. the narrowing is positive-only,
//!   so a False never narrows unsoundly. no element-witness heuristic is used:
//!   the arguments come from the class's declared bases, never from peeking at a
//!   runtime element
//! - the value is undecidable and the target is a *protocol* → its members are
//!   checked structurally against the value's reified annotations
//!   ([`PROTOCOL_IS_RUNTIME`]): a data member against the value's class
//!   annotation, a method member against the value method's parameter and
//!   return annotations. only a protocol with a member whose specialized type
//!   has no runtime spelling stays an error (`erased-type-check`)
//!
//! a union rhs (`x is A[int] | object`) is the disjunction of its arms — each
//! arm lowered by its own kind — never a runtime `isinstance(x, A[int] |
//! object)`, which a parameterized arm makes a `TypeError`
//!
//! a subscripted rhs that is *not* a generic class (`x is candidates[0]`)
//! falls back to the ordinary `isinstance` lowering that
//! [`identity_swap`](super::identity_swap) applies to every other rhs
//!
//! # shared with the checked cast
//!
//! [`build_predicate`] is the core of this pass *and* of
//! [`checked_cast`](super::checked_cast): both ask one question — does this
//! value satisfy this type at runtime — over the same [`ParametricIsPlan`],
//! built from the same type expression. they differ in one parameter,
//! [`ProbeStrictness`]: an `is`-test must *earn* a `True` (it narrows) while a
//! cast is an assertion that only holds the value to arguments the runtime can
//! actually see

use std::collections::BTreeSet;
use std::fmt::Write as _;

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, CmpOp, Expr, PySourceType, Stmt};
use ruff_python_trivia::{SimpleTokenKind, SimpleTokenizer};
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::{ArgVariance, ParametricIsPlan, ProtocolMemberCheck, TargetSpelling};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

/// probes a value's `__orig_class__` against a target alias — the runtime
/// residue when a parametric test against a user-defined generic can't be
/// resolved statically. `variances` gives the target's per-parameter
/// variance (0 invariant, 1 covariant, 2 contravariant, 3 bivariant), so the
/// match respects `out T` / `in T`: `A[int]` is an `A[object]` when `T` is
/// covariant. `_sub` is a deliberately conservative one-level subtype check —
/// exact, the `object` top type, or an unparameterized supertype origin — so
/// it never reports a subtype that does not hold
/// render a variance-code list as the python tuple literal `_parametric_is`
/// takes as its `variances` argument (`(0,)`, `(0, 1)`)
pub(crate) fn variance_tuple(variances: &[u8]) -> String {
    match variances {
        [single] => format!("({single},)"),
        _ => format!(
            "({})",
            variances
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub(crate) const PARAMETRIC_IS_RUNTIME: &str = "\
def _by_type_param_defaults(args):
    # a class records its generic bases *unsubstituted* — `class L[T = Never]
    # (list[T])` stores `list[T]`, never `list[Never]` — so a type parameter
    # left at its pep 696 default resolves to that default rather than staying a
    # bare TypeVar that matches nothing
    resolved = []
    substituted = False
    for arg in args:
        has_default = getattr(arg, \"has_default\", None)
        if has_default is not None and has_default():
            resolved.append(arg.__default__)
            substituted = True
        else:
            resolved.append(arg)
    return tuple(resolved) if substituted else args

def _by_alias(value):
    # a reified generic class specializes to a *subclass*, which records the
    # alias it stands for; anything else already is what it says it is. read
    # from the class's own dict, so an ordinary subclass of a specialization is
    # not mistaken for one
    if isinstance(value, type):
        return value.__dict__.get(\"__orig_class__\", value)
    return value

def _by_subst(annotation, mapping):
    # replace type parameters with the arguments bound to them, rebuilding
    # nested aliases (`list[dict[str, T]]` with `T = int` → `list[dict[str, int]]`)
    annotation = _by_alias(annotation)
    try:
        if annotation in mapping:
            return mapping[annotation]
    except TypeError:
        pass
    args = getattr(annotation, \"__args__\", ())
    if not args:
        return annotation
    replaced = tuple(_by_subst(arg, mapping) for arg in args)
    if replaced == args:
        return annotation
    origin = getattr(annotation, \"__origin__\", None)
    if origin is None:
        return annotation
    try:
        return origin[replaced]
    except TypeError:
        return annotation

def _by_specialize(alias, origin, depth=0):
    # the arguments with which `alias` satisfies `origin`, resolved *down the
    # declared base chain* rather than assumed to line up positionally. a base
    # that fixes or reorders its arguments is then followed faithfully:
    # `class Odd[T](list[int])` is a `list[int]` whatever `T` is, and
    # `class Swap[A, B](dict[B, A])` specializes `dict` in the other order
    if depth > 16:
        return None
    alias = _by_alias(alias)
    klass = getattr(alias, \"__origin__\", alias)
    if not isinstance(klass, type):
        return None
    args = getattr(alias, \"__args__\", ())
    params = getattr(klass, \"__type_params__\", ())
    if not args:
        defaulted = _by_type_param_defaults(params)
        if defaulted is not params:
            args = defaulted
    if klass is origin:
        return args or None
    mapping = {}
    for param, arg in zip(params, args):
        try:
            mapping[param] = arg
        except TypeError:
            pass
    bases = klass.__dict__.get(\"__orig_bases__\")
    if bases is None:
        # a class inheriting only plain classes records no `__orig_bases__`
        bases = getattr(klass, \"__bases__\", ())
    for base in bases:
        found = _by_specialize(_by_subst(base, mapping) if mapping else base, origin, depth + 1)
        if found is not None:
            return found
    # the declared bases don't reach `origin`: a builtin registered as a *virtual*
    # subclass of an abc (`list` for `Sequence`) has no base to walk. its
    # arguments do line up positionally once membership is established. this runs
    # only after resolution has failed, so it applies to the already-resolved base
    # (`list[int]`), never to a subclass that fixes or reorders arguments
    if args and isinstance(origin, type):
        try:
            if issubclass(klass, origin):
                return args
        except TypeError:
            pass
    return None

def _by_generic_args(value, origin):
    # an explicit `A[int]()` records its specialization on the instance;
    # otherwise the class itself is the starting point and any pep 696 defaults
    # stand in for the arguments it was constructed with
    reified = getattr(value, \"__orig_class__\", None)
    found = _by_specialize(reified if reified is not None else type(value), origin)
    return [found] if found is not None else []

def _parametric_is(value, alias, variances):
    alias = _by_alias(getattr(alias, \"__value__\", alias))
    origin = getattr(alias, \"__origin__\", alias)
    if not isinstance(value, origin):
        return False
    target_args = getattr(alias, \"__args__\", ())
    if len(target_args) != len(variances):
        return False
    for reified_args in _by_generic_args(value, origin):
        if len(reified_args) != len(target_args):
            continue
        for r, t, v in zip(reified_args, target_args, variances):
            if v == 3 or r == t:
                continue
            if v == 1 and _parametric_is_sub(r, t):
                continue
            if v == 2 and _parametric_is_sub(t, r):
                continue
            break
        else:
            return True
    return False

def _parametric_is_sub(a, b):
    if a is b or b is object:
        return True
    a_origin = getattr(a, \"__origin__\", a)
    b_origin = getattr(b, \"__origin__\", b)
    if isinstance(a_origin, type) and isinstance(b_origin, type) and not getattr(b, \"__args__\", ()):
        try:
            return issubclass(a_origin, b_origin)
        except TypeError:
            return False
    return a == b

def _parametric_is_lenient(value, alias, variances):
    # the checked-cast form: a value that records no reification has no
    # arguments to check, so the base class test is the whole guarantee. this is
    # what keeps `[1, 2] cast list[int]` legal while still rejecting a value
    # whose recorded arguments contradict the target
    alias = _by_alias(getattr(alias, \"__value__\", alias))
    origin = getattr(alias, \"__origin__\", alias)
    if not isinstance(value, origin):
        return False
    if not _by_generic_args(value, origin):
        return True
    return _parametric_is(value, alias, variances)
";

/// runtime residue for a parametric test against a *protocol* target
/// (`value is A[int]`). a protocol's instances never record which
/// specialization they satisfy, so `__orig_class__` can't answer it — but
/// basedpython reifies annotations, so the value's class is checked
/// structurally: each protocol member's reified annotation must match the
/// member's specialized type. `members` is a list of kind-tagged tuples:
///
/// - `("attr", name, expected_type, variance)` — a data member, checked against
///   the value class's annotation for `name`
/// - `("method", name, [(type, variance), …], return_or_None)` — a method
///   member, whose parameters (contravariant) and return (covariant) are checked
///   against the value method's reified parameter/return annotations; a
///   parameter with no annotation but a default falls back to `type(default)`
///
/// `variance` matches [`ArgVariance`]'s codes (0 invariant → equality, 1
/// covariant → subtype, 2 contravariant → supertype, 3 bivariant → any).
/// annotations are read with `typing.get_type_hints` (resolving string
/// annotations and inherited members), falling back to a raw `__mro__` walk
const PROTOCOL_IS_RUNTIME: &str = "\
_by_proto_missing = object()

def _by_member_annotation(klass, name):
    try:
        import typing
        hints = typing.get_type_hints(klass)
    except Exception:
        hints = None
    if hints is not None and name in hints:
        return hints[name]
    for base in klass.__mro__:
        annotations = base.__dict__.get(\"__annotations__\", {})
        if name in annotations:
            return annotations[name]
    return _by_proto_missing

def _by_lit(*values):
    # rebuild `typing.Literal[…]` for a literal type argument (`A[True]`
    # specializes `T` to `Literal[True]`). spelled as a call so the member list
    # needs no import of its own — this helper ships with the check
    import typing
    return typing.Literal[values]

def _by_literal_args(t):
    import typing
    return typing.get_args(t) if typing.get_origin(t) is typing.Literal else None

def _by_proto_sub(a, b):
    if a is b or b is object:
        return True
    a_values = _by_literal_args(a)
    b_values = _by_literal_args(b)
    if a_values is not None:
        # `Literal[True]` is a subtype of another literal that lists all its
        # values, and of any class every value is an instance of
        if b_values is not None:
            return all(value in b_values for value in a_values)
        return isinstance(b, type) and all(isinstance(value, b) for value in a_values)
    if b_values is not None:
        # a whole class is never a subtype of a narrower literal
        return False
    a_origin = getattr(a, \"__origin__\", a)
    b_origin = getattr(b, \"__origin__\", b)
    if isinstance(a_origin, type) and isinstance(b_origin, type) and not getattr(b, \"__args__\", ()):
        try:
            return issubclass(a_origin, b_origin)
        except TypeError:
            return False
    return a == b

def _by_variance_ok(actual, expected, variance):
    # 0 invariant (equality), 1 covariant (actual <: expected),
    # 2 contravariant (expected <: actual), 3 bivariant (any)
    if variance == 3 or actual == expected:
        return True
    if variance == 1 and _by_proto_sub(actual, expected):
        return True
    if variance == 2 and _by_proto_sub(expected, actual):
        return True
    return False

def _by_method_matches(klass, name, params, ret):
    method = getattr(klass, name, None)
    if not callable(method):
        return False
    import inspect, typing
    try:
        signature = inspect.signature(method)
        hints = typing.get_type_hints(method)
    except Exception:
        return False
    positional = [
        p for p in signature.parameters.values()
        if p.kind in (inspect.Parameter.POSITIONAL_ONLY, inspect.Parameter.POSITIONAL_OR_KEYWORD)
    ]
    # drop the receiver (`self` / `cls`) an unbound method still carries
    positional = positional[1:]
    if len(positional) < len(params):
        return False
    # extra positional parameters the protocol doesn't supply must be optional,
    # else a caller matching the protocol would fail to provide them
    for p in positional[len(params):]:
        if p.default is inspect.Parameter.empty:
            return False
    # likewise any required keyword-only parameter would break a protocol call
    for p in signature.parameters.values():
        if p.kind == inspect.Parameter.KEYWORD_ONLY and p.default is inspect.Parameter.empty:
            return False
    for (expected, variance), p in zip(params, positional):
        if p.name in hints:
            actual = hints[p.name]
        elif p.default is not inspect.Parameter.empty:
            # a reified default gives the parameter's inferred type at runtime
            actual = type(p.default)
        else:
            return False
        if not _by_variance_ok(actual, expected, variance):
            return False
    if ret is not None:
        expected, variance = ret
        if \"return\" not in hints or not _by_variance_ok(hints[\"return\"], expected, variance):
            return False
    return True

def _by_protocol_is(value, members):
    klass = type(value)
    for member in members:
        kind = member[0]
        if kind == \"attr\":
            _, name, expected, variance = member
            actual = _by_member_annotation(klass, name)
            if actual is _by_proto_missing:
                # the member is *there*, it just carries no annotation any
                # runtime can read — python records nothing for a `self.a: int`
                # written inside `__init__`. answering `False` would contradict
                # the checker, which accepts that class as satisfying the
                # protocol, so refuse to answer rather than answer wrongly
                if hasattr(value, name):
                    raise TypeError(
                        \"cannot check `\" + klass.__qualname__ + \".\" + name
                        + \"` against a parameterized protocol: its type is declared \"
                        + \"inside a method, and only a class-level annotation \"
                        + \"survives to runtime. declare it in the class body\"
                    )
                return False
            if not _by_variance_ok(actual, expected, variance):
                return False
        else:
            _, name, params, ret = member
            if not _by_method_matches(klass, name, params, ret):
                return False
    return True
";

/// matches a value against a template literal type — a pattern such as
/// `f"a{int}b"`, whose type is the set of strings it can produce.
///
/// the regular expression comes from the checker, which builds it from the same
/// reading of the pattern's holes that decides the static answer, so the test
/// accepts exactly the strings the type contains. a non-`str` value is not one
/// of them
const PATTERN_IS_RUNTIME: &str = "\
import re as _by_re

def _by_pattern_is(value, pattern):
    return isinstance(value, str) and _by_re.fullmatch(pattern, value) is not None
";

/// the runtime variance code `_by_variance_ok` expects
fn variance_code(variance: ArgVariance) -> u8 {
    match variance {
        ArgVariance::Invariant => 0,
        ArgVariance::Covariant => 1,
        ArgVariance::Contravariant => 2,
        ArgVariance::Bivariant => 3,
    }
}

/// render a protocol member list as the python list literal `_by_protocol_is`
/// takes as its `members` argument. shared with the checked cast, which
/// validates the same structural claim. each entry is a kind-tagged tuple:
/// `(\"attr\", name, type, variance)` or `(\"method\", name, [(type, variance),
/// …], return_or_None)`
fn protocol_members_literal(checks: &[ProtocolMemberCheck]) -> String {
    let type_variance = |(expected, variance): &(String, ArgVariance)| {
        format!("({expected}, {})", variance_code(*variance))
    };
    let entries = checks
        .iter()
        .map(|check| match check {
            ProtocolMemberCheck::Attribute {
                name,
                expected,
                variance,
            } => format!(
                "(\"attr\", {name:?}, {expected}, {})",
                variance_code(*variance)
            ),
            ProtocolMemberCheck::Method { name, params, ret } => {
                let params = params
                    .iter()
                    .map(type_variance)
                    .collect::<Vec<_>>()
                    .join(", ");
                let ret = ret.as_ref().map_or("None".to_owned(), type_variance);
                format!("(\"method\", {name:?}, [{params}], {ret})")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{entries}]")
}

/// how a runtime probe treats a value that carries no reification
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeStrictness {
    /// no reification → no match. an `is`-test must *earn* its positive answer,
    /// since a `True` narrows
    Strict,
    /// no reification → the base class test is the whole guarantee. a checked
    /// cast is an assertion, so arguments it cannot see are not held against the
    /// value (`[1, 2] cast list[int]` stays legal)
    Lenient,
}

/// what a built predicate needs from the preamble, and what it proved
#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent yes/no facts about one built predicate; grouping them \
              would only rename the same flags"
)]
pub(crate) struct PredicateNeeds {
    /// the predicate calls `_parametric_is` / `_parametric_is_lenient`
    pub(crate) parametric_runtime: bool,
    /// the predicate calls `_by_protocol_is`
    pub(crate) protocol_runtime: bool,
    /// the predicate calls `_by_conforms`
    pub(crate) conformance_runtime: bool,
    /// the predicate calls `_by_pattern_is`
    pub(crate) pattern_runtime: bool,
    /// no arm carried a parametric claim — a plain `isinstance` covers the whole
    /// target, so a caller may use its compact shallow form instead
    pub(crate) all_plain: bool,
    /// every arm folded to `True`; the check is redundant
    pub(crate) all_true: bool,
    /// some arm has no faithful runtime check
    pub(crate) erased: bool,
    /// the predicate mentions the value; when it doesn't (a static fold, a
    /// reified-cell comparison) the caller must keep an effectful value alive
    /// itself
    pub(crate) references_value: bool,
}

impl PredicateNeeds {
    fn new() -> Self {
        Self {
            all_plain: true,
            all_true: true,
            ..Self::default()
        }
    }
}

/// the bare (non-negated) runtime predicate for one target arm, referencing the
/// value through `value_ref` so the caller controls how it is bound.
///
/// this is the shared core of `x is T` and `x cast T`: both ask the same
/// question — does this value satisfy this type at runtime — and differ only in
/// `probe`, which decides what an unreified value means.
fn arm_predicate(
    types: &dyn TypeInfo,
    value_ref: &dyn Fn() -> Fragment,
    value_expr: &Expr,
    arm: &Expr,
    probe: ProbeStrictness,
    needs: &mut PredicateNeeds,
) -> Vec<Fragment> {
    let Some(plan) = types.parametric_is_plan(value_expr, arm) else {
        needs.all_true = false;
        needs.references_value = true;
        return vec![
            Fragment::Lit("isinstance(".to_owned()),
            value_ref(),
            Fragment::Lit(", ".to_owned()),
            Fragment::Src(arm.range()),
            Fragment::Lit(")".to_owned()),
        ];
    };
    fragments_for_plan(types, plan, value_ref, arm, probe, needs)
}

/// the fragments one already-classified plan lowers to. split out from
/// [`arm_predicate`] so a union plan can build its arms, which the source need
/// not spell separately — a PEP 695 alias names a whole union with one word
fn fragments_for_plan(
    types: &dyn TypeInfo,
    plan: ParametricIsPlan,
    value_ref: &dyn Fn() -> Fragment,
    arm: &Expr,
    probe: ProbeStrictness,
    needs: &mut PredicateNeeds,
) -> Vec<Fragment> {
    // a union reports through its arms instead: a disjunction of plain
    // `isinstance` calls is still a plain check, and it holds as soon as one arm
    // does
    if !matches!(plan, ParametricIsPlan::Union(_)) {
        if !matches!(
            plan,
            ParametricIsPlan::Isinstance(_) | ParametricIsPlan::Unresolved
        ) {
            needs.all_plain = false;
        }
        if !matches!(plan, ParametricIsPlan::Fold(true)) {
            needs.all_true = false;
        }
    }
    match plan {
        // an erased arm can't be checked at runtime; ty reports the error (a
        // union arm may not silently fold to `False` — that would be unsound —
        // so the checker rejects it), and the lowering is the constant it is
        ParametricIsPlan::ErasedTarget(_) => {
            needs.erased = true;
            vec![Fragment::Lit("False".to_owned())]
        }
        ParametricIsPlan::Fold(false) => vec![Fragment::Lit("False".to_owned())],
        ParametricIsPlan::Fold(true) => vec![Fragment::Lit("True".to_owned())],
        ParametricIsPlan::TokenEq(tokens) => {
            let mut frags = vec![Fragment::Lit("(".to_owned())];
            for (index, (name, target)) in tokens.iter().enumerate() {
                let lead = if index == 0 { "" } else { " and " };
                frags.push(Fragment::Lit(format!("{lead}{name} == ")));
                frags.push(Fragment::Src(*target));
            }
            frags.push(Fragment::Lit(")".to_owned()));
            frags
        }
        ParametricIsPlan::Probe { target, variances } => {
            needs.parametric_runtime = true;
            needs.references_value = true;
            let codes: Vec<u8> = variances.iter().copied().map(variance_code).collect();
            let call = match probe {
                ProbeStrictness::Strict => "_parametric_is(",
                ProbeStrictness::Lenient => "_parametric_is_lenient(",
            };
            let mut frags = vec![
                Fragment::Lit(call.to_owned()),
                value_ref(),
                Fragment::Lit(", ".to_owned()),
                target_fragment(&target, arm),
            ];
            frags.push(Fragment::Lit(format!(", {})", variance_tuple(&codes))));
            frags
        }
        ParametricIsPlan::ProtocolStructural(checks) => {
            needs.protocol_runtime = true;
            needs.references_value = true;
            let members = protocol_members_literal(&checks);
            vec![
                Fragment::Lit("_by_protocol_is(".to_owned()),
                value_ref(),
                Fragment::Lit(format!(", {members})")),
            ]
        }
        // nothing is known about the target, so the source's own spelling is
        // the only thing to test against — and the error that left it unknown
        // is already reported
        // an interface something visibly conforms to cannot be answered by
        // `isinstance`: a conforming type is not a subclass. the registry
        // answers first, and a value nothing registered falls back to carrying
        // the requirements
        ParametricIsPlan::Conformance { target, members } => {
            needs.references_value = true;
            needs.conformance_runtime = true;
            let mut spelled = String::from(", (");
            for name in &members {
                let _ = write!(spelled, "\"{name}\", ");
            }
            spelled.push(')');
            vec![
                Fragment::Lit("_by_conforms(".to_owned()),
                value_ref(),
                Fragment::Lit(", ".to_owned()),
                target_fragment(&target, arm),
                Fragment::Lit(format!("{spelled})")),
            ]
        }
        // the checker could not read the target as a type. that is how a unit
        // enum variant arrives: the enum lowering has already rewritten it into
        // a singleton instance, so what the source named as a type names a
        // value here — and identity is the test for a value. anything else
        // keeps the plain instance check, and whatever left the target unknown
        // is already reported where it is written
        ParametricIsPlan::Unresolved => {
            needs.references_value = true;
            if types.is_plain_value(arm) {
                return vec![
                    value_ref(),
                    Fragment::Lit(" is ".to_owned()),
                    Fragment::Src(arm.range()),
                ];
            }
            vec![
                Fragment::Lit("isinstance(".to_owned()),
                value_ref(),
                Fragment::Lit(", ".to_owned()),
                Fragment::Src(arm.range()),
                Fragment::Lit(")".to_owned()),
            ]
        }
        ParametricIsPlan::Isinstance(target) => {
            needs.references_value = true;
            let mut frags = vec![
                Fragment::Lit("isinstance(".to_owned()),
                value_ref(),
                Fragment::Lit(", ".to_owned()),
            ];
            frags.push(target_fragment(&target, arm));
            frags.push(Fragment::Lit(")".to_owned()));
            frags
        }
        // `None` is a value, not a class, so `isinstance` cannot take it — and
        // there is only one `None`, which makes identity the whole test
        // a bare `Callable` asks exactly what `callable()` answers
        ParametricIsPlan::IsCallable => {
            needs.references_value = true;
            vec![
                Fragment::Lit("callable(".to_owned()),
                value_ref(),
                Fragment::Lit(")".to_owned()),
            ]
        }
        ParametricIsPlan::IsNone => {
            needs.references_value = true;
            vec![value_ref(), Fragment::Lit(" is None".to_owned())]
        }
        // the class guard is not redundant: python's `1 == True` would let a
        // `bool` satisfy `Literal[1]` without it
        ParametricIsPlan::Equality { class, value } => {
            needs.references_value = true;
            vec![
                Fragment::Lit("(type(".to_owned()),
                value_ref(),
                Fragment::Lit(format!(") is {class} and ")),
                value_ref(),
                Fragment::Lit(" == ".to_owned()),
                target_fragment(&value, arm),
                Fragment::Lit(")".to_owned()),
            ]
        }
        ParametricIsPlan::Identity(target) => {
            needs.references_value = true;
            vec![
                value_ref(),
                Fragment::Lit(" is ".to_owned()),
                target_fragment(&target, arm),
            ]
        }
        ParametricIsPlan::Subclass(target) => {
            needs.references_value = true;
            let mut frags = vec![Fragment::Lit("(isinstance(".to_owned()), value_ref()];
            frags.push(Fragment::Lit(", type) and issubclass(".to_owned()));
            frags.push(value_ref());
            frags.push(Fragment::Lit(", ".to_owned()));
            frags.push(target_fragment(&target, arm));
            frags.push(Fragment::Lit("))".to_owned()));
            frags
        }
        // a template literal type is a set of strings; the regular expression
        // ty built spells exactly the strings it produces
        ParametricIsPlan::Pattern(pattern) => {
            needs.pattern_runtime = true;
            needs.references_value = true;
            vec![
                Fragment::Lit("_by_pattern_is(".to_owned()),
                value_ref(),
                Fragment::Lit(format!(", {})", python_string_literal(&pattern))),
            ]
        }
        // a value satisfies a union as soon as one arm holds
        ParametricIsPlan::Union(arms) => {
            let mut frags = vec![Fragment::Lit("(".to_owned())];
            let mut any_true = false;
            for (index, arm_plan) in arms.iter().enumerate() {
                if index > 0 {
                    frags.push(Fragment::Lit(" or ".to_owned()));
                }
                any_true |= matches!(arm_plan, ParametricIsPlan::Fold(true));
                frags.extend(fragments_for_plan(
                    types,
                    arm_plan.clone(),
                    value_ref,
                    arm,
                    probe,
                    needs,
                ));
            }
            frags.push(Fragment::Lit(")".to_owned()));
            if any_true {
                needs.all_true = true;
            }
            frags
        }
    }
}

/// how a plan's target is written: its own spelling, or the source the target
/// was written as when the plan carries none
fn target_fragment(target: &TargetSpelling, arm: &Expr) -> Fragment {
    match target {
        TargetSpelling::Written => Fragment::Src(arm.range()),
        TargetSpelling::Rebuilt(text) => Fragment::Lit(text.clone()),
    }
}

/// a python string literal for `text`, escaped so the emitted source reads it
/// back byte for byte. only the characters that end or reinterpret a
/// single-quoted literal need escaping
fn python_string_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// the full runtime predicate for a target expression. shared by `is` and
/// `cast`.
///
/// A union target needs no splitting here: the target is a type expression, so
/// its type is the union and the plan carries one arm per member — including
/// the arms of a union the source never spelled as one.
pub(crate) fn build_predicate(
    types: &dyn TypeInfo,
    value_ref: &dyn Fn() -> Fragment,
    value_expr: &Expr,
    target: &Expr,
    probe: ProbeStrictness,
) -> (Vec<Fragment>, PredicateNeeds) {
    let mut needs = PredicateNeeds::new();
    let frags = arm_predicate(types, value_ref, value_expr, target, probe, &mut needs);
    (frags, needs)
}

/// whether replacing an expression may drop it without losing effects
fn effect_free(expr: &Expr) -> bool {
    matches!(expr, Expr::Name(_)) || expr.is_literal_expr()
}

struct ParametricIs<'src, 'ti> {
    source: &'src str,
    types: &'ti dyn TypeInfo,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    /// the runtime helpers the predicates emitted so far call
    runtimes: BTreeSet<PredicateRuntime>,
}

impl ParametricIs<'_, '_> {
    /// the folded / token result expression, with the lhs kept alive when it
    /// may have effects (`(g(), True)[1]` evaluates then discards it)
    fn with_lhs_effects(lhs: &Expr, result: Vec<Fragment>) -> Vec<Fragment> {
        if effect_free(lhs) {
            return result;
        }
        let mut frags = vec![Fragment::Lit("(".to_owned()), Fragment::Src(lhs.range())];
        frags.push(Fragment::Lit(", ".to_owned()));
        frags.extend(result);
        frags.push(Fragment::Lit(")[1]".to_owned()));
        frags
    }

    /// `lhs is rhs` for a single (non-union) target, built by the shared
    /// predicate builder and then wrapped for this form: negation, and keeping
    /// an effectful lhs alive when the predicate doesn't mention it
    fn lower_pair(&mut self, lhs: &Expr, rhs: &Expr, negate: bool) -> Vec<Fragment> {
        // a predicate may mention the value more than once — a union tests each
        // arm, an equality also checks the class — so an effectful left operand
        // is bound to a lambda parameter and evaluated once. counting the
        // references is what tells the two cases apart: a single mention needs
        // no binding, and the lambda would only obscure the output
        let mentions = std::cell::Cell::new(0usize);
        let counting = || {
            mentions.set(mentions.get() + 1);
            Fragment::Src(lhs.range())
        };
        let (frags, needs) =
            build_predicate(self.types, &counting, lhs, rhs, ProbeStrictness::Strict);
        let via_lambda = mentions.get() > 1 && !effect_free(lhs);
        let (frags, needs) = if via_lambda {
            let param = || Fragment::Lit(UNION_VALUE_PARAM.to_owned());
            build_predicate(self.types, &param, lhs, rhs, ProbeStrictness::Strict)
        } else {
            (frags, needs)
        };
        self.runtimes.extend(PredicateRuntime::used(&needs));

        // a predicate that folded to a constant inverts in place rather than
        // growing a `not`
        if let [Fragment::Lit(literal)] = frags.as_slice()
            && let Some(folded) = match literal.as_str() {
                "True" => Some(true),
                "False" => Some(false),
                _ => None,
            }
        {
            let value = if folded != negate { "True" } else { "False" };
            return Self::with_lhs_effects(lhs, vec![Fragment::Lit(value.to_owned())]);
        }

        // `x is not None` rather than `not x is None`: the same test, and the
        // one a reader (and every linter) expects. an identity predicate is the
        // only shape python can negate in place, and it is spelled with the
        // operator in a literal fragment of its own
        if negate
            && let [value, Fragment::Lit(operator), rest @ ..] = frags.as_slice()
            && operator.starts_with(" is ")
        {
            let mut negated = vec![
                value.clone(),
                Fragment::Lit(operator.replacen(" is ", " is not ", 1)),
            ];
            negated.extend_from_slice(rest);
            return negated;
        }

        let mut result = Vec::new();
        if negate {
            result.push(Fragment::Lit("not ".to_owned()));
        }
        if via_lambda {
            result.push(Fragment::Lit(format!("(lambda {UNION_VALUE_PARAM}: ")));
            result.extend(frags);
            result.push(Fragment::Lit(")(".to_owned()));
            result.push(Fragment::Src(lhs.range()));
            result.push(Fragment::Lit(")".to_owned()));
            return result;
        }
        result.extend(frags);
        if needs.references_value {
            result
        } else {
            Self::with_lhs_effects(lhs, result)
        }
    }

    fn process_compare(&mut self, compare: &ast::ExprCompare) {
        let mut lhs: &Expr = &compare.left;
        for (index, rhs) in compare.comparators.iter().enumerate() {
            // every type test is lowered here, whatever shape its target was
            // written in. the plan comes from the target's *type*, so a target
            // the source spells as one word (`x is Alias`) and the union that
            // word stands for reach the same lowering
            if compare.is_type_test(index, PySourceType::BasedPython) {
                let negate = compare.ops.get(index) == Some(&CmpOp::IsNot);
                let (opening, closing) = paren_padding(self.source, lhs, rhs);
                let mut replacement = Vec::new();
                replacement.push(Fragment::Lit("(".repeat(opening)));
                replacement.extend(self.lower_pair(lhs, rhs, negate));
                replacement.push(Fragment::Lit(")".repeat(closing)));
                let pair_range = TextRange::new(lhs.range().start(), rhs.range().end());
                self.edits.push((pair_range, replacement));
            }
            lhs = rhs;
        }
    }
}

/// a runtime helper one of the emitted predicates calls. the preamble emits one
/// definition per helper a pass actually used, in this order — `_parametric_is`
/// and `_by_protocol_is` must precede the predicates that call them
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PredicateRuntime {
    Parametric,
    Protocol,
    Conformance,
    Pattern,
}

impl PredicateRuntime {
    /// the definitions this helper needs in the emitted module
    pub(crate) fn source(self) -> &'static str {
        match self {
            Self::Parametric => PARAMETRIC_IS_RUNTIME,
            Self::Protocol => PROTOCOL_IS_RUNTIME,
            Self::Conformance => super::conformance::WITNESS_RUNTIME,
            Self::Pattern => PATTERN_IS_RUNTIME,
        }
    }

    /// the helpers `needs` recorded, in preamble order
    pub(crate) fn used(needs: &PredicateNeeds) -> impl Iterator<Item = Self> {
        [
            needs.parametric_runtime.then_some(Self::Parametric),
            needs.protocol_runtime.then_some(Self::Protocol),
            needs.conformance_runtime.then_some(Self::Conformance),
            needs.pattern_runtime.then_some(Self::Pattern),
        ]
        .into_iter()
        .flatten()
    }
}

/// the lambda parameter that binds an effectful test value for a predicate that
/// mentions it more than once. unlikely to collide: an `is`-target is a type
/// expression, and this name would have to appear free inside one
const UNION_VALUE_PARAM: &str = "_by_is_value";

/// how many parentheses a replacement for one comparison pair must supply
/// itself, because the source wrote them around an operand rather than around
/// the pair.
///
/// An operand's range stops inside its own parentheses, so the pair
/// `lhs.start() .. rhs.end()` of `(a) is str` swallows the `)` while leaving the
/// `(` outside it. Rather than guess which outer `(` that was — the `(` of an
/// enclosing call looks exactly the same — the replacement closes what it
/// swallowed and opens what the source will close after it.
fn paren_padding(source: &str, lhs: &Expr, rhs: &Expr) -> (usize, usize) {
    let gap = TextRange::new(lhs.range().end(), rhs.range().start());
    let tokens: Vec<_> = SimpleTokenizer::new(source, gap).skip_trivia().collect();
    // parens closing around the left operand come first in the gap, parens
    // opening around the right operand last; the operator sits between them
    let closing = tokens
        .iter()
        .take_while(|token| token.kind() == SimpleTokenKind::RParen)
        .count();
    let opening = tokens
        .iter()
        .rev()
        .take_while(|token| token.kind() == SimpleTokenKind::LParen)
        .count();
    (opening, closing)
}

impl<'ast> Visitor<'ast> for ParametricIs<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Compare(compare) = expr {
            self.process_compare(compare);
        }
        walk_expr(self, expr);
    }
}

pub(crate) struct ParametricIsPass<'src> {
    source: &'src str,
}

impl<'src> ParametricIsPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for ParametricIsPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = ParametricIs {
            source: self.source,
            types,
            edits: Vec::new(),
            runtimes: BTreeSet::new(),
        };
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        // no version gate is needed here: the only lowering that spells a
        // *builtin* subscript at runtime is the reified-cell token equality
        // (`T == list[int]`), which is already restricted to 3.12+ by the
        // reified-generic requirement; a user-generic probe (`A[int]`) works
        // on any target
        for runtime in inner.runtimes {
            ctx.required_imports.push(runtime.source().to_owned());
        }
        ctx.template_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    fn out(input: &str) -> String {
        transpile(
            input,
            &Config {
                min_version: PythonVersion::PY313,
                ..Config::test_default()
            },
        )
        .unwrap()
    }

    #[test]
    fn concrete_mismatch_folds_false() {
        let out = out(indoc! {"
            xs: list[object] = [1]
            print(xs is list[int])
        "});
        assert!(out.contains("print(False)"), "should fold false: {out}");
    }

    #[test]
    fn concrete_match_folds_true() {
        let out = out(indoc! {"
            xs = [1, 2]
            print(xs is list[int])
        "});
        assert!(out.contains("print(True)"), "should fold true: {out}");
    }

    #[test]
    fn disjoint_class_folds_false() {
        let out = out(indoc! {"
            x = \"s\"
            print(x is list[int])
        "});
        assert!(
            out.contains("print(False)"),
            "str excludes list[int]: {out}"
        );
    }

    #[test]
    fn covariant_subtype_folds_true() {
        // `A[int]` is an `A[object]` when `T` is covariant (`out T`), so a
        // statically-`A[int]` value folds the test to True
        let out = out(indoc! {"
            class A[out T]:
                def __init__(self): ...
            def f(a: A[int]) -> bool:
                return a is A[object]
        "});
        assert!(
            out.contains("return True"),
            "covariant A[int] is an A[object]: {out}"
        );
    }

    #[test]
    fn covariant_dynamic_probes_with_variance() {
        // a dynamic value against a covariant target emits a probe whose
        // variance code (1) makes the runtime match respect `out T`
        let out = out(indoc! {"
            class A[out T]:
                def __init__(self): ...
            def f(a: object) -> bool:
                return a is A[object]
        "});
        assert!(
            out.contains("return _parametric_is(a, A[object], (1,))"),
            "covariant probe carries variance code 1: {out}"
        );
    }

    #[test]
    fn use_site_covariant_target_probes_covariantly() {
        // `A[out int]` projects an invariant `T` covariantly for this one
        // test, so the probe matches with code 1 — and the target renders as
        // plain `A[int]`, the keyword having no runtime spelling
        let out = out(indoc! {"
            class A[in out T]:
                def __init__(self): ...
            def f(a: A[*]) -> bool:
                return a is A[out int]
        "});
        assert!(
            out.contains("return _parametric_is(a, A[int], (1,))"),
            "use-site `out` probes covariantly: {out}"
        );
    }

    #[test]
    fn use_site_contravariant_target_probes_contravariantly() {
        let out = out(indoc! {"
            class S[in out T]:
                def __init__(self): ...
            def f(s: S[*]) -> bool:
                return s is S[in bool]
        "});
        assert!(
            out.contains("return _parametric_is(s, S[bool], (2,))"),
            "use-site `in` probes contravariantly: {out}"
        );
    }

    #[test]
    fn unprojected_invariant_target_probes_invariantly() {
        // the counterpart to the two above: without a projection an invariant
        // `T` keeps demanding an exact match
        let out = out(indoc! {"
            class A[in out T]:
                def __init__(self): ...
            def f(a: A[*]) -> bool:
                return a is A[int]
        "});
        assert!(
            out.contains("return _parametric_is(a, A[int], (0,))"),
            "no projection stays invariant: {out}"
        );
    }

    #[test]
    fn use_site_covariant_target_folds_true() {
        // `A[bool]` is an `A[out int]` statically, so this folds rather than
        // probing — the fold must agree with assignability, not contradict it
        let out = out(indoc! {"
            class A[in out T]:
                def __init__(self): ...
            def f(a: A[bool]) -> bool:
                return a is A[out int]
        "});
        assert!(
            out.contains("return True"),
            "A[bool] is an A[out int]: {out}"
        );
    }

    #[test]
    fn use_site_variance_on_declared_covariant_target_is_a_no_op() {
        // a declared `out T` already covers what the projection could give
        let out = out(indoc! {"
            class A[out T]:
                def __init__(self): ...
            def f(a: object) -> bool:
                return a is A[out object]
        "});
        assert!(
            out.contains("return _parametric_is(a, A[object], (1,))"),
            "declared variance wins: {out}"
        );
    }

    #[test]
    fn reified_tuple_target_compares_each_cell() {
        // a `tuple[T, U]` value unifies against the tuple target position by
        // position — the `Tuple::Fixed` unify branch
        let out = out(indoc! {"
            def f[T, U](x: tuple[T, U]) -> bool:
                return x is tuple[int, str]
        "});
        assert!(
            out.contains("return (T == int and U == str)"),
            "tuple target compares each cell: {out}"
        );
    }

    #[test]
    fn nested_generic_value_unifies_recursively() {
        // `A[list[T]]` reaches `T` through two levels; the unify descends the
        // target structure to the cell
        let out = out(indoc! {"
            class A[T]:
                def __init__(self): ...
            def f[T](x: A[list[T]]) -> bool:
                return x is A[list[int]]
        "});
        assert!(
            out.contains("return (T == int)"),
            "nested value unifies to the inner cell: {out}"
        );
    }

    #[test]
    fn multi_param_probe_carries_a_variance_per_param() {
        // two invariant parameters → a two-entry variance tuple, exercising the
        // plural branch of the tuple spelling and the polyfill's per-arg loop
        let out = out(indoc! {"
            class Pair[K, V]:
                def __init__(self, k: K, v: V):
                    self.k: K = k
                    self.v: V = v
            def f(x: object) -> bool:
                return x is Pair[int, str]
        "});
        assert!(
            out.contains("return _parametric_is(x, Pair[int, str], (0, 0))"),
            "one variance code per parameter: {out}"
        );
    }

    #[test]
    fn bivariant_typevar_probes_with_code_three() {
        // a parameter unused in the class body is bivariant; the probe matches
        // either way (code 3)
        let out = out(indoc! {"
            class Box[T]:
                def __init__(self): ...
            def f(x: object) -> bool:
                return x is Box[int]
        "});
        assert!(
            out.contains("return _parametric_is(x, Box[int], (3,))"),
            "bivariant parameter probes with code 3: {out}"
        );
    }

    #[test]
    fn declared_contravariant_dynamic_probes_with_code_two() {
        // the counterpart to `covariant_dynamic_probes_with_variance` for a
        // declared `in T` — the probe carries variance code 2
        let out = out(indoc! {"
            class Sink[in T]:
                def __init__(self): ...
                def put(self, x: T) -> None: ...
            def f(x: object) -> bool:
                return x is Sink[int]
        "});
        assert!(
            out.contains("return _parametric_is(x, Sink[int], (2,))"),
            "declared contravariant probes with code 2: {out}"
        );
    }

    #[test]
    fn protocol_method_return_is_covariant() {
        // a method member is checkable too: its return type is checked
        // covariantly against the value method's reified return annotation. an
        // empty parameter list, then the return `(int, 1)`
        let out = out(indoc! {"
            from typing import Protocol
            class P[T](Protocol):
                def get(self) -> T: ...
            def f(x: object) -> bool:
                return x is P[int]
        "});
        assert!(
            out.contains("return _by_protocol_is(x, [(\"method\", \"get\", [], (int, 1))])"),
            "method return checked covariantly: {out}"
        );
        assert!(
            out.contains("def _by_method_matches"),
            "method-check runtime emitted: {out}"
        );
    }

    #[test]
    fn protocol_method_parameter_is_contravariant() {
        // a method parameter is checked contravariantly (`in out T` keeps `T`
        // invariant so the example is unambiguous); the method declares no
        // meaningful return, so `ret` is `None`
        let out = out(indoc! {"
            from typing import Protocol
            class A[in out T](Protocol):
                def f(self, other: T): ...
            def f(x: object) -> bool:
                return x is A[int]
        "});
        assert!(
            out.contains("return _by_protocol_is(x, [(\"method\", \"f\", [(int, 2)], None)])"),
            "method parameter checked contravariantly: {out}"
        );
    }

    #[test]
    fn protocol_literal_argument_spells_through_the_helper() {
        // `A[True]` specializes `T` to `Literal[True]`, which has no bare
        // runtime spelling — it is rebuilt by the check's own `_by_lit` helper,
        // so the member list needs no import of its own
        let out = out(indoc! {"
            from typing import Protocol
            class A[in out T](Protocol):
                a: T
            def f(x: object) -> bool:
                return x is A[True]
        "});
        assert!(
            out.contains("return _by_protocol_is(x, [(\"attr\", \"a\", _by_lit(True), 0)])"),
            "literal argument spelled through `_by_lit`: {out}"
        );
        assert!(
            out.contains("def _by_lit(*values):"),
            "the literal helper is emitted: {out}"
        );
    }

    #[test]
    fn protocol_int_literal_argument() {
        let out = out(indoc! {"
            from typing import Protocol
            class A[in out T](Protocol):
                a: T
            def f(x: object) -> bool:
                return x is A[3]
        "});
        assert!(
            out.contains("return _by_protocol_is(x, [(\"attr\", \"a\", _by_lit(3), 0)])"),
            "int literal argument: {out}"
        );
    }

    #[test]
    fn protocol_with_unspellable_member_becomes_false() {
        // a member whose specialized type has no runtime spelling (a callable
        // attribute) can't be checked, so the whole protocol falls back to the
        // erased-target error and the lowering is the constant it always is
        let out = out(indoc! {"
            from typing import Protocol
            from collections.abc import Callable
            class P[T](Protocol):
                cb: Callable[[T], T]
            def f(x: object) -> bool:
                return x is P[int]
        "});
        assert!(out.contains("return False"), "protocol target folds: {out}");
        assert!(
            !out.contains("_by_protocol_is"),
            "no structural check for an unspellable member: {out}"
        );
    }

    #[test]
    fn protocol_data_member_checks_reified_annotation() {
        // the headline case: a protocol whose members are all data members can
        // be checked structurally against the value's reified class annotations
        let out = out(indoc! {"
            from typing import Protocol
            class A[T](Protocol):
                a: T
            def f(x: object) -> bool:
                return x is A[int]
        "});
        assert!(
            out.contains("return _by_protocol_is(x, [(\"attr\", \"a\", int, 0)])"),
            "data-member protocol checks reified annotation: {out}"
        );
        assert!(
            out.contains("def _by_protocol_is(value, members):"),
            "structural-check runtime emitted: {out}"
        );
    }

    #[test]
    fn protocol_is_not_negates_structural_check() {
        let out = out(indoc! {"
            from typing import Protocol
            class A[T](Protocol):
                a: T
            def f(x: object) -> bool:
                return x is not A[bool]
        "});
        assert!(
            out.contains("return not _by_protocol_is(x, [(\"attr\", \"a\", bool, 0)])"),
            "is not negates the structural check: {out}"
        );
    }

    #[test]
    fn protocol_multiple_data_members() {
        let out = out(indoc! {"
            from typing import Protocol
            class A[T, U](Protocol):
                a: T
                b: U
            def f(x: object) -> bool:
                return x is A[int, str]
        "});
        assert!(
            out.contains(
                "return _by_protocol_is(x, [(\"attr\", \"a\", int, 0), (\"attr\", \"b\", str, 0)])"
            ),
            "each data member is checked: {out}"
        );
    }

    #[test]
    fn protocol_nested_generic_member_spells_specialized_type() {
        let out = out(indoc! {"
            from typing import Protocol
            class A[T](Protocol):
                a: list[T]
            def f(x: object) -> bool:
                return x is A[int]
        "});
        assert!(
            out.contains("return _by_protocol_is(x, [(\"attr\", \"a\", list[int], 0)])"),
            "member type is spelled with the specialization applied: {out}"
        );
    }

    #[test]
    fn protocol_readonly_property_member_is_covariant() {
        // a read-only property member is covariant, so the value's annotation
        // need only be a subtype (variance code 1)
        let out = out(indoc! {"
            from typing import Protocol
            class A[T](Protocol):
                @property
                def a(self) -> T: ...
            def f(x: object) -> bool:
                return x is A[int]
        "});
        assert!(
            out.contains("return _by_protocol_is(x, [(\"attr\", \"a\", int, 1)])"),
            "read-only property member is covariant: {out}"
        );
    }

    #[test]
    fn protocol_concrete_value_folds_statically() {
        // a value whose static type already answers the structural question is
        // resolved at compile time, with no runtime residue
        let out = out(indoc! {"
            from typing import Protocol
            class A[T](Protocol):
                a: T
            class C:
                a: bool
            def f(c: C) -> bool:
                return c is A[bool]
        "});
        assert!(
            out.contains("return True"),
            "concrete value structurally satisfies the protocol: {out}"
        );
        assert!(
            !out.contains("_by_protocol_is"),
            "no runtime residue for a statically decided test: {out}"
        );
    }

    #[test]
    fn implicit_alias_target_probes_like_the_specialization() {
        // `X = A[int]` binds `X` to the specialization itself, so `y is X`
        // resolves exactly as `y is A[int]` would — and is written back out
        // that way, which is what the runtime probe needs to unwind
        let out = out(indoc! {"
            class A[T]:
                def __init__(self, v: T):
                    self.v: list[T] = [v]
            X = A[int]
            def f(y: object) -> bool:
                return y is X
        "});
        assert!(
            out.contains("return _parametric_is(y, A[int], (0,))"),
            "alias name probes against the specialization it binds: {out}"
        );
    }

    #[test]
    fn implicit_alias_builtin_target_probes() {
        // an alias to a builtin specialization probes just like the direct form
        let out = out(indoc! {"
            X = list[int]
            def f(y: object) -> bool:
                return y is X
        "});
        assert!(
            out.contains("return _parametric_is(y, list[int], (0,))"),
            "alias to a builtin probes against the specialization: {out}"
        );
    }

    #[test]
    fn pep695_type_alias_target_probes_through_value() {
        // `type W = A[int]` evaluates to a `TypeAliasType`, which `isinstance`
        // would refuse — so the specialization it stands for is written out
        // instead of the name
        let out = out(indoc! {"
            class A[T]:
                def __init__(self, v: T):
                    self.v: list[T] = [v]
            type W = A[int]
            def f(y: object) -> bool:
                return y is W
        "});
        assert!(
            out.contains("return _parametric_is(y, A[int], (0,))"),
            "type alias probes against its value: {out}"
        );
    }

    #[test]
    fn bare_class_name_still_lowers_to_isinstance() {
        // a non-generic name target is not a specialization; it keeps the
        // ordinary isinstance lowering that identity_swap used to own
        let out = out(indoc! {"
            def f(y: object) -> bool:
                return y is int
        "});
        assert!(
            out.contains("return isinstance(y, int)"),
            "bare class → isinstance: {out}"
        );
    }

    #[test]
    fn union_of_plain_classes_ors_isinstance() {
        // a union target is the disjunction of its arms — never a runtime
        // `isinstance(a, X | Y)`, which fails on a parameterized arm and even on
        // plain classes before python 3.10
        let out = out(indoc! {"
            def f(a: object) -> bool:
                return a is int | str
        "});
        assert!(
            out.contains("return (isinstance(a, int) or isinstance(a, str))"),
            "plain union ORs isinstance per arm: {out}"
        );
    }

    #[test]
    fn union_mixes_probe_and_isinstance_per_arm() {
        let out = out(indoc! {"
            class A[T]:
                def __init__(self, v: T):
                    self.v: list[T] = [v]
            def f(a: object) -> bool:
                return a is A[int] | str
        "});
        assert!(
            out.contains("return (_parametric_is(a, A[int], (0,)) or isinstance(a, str))"),
            "each arm lowered by its own kind: {out}"
        );
    }

    #[test]
    fn union_negation_wraps_the_disjunction() {
        let out = out(indoc! {"
            def f(a: object) -> bool:
                return a is not int | str
        "});
        assert!(
            out.contains("return not (isinstance(a, int) or isinstance(a, str))"),
            "`is not` negates the whole disjunction: {out}"
        );
    }

    #[test]
    fn union_three_arms() {
        let out = out(indoc! {"
            def f(a: object) -> bool:
                return a is int | str | bytes
        "});
        assert!(
            out.contains(
                "return (isinstance(a, int) or isinstance(a, str) or isinstance(a, bytes))"
            ),
            "a flat chain of arms: {out}"
        );
    }

    #[test]
    fn union_none_arm_is_an_identity_check() {
        // `X | None` (an optional) tests the `None` arm by identity — `None` is
        // a value, so `isinstance(a, None)` would be a runtime `TypeError`
        let out = out(indoc! {"
            def f(a: object) -> bool:
                return a is int | None
        "});
        assert!(
            out.contains("return (isinstance(a, int) or a is None)"),
            "None arm is an identity check: {out}"
        );
    }

    #[test]
    fn union_effectful_lhs_binds_once_via_lambda() {
        // an effectful lhs must be evaluated exactly once across the arms, so it
        // is bound to a lambda parameter rather than referenced per arm
        let out = out(indoc! {"
            def g() -> object:
                return 1
            def f() -> bool:
                return g() is int | str
        "});
        assert!(
            out.contains(
                "return (lambda _by_is_value: (isinstance(_by_is_value, int) or \
                 isinstance(_by_is_value, str)))(g())"
            ),
            "effectful lhs bound once: {out}"
        );
    }

    #[test]
    fn effectful_lhs_preserved_in_fold() {
        let out = out(indoc! {"
            def g() -> list[int]:
                return [1]
            print(g() is list[int])
        "});
        assert!(
            out.contains("print((g(), True)[1])"),
            "side effects must survive the fold: {out}"
        );
    }

    #[test]
    fn bare_typevar_compares_reified_cell() {
        let out = out(indoc! {"
            def f[T](x: T) -> bool:
                return x is list[int]
        "});
        assert!(
            out.contains("return (T == list[int])"),
            "bare typevar compares whole alias: {out}"
        );
        assert!(
            out.contains("@generic  # basedpython: reified"),
            "the parametric test must reify T: {out}"
        );
    }

    #[test]
    fn structural_typevar_unifies() {
        let out = out(indoc! {"
            def f[T](x: list[T]) -> bool:
                return x is list[int]
        "});
        assert!(
            out.contains("return (T == int)"),
            "list[T] vs list[int] unifies to T == int: {out}"
        );
    }

    #[test]
    fn is_not_negates() {
        let out = out(indoc! {"
            def f[T](x: T) -> bool:
                return x is not list[int]
        "});
        assert!(
            out.contains("return not (T == list[int])"),
            "is not negates the token check: {out}"
        );
    }

    #[test]
    fn dynamic_value_against_user_generic_probes_orig_class() {
        // a user-defined generic's instances carry `__orig_class__`, so a
        // dynamic value against it is a valid runtime probe
        let out = out(indoc! {"
            class A[T]:
                def __init__(self, t: T): ...
            def f(x) -> bool:
                return x is A[int]
        "});
        assert!(
            out.contains("return _parametric_is(x, A[int], ("),
            "dynamic lhs against a user generic probes __orig_class__: {out}"
        );
        assert!(
            out.contains("def _parametric_is(value, alias, variances):"),
            "probe polyfill emitted: {out}"
        );
    }

    #[test]
    fn dynamic_value_against_builtin_probes_mro() {
        // a builtin collection target is checkable after all: the probe unwinds
        // the value's mro/`__orig_bases__`, so a concrete subclass that fixes
        // the arguments (`class B(list[int])`) answers True while a bare list
        // answers False — sound, since the test narrows only its positive branch
        let out = out(indoc! {"
            def f(x) -> bool:
                return x is list[int]
        "});
        assert!(
            out.contains("return _parametric_is(x, list[int], (0,))"),
            "builtin target probes via mro: {out}"
        );
    }

    #[test]
    fn builtin_union_compares_the_reified_cell() {
        // a union of builtin specializations is not probeable — the arms are
        // the same C-level `list` and a bare list answers False for every one.
        // the parameter is given a reified type parameter instead (see
        // [`erased_union`](super::super::erased_union)), so the test compares
        // the cell the call site supplied rather than interrogating the value
        let out = out(indoc! {"
            def f(x: list[int] | list[str]) -> bool:
                return x is list[int]
        "});
        assert!(
            out.contains("return (__by_erased_0 == int)"),
            "builtin union compares the reified cell: {out}"
        );
    }

    #[test]
    fn user_generic_union_probes_orig_class() {
        // a user generic carries `__orig_class__`, so a union against it is
        // soundly discriminated per arm by the probe (an invariant field
        // keeps ty from collapsing the union)
        let out = out(indoc! {"
            class A[T]:
                def __init__(self, t: T):
                    self.v: list[T] = [t]
            def f(x: A[int] | A[str]) -> bool:
                return x is A[int]
        "});
        assert!(
            out.contains("return _parametric_is(x, A[int], (0,))"),
            "user-generic union probes __orig_class__ with invariant T: {out}"
        );
    }

    #[test]
    fn a_target_outside_the_union_can_never_hold() {
        // no arm matches `list[bytes]`, so this can never be true — and the
        // constraint `(int, str)` on the reified parameter says so statically,
        // before any runtime residue is reached
        let out = out(indoc! {"
            def f(x: list[int] | list[str]) -> bool:
                return x is list[bytes]
        "});
        assert!(
            out.contains("return False"),
            "a target outside the union can never hold: {out}"
        );
    }

    #[test]
    fn a_subscript_of_a_value_keeps_the_plain_instance_check() {
        // `pair[0]` is not something a type expression can say, so the checker
        // reports it and the lowering keeps the `isinstance` the source wrote
        let out = out(indoc! {"
            class A: ...
            pair = (A, A)
            x = A()
            print(x is pair[0])
        "});
        assert!(
            out.contains("print(isinstance(x, pair[0]))"),
            "non-alias subscript rhs is a plain isinstance: {out}"
        );
    }

    #[test]
    fn identity_operator_untouched() {
        // `===` keeps python identity semantics even against an alias
        let out = out(indoc! {"
            xs = [1]
            print(xs === list[int])
        "});
        assert!(
            out.contains("print(xs is list[int])"),
            "=== stays identity: {out}"
        );
    }

    #[test]
    fn builtin_multi_arg_target_probes() {
        // a two-argument builtin target probes with a variance code per
        // parameter; the runtime unwinds `dict`-fixing subclasses via the mro
        let out = out(indoc! {"
            def f(x) -> bool:
                return x is dict[str, int]
        "});
        assert!(
            out.contains("return _parametric_is(x, dict[str, int], (0, 0))"),
            "builtin dict target probes: {out}"
        );
    }

    #[test]
    fn stdlib_enum_member_rhs_keeps_identity() {
        // `Color.RED` names the type `Literal[Color.RED]`, which holds exactly
        // one object — so the test is identity, and `isinstance(x, Color.RED)`
        // (a runtime `TypeError`) is never emitted
        let out = out(indoc! {"
            from enum import Enum

            class Color(Enum):
                RED = 1
                GREEN = 2

            def f(c: Color) -> None:
                print(c is Color.RED)
                print(c is not Color.GREEN)
        "});
        assert!(
            out.contains("print(c is Color.RED)"),
            "enum member rhs is an identity check: {out}"
        );
        assert!(
            out.contains("print(c is not Color.GREEN)"),
            "enum member rhs is an identity check under `is not`: {out}"
        );
    }

    #[test]
    fn an_alias_to_the_enum_still_reaches_the_variant() {
        // the target is decided from the *type* it names, so a binding standing
        // in for the enum reaches the same variant. a match on the written name
        // would miss this one and emit `isinstance` against a singleton
        let out = out(indoc! {"
            enum class Shape:
                case Circle(radius: float)
                case Point

            S = Shape

            def f(s: Shape) -> bool:
                return s is S.Point
        "});
        assert!(
            out.contains("return s is S.Point"),
            "an aliased enum's unit variant is still an identity check: {out}"
        );
    }

    #[test]
    fn a_binding_shadowing_the_enum_names_what_it_holds() {
        // the same rule the other way: `Shape` here is the local class, whose
        // `Point` is an ordinary class and therefore an instance check
        let out = out(indoc! {"
            enum class Shape:
                case Circle(radius: float)
                case Point

            class Other:
                class Point: pass

            def f(x: object) -> bool:
                Shape = Other
                return x is Shape.Point
        "});
        assert!(
            out.contains("return isinstance(x, Shape.Point)"),
            "a shadowed name names what the binding holds: {out}"
        );
    }

    #[test]
    fn a_literal_target_keeps_the_source_spelling() {
        // the source already wrote a literal that is valid where it sits —
        // rebuilding one would pick its own quote character, which a python
        // before 3.12 forbids reusing inside an f-string
        let out = out(indoc! {"
            def f(x: object) -> str:
                return f\"{x is 'q'}\"
        "});
        assert!(
            out.contains("x == 'q'"),
            "the literal is re-emitted as written: {out}"
        );
    }

    #[test]
    fn an_effectful_value_is_evaluated_once_for_a_literal_target() {
        // the equality check mentions the value twice — once for the class
        // guard — so an effectful operand is bound rather than repeated
        let out = out(indoc! {"
            def g() -> object:
                return 1

            def f() -> bool:
                return g() is 1
        "});
        assert_eq!(out.matches("g()").count(), 2, "single evaluation:\n{out}");
    }

    #[test]
    fn a_pattern_target_escapes_what_the_source_cannot_carry() {
        // a control character has no raw spelling in python source — CPython
        // refuses a file containing a NUL outright — so the regex carries its
        // escape instead
        let out = out(indoc! {"
            def f(s: str) -> bool:
                return s is f\"a\\x00b{int}\"
        "});
        assert!(
            out.contains("\\\\x00") || out.contains("a\\\\x00b"),
            "the pattern escapes the control character: {out}"
        );
        assert!(!out.contains('\0'), "no raw NUL reaches the output: {out}");
    }

    #[test]
    fn caseless_based_variant_rhs_keeps_identity() {
        // the repro: a payload-less based-enum variant is a singleton instance
        let out = out(indoc! {"
            enum class Genre:
                case A, B

            def f(g: Genre) -> None:
                print(g is not Genre.B)
        "});
        assert!(
            out.contains("print(g is not Genre.B)"),
            "caseless variant rhs is an identity check: {out}"
        );
    }

    #[test]
    fn payload_variant_class_rhs_lowers_but_unit_variant_kept() {
        // a payload variant resolves to a *class* (→ isinstance); a unit
        // variant in the same enum is a singleton instance (→ kept)
        let out = out(indoc! {"
            enum class Shape:
                case Circle(radius: float)
                case Point

            def f(c: Shape) -> None:
                print(c is Shape.Circle)
                print(c is not Shape.Point)
        "});
        assert!(
            out.contains("print(isinstance(c, _Shape_Circle))"),
            "payload variant rhs is a class and lowers to isinstance: {out}"
        );
        assert!(
            out.contains("print(c is not Shape.Point)"),
            "unit variant rhs is a singleton instance and keeps identity: {out}"
        );
    }
}
