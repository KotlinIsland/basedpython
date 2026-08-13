//! runtime soundness-check queries for the basedpython transpiler
//!
//! the transpiler inserts `_soundness_check(expr, target)` validations at
//! expressions whose inferred type rests on an assumption ty cannot verify:
//! a typevar solution at a generic call site, an annotated container's
//! element projection, or an explicit `Any` flowing into a declared binding.
//! these queries answer the two questions that decide an insertion:
//!
//! - does this expression's type rest on such an assumption? (the gates:
//!   [`call_result_is_typevar_derived`], [`is_specialized_generic_instance`])
//! - can the inferred type be validated with `isinstance` at runtime, and
//!   what second argument does that check take? ([`runtime_check_target`])

use ruff_db::files::File;

use crate::Db;
use crate::place::{Place, explicit_global_symbol};
use crate::types::ProgramEnvironment;
use crate::types::instance::Protocol;
use crate::types::literal::LiteralValueTypeKind;
use crate::types::reified_infer::{
    ArgVariance, parametric_soundness_spelling, protocol_structural_members,
};
use crate::types::signatures::CallableSignature;
use crate::types::visitor::any_over_type;
use crate::types::{ClassLiteral, ClassType, FunctionType, KnownClass, Type};
use ty_module_resolver::{KnownModule, file_to_module};

/// How a runtime soundness check validates a value against a target type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckKind {
    /// a shallow `isinstance(value, <target>)` check — the string is the
    /// second `isinstance` argument (`str`, `(int, type(None))`)
    Isinstance(String),
    /// a deep check that validates the base class *and*, when the value
    /// carries `__orig_class__`, its type arguments. `alias` is the runtime
    /// spelling of the specialization (`A[int]`); `variances` is one code per
    /// type parameter (0 invariant, 1 covariant, 2 contravariant, 3 bivariant)
    Parametric { alias: String, variances: Vec<u8> },
}

/// How a checked `cast` / `cast?` validates its value against the target type.
///
/// A superset of [`CheckKind`]: a cast target may be a *protocol*, which a
/// general soundness check simply skips but a cast must handle without emitting
/// a runtime `isinstance` against a subscripted (or non-`@runtime_checkable`)
/// protocol — which would raise `TypeError` at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastCheck {
    /// a shallow or deep soundness check, exactly as a soundness insertion uses
    Kind(CheckKind),
    /// basedpython: a protocol target the structural check can validate against
    /// the value's reified annotations. the member list itself comes from the
    /// shared parametric engine at lowering time, so this only records *that*
    /// the target is checkable
    Protocol,
    /// a protocol target with no faithful runtime check — a member whose
    /// specialized type has no runtime spelling (a callable attribute). no sound
    /// residue exists, so the checked cast degrades to an unchecked
    /// `typing.cast` rather than a crashing `isinstance`
    Unchecked,
    /// basedpython: a `Literal` target, checked by *membership* — the strings
    /// are the python spellings of the admissible values. a `Literal` is a set
    /// of values rather than a class, so `isinstance` cannot test one at all
    /// (`isinstance(v, Literal["a"])` raises), and promoting it to the values'
    /// class would accept every other value of that class
    Members(Vec<String>),
}

/// basedpython: the python spellings of the values a `Literal` target admits,
/// when every arm of `ty` is a literal whose value has one.
///
/// An enum member is deliberately excluded: its spelling depends on the enum's
/// name being bound in the emitting module, which the shallow `isinstance`
/// against the enum class already covers.
fn literal_members<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<Vec<String>> {
    fn collect<'db>(db: &'db dyn Db, ty: Type<'db>, out: &mut Vec<String>) -> bool {
        match ty {
            Type::TypeAlias(alias) => collect(db, alias.value_type(db), out),
            Type::Union(union) => union
                .elements(db)
                .iter()
                .all(|element| collect(db, *element, out)),
            _ if ty.is_none(db) => {
                out.push("None".to_owned());
                true
            }
            Type::LiteralValue(literal) => match literal.kind() {
                LiteralValueTypeKind::Bool(value) => {
                    out.push(if value { "True" } else { "False" }.to_owned());
                    true
                }
                LiteralValueTypeKind::Int(value) => {
                    out.push(value.as_i64().to_string());
                    true
                }
                LiteralValueTypeKind::String(value) => {
                    out.push(python_string_literal(value.value(db)));
                    true
                }
                // a bytes literal only has a faithful spelling when every byte
                // is ascii; anything else would need an escape table this does
                // not carry, and falls back to the ordinary plan
                LiteralValueTypeKind::Bytes(value) => match str::from_utf8(value.value(db)) {
                    Ok(text) if text.is_ascii() => {
                        out.push(format!("b{}", python_string_literal(text)));
                        true
                    }
                    _ => false,
                },
                _ => false,
            },
            _ => false,
        }
    }

    let mut members = Vec::new();
    (collect(db, ty, &mut members) && !members.is_empty()).then_some(members)
}

/// a python string literal spelling `value`, double-quoted, with the characters
/// python needs escaped escaped
fn python_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
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

/// The runtime check a checked `cast` / `cast?` applies to validate its value
/// against target type `ty`.
///
/// Unlike a general soundness check, a protocol target is not skipped: a
/// data-member protocol is checked structurally ([`CastCheck::Protocol`]) and a
/// protocol with no faithful check degrades to [`CastCheck::Unchecked`]. This
/// keeps the emitted code from ever running `isinstance` against a subscripted
/// or non-`@runtime_checkable` protocol, which raises at runtime.
pub fn cast_check_plan<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    ty: Type<'db>,
) -> Option<CastCheck> {
    if let Type::ProtocolInstance(instance) = ty {
        if let Protocol::FromClass(protocol_class) = instance.inner
            && protocol_structural_members(db, env, file, *protocol_class).is_some()
        {
            return Some(CastCheck::Protocol);
        }
        // a protocol with a method member, an unspellable data member, or a
        // synthesized structural protocol has no annotation to check against
        return Some(CastCheck::Unchecked);
    }
    if let Some(members) = literal_members(db, ty) {
        return Some(CastCheck::Members(members));
    }
    runtime_check_plan(db, env, file, ty).map(CastCheck::Kind)
}

/// whether a checked cast to `target` has no faithful runtime residue and must
/// degrade to an unchecked `typing.cast`: a protocol target that
/// [`cast_check_plan`] cannot validate structurally. this is what stops the
/// transpiler emitting `isinstance(value, <protocol>)`, which raises at runtime
pub fn cast_target_is_unverifiable_protocol<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    target: Type<'db>,
) -> bool {
    matches!(
        cast_check_plan(db, env, file, target),
        Some(CastCheck::Unchecked)
    )
}

/// whether a call through `callee` produces a result whose type was derived
/// by substituting typevars: the declared return type mentions a typevar
/// (`def t[T]() -> T`), or the method is bound to a specialized generic
/// instance (`dict[str, int].get`), where the specialization itself is an
/// unverified annotation-level claim
pub fn call_result_is_typevar_derived<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    callee: Type<'db>,
) -> bool {
    match callee {
        Type::FunctionLiteral(function) => return_mentions_type_var(db, env, function),
        Type::BoundMethod(method) => {
            is_specialized_generic_instance(db, env, method.self_instance(db))
                || return_mentions_type_var(db, env, method.function(db))
        }
        _ => false,
    }
}

fn return_mentions_type_var<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    function: FunctionType<'db>,
) -> bool {
    function
        .signature(db)
        .overloads
        .iter()
        .any(|signature| mentions_type_var(db, env, signature.return_ty))
}

fn mentions_type_var<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> bool {
    any_over_type(db, env, ty, false, |nested| {
        matches!(nested, Type::TypeVar(_))
    })
}

/// whether `ty` is an instance whose static shape includes a generic
/// specialization — the runtime-unverifiable part of an annotation like
/// `list[str]`. element projections out of such values (subscripts, method
/// results, iteration) are where the specialization's claim is consumed
pub fn is_specialized_generic_instance<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> bool {
    match ty {
        Type::NominalInstance(instance) => {
            matches!(instance.class(db, env), ClassType::Generic(_))
        }
        // a generic protocol instance (`Iterator[str]`, `Sequence[int]`)
        // carries the same annotation-level specialization claim
        Type::ProtocolInstance(instance) => match instance.inner {
            Protocol::Materialized(_) => false,
            Protocol::FromClass(class) => matches!(*class, ClassType::Generic(_)),
            Protocol::Synthesized(_) => false,
        },
        // a TypedDict value is a plain dict at runtime; its per-key value
        // types are annotation-level claims just like a specialization
        Type::TypedDict(_) => true,
        Type::Union(union) => union
            .elements(db)
            .iter()
            .any(|element| is_specialized_generic_instance(db, env, *element)),
        _ => false,
    }
}

/// render `ty` as a second argument for a runtime `isinstance` check
/// (e.g. `str`, `(int, type(None))`), or `None` when the type has no
/// faithful shallow runtime test (protocols, callables, dynamic types,
/// unsolved typevars) or its name cannot be resolved at module scope in
/// `file`. the check is deliberately shallow: `list[str]` validates as
/// `list` — the element claim is validated at its own projection sites
pub fn runtime_check_target<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    ty: Type<'db>,
) -> Option<String> {
    target(db, env, file, ty, 0)
}

/// The runtime soundness check for a value whose declared type is `ty`.
///
/// Prefers a [`CheckKind::Parametric`] deep check when `ty` is a user-defined
/// generic specialization whose instances carry `__orig_class__` (so the type
/// arguments are checkable at runtime); otherwise falls back to the shallow
/// [`CheckKind::Isinstance`] of [`runtime_check_target`]. `None` when neither
/// applies (no faithful runtime test).
pub fn runtime_check_plan<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    ty: Type<'db>,
) -> Option<CheckKind> {
    if let Some((alias, variances)) = parametric_soundness_spelling(db, env, file, ty) {
        return Some(CheckKind::Parametric {
            alias,
            variances: variances.iter().copied().map(variance_code).collect(),
        });
    }
    runtime_check_target(db, env, file, ty).map(CheckKind::Isinstance)
}

/// whether a runtime check against `ty` must silently drop a type-argument
/// claim: `ty` carries a written generic specialization, but its arguments are
/// erased at runtime, so only the origin class can be tested. this is what
/// separates `list[int]` (a builtin, erased — only `list` is checkable) from
/// `A[int]` (a user generic, whose instances carry `__orig_class__`)
pub fn erases_type_arguments<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    ty: Type<'db>,
) -> bool {
    match ty {
        Type::NominalInstance(instance) => {
            let ClassType::Generic(alias) = instance.class(db, env) else {
                return false;
            };
            // a bare `list` infers as `list[Unknown]`: no argument was written,
            // so there is no claim to drop
            alias
                .specialization(db)
                .types(db)
                .iter()
                .any(|argument| !argument.is_dynamic())
                && parametric_soundness_spelling(db, env, file, ty).is_none()
        }
        Type::Union(union) => union
            .elements(db)
            .iter()
            .any(|element| erases_type_arguments(db, env, file, *element)),
        _ => false,
    }
}

/// whether a `cast` from `value_ty` to `target` needs no runtime verification
/// because the checker already proves the value is the target: a runtime check
/// would always pass, so it is redundant. this both saves the probe and avoids
/// emitting one that cannot run — a subscripted builtin (`list[int]`) whose
/// arguments are erased, or a subscripted protocol (`Sequence[object]`) whose
/// bare `isinstance` is itself a runtime error. gradual `Any`/`Unknown` values
/// are *not* subtypes of a concrete target, so their checks are kept
pub fn cast_is_redundant<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    value_ty: Type<'db>,
    target: Type<'db>,
) -> bool {
    value_ty.is_subtype_of(db, env, target)
}

/// the runtime variance code the `_parametric_is` probe expects
fn variance_code(variance: ArgVariance) -> u8 {
    match variance {
        ArgVariance::Invariant => 0,
        ArgVariance::Covariant => 1,
        ArgVariance::Contravariant => 2,
        ArgVariance::Bivariant => 3,
    }
}

fn target<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    ty: Type<'db>,
    depth: u8,
) -> Option<String> {
    if depth > 8 {
        return None;
    }
    // literals promote to their instance form (`Literal[3]` → `int`,
    // `LiteralString` → `str`, enum literals → the enum class)
    let ty = ty.promote(db, env);
    match ty {
        Type::NominalInstance(instance) => {
            if ty.is_none(db) {
                return Some("type(None)".to_owned());
            }
            let class = instance.class(db, env);
            let literal = class.class_literal(db);
            // `object` always passes — a check would validate nothing
            if literal.is_known(db, KnownClass::Object) {
                return None;
            }
            class_target(db, file, literal)
        }
        Type::Union(union) => {
            let mut parts: Vec<String> = Vec::new();
            for element in union.elements(db) {
                let part = target(db, env, file, *element, depth + 1)?;
                if !parts.contains(&part) {
                    parts.push(part);
                }
            }
            match parts.len() {
                0 => None,
                1 => parts.pop(),
                // isinstance accepts nested tuples, so union parts that are
                // themselves rendered unions compose without flattening
                _ => Some(format!("({})", parts.join(", "))),
            }
        }
        // a TypedDict inhabitant is a plain dict at runtime
        Type::TypedDict(_) => builtin_target(db, file, "dict"),
        // values that are classes: the shallow runtime fact is `type`
        Type::ClassLiteral(_) | Type::GenericAlias(_) | Type::SubclassOf(_) => {
            builtin_target(db, file, "type")
        }
        Type::TypeIs(_) | Type::TypeGuard(_) => builtin_target(db, file, "bool"),
        Type::TypeAlias(alias) => target(db, env, file, alias.value_type(db), depth + 1),
        _ => None,
    }
}

/// resolve the runtime name for `literal` as seen from module scope in
/// `file`: either the module binds the class's name to this exact class
/// (definition or import), or the name is unbound and the class is a
/// builtin so the bare name reaches it
fn class_target<'db>(db: &'db dyn Db, file: File, literal: ClassLiteral<'db>) -> Option<String> {
    let name = literal.name(db).as_str();
    match explicit_global_symbol(db, db.program_file(file), name).place {
        Place::Defined(defined) if defined.ty == Type::ClassLiteral(literal) => {
            Some(name.to_owned())
        }
        Place::Undefined if class_is_builtin(db, literal) => Some(name.to_owned()),
        _ => None,
    }
}

fn class_is_builtin<'db>(db: &'db dyn Db, literal: ClassLiteral<'db>) -> bool {
    file_to_module(db, literal.program_file(db).resolver_file(db))
        .is_some_and(|module| module.is_known(db, KnownModule::Builtins))
}

/// a builtin referenced by bare name is only trustworthy when the module
/// does not rebind that name
fn builtin_target(db: &dyn Db, file: File, name: &str) -> Option<String> {
    match explicit_global_symbol(db, db.program_file(file), name).place {
        Place::Undefined => Some(name.to_owned()),
        Place::Defined(_) => None,
    }
}

/// selects which parameter of a call an argument binds to, for
/// [`parameter_check_plan`]
#[derive(Clone, Copy)]
pub enum ArgSelector<'a> {
    /// a positional argument at the given 0-based index (after any bound
    /// `self`/`cls`, which the bound-method signature already drops)
    Positional(usize),
    /// a keyword argument passed by name
    Keyword(&'a str),
}

/// the runtime soundness check for the parameter that `selector` binds to in
/// a call through `callee`, or `None` when the boundary can't be validated
/// faithfully: a non-function callee, an overloaded signature (overload
/// resolution is ty's job, and the wrong overload's parameter type would be
/// misleading), a variadic / unmatched / unannotated parameter, or a
/// parameter type with no runtime test. deliberately conservative —
/// a missed check is a no-op, a wrong one changes semantics
pub fn parameter_check_plan<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    callee: Type<'db>,
    selector: ArgSelector<'_>,
) -> Option<CheckKind> {
    let signature = single_signature(db, callee)?;
    let parameters = signature.parameters();
    let parameter = match selector {
        ArgSelector::Positional(index) => parameters.get_positional(index)?,
        ArgSelector::Keyword(name) => parameters.keyword_by_name(name).map(|(_, p)| p)?,
    };
    // `*args` / `**kwargs` collect many values into a container — the
    // annotation describes the element, not the argument as passed, so a
    // direct isinstance would be wrong
    if parameter.is_variadic() || parameter.is_keyword_variadic() {
        return None;
    }
    runtime_check_plan(db, env, file, parameter.annotated_type())
}

/// the sole overload of `callee`'s signature, or `None` if `callee` is not a
/// plain function / bound method or is overloaded
pub(crate) fn single_signature<'db>(
    db: &'db dyn Db,
    callee: Type<'db>,
) -> Option<crate::types::signatures::Signature<'db>> {
    let signature: CallableSignature<'db> = match callee {
        Type::FunctionLiteral(function) => function.signature(db).clone(),
        Type::BoundMethod(method) => method.bound_signatures(db).clone(),
        _ => return None,
    };
    let [overload] = signature.overloads.as_slice() else {
        return None;
    };
    Some(overload.clone())
}
