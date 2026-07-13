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
use crate::types::instance::Protocol;
use crate::types::visitor::any_over_type;
use crate::types::{ClassLiteral, ClassType, FunctionType, KnownClass, Type};
use ty_module_resolver::{KnownModule, file_to_module};

/// whether a call through `callee` produces a result whose type was derived
/// by substituting typevars: the declared return type mentions a typevar
/// (`def t[T]() -> T`), or the method is bound to a specialized generic
/// instance (`dict[str, int].get`), where the specialization itself is an
/// unverified annotation-level claim
pub fn call_result_is_typevar_derived<'db>(db: &'db dyn Db, callee: Type<'db>) -> bool {
    match callee {
        Type::FunctionLiteral(function) => return_mentions_type_var(db, function),
        Type::BoundMethod(method) => {
            is_specialized_generic_instance(db, method.self_instance(db))
                || return_mentions_type_var(db, method.function(db))
        }
        _ => false,
    }
}

fn return_mentions_type_var<'db>(db: &'db dyn Db, function: FunctionType<'db>) -> bool {
    function
        .signature(db)
        .overloads
        .iter()
        .any(|signature| mentions_type_var(db, signature.return_ty))
}

fn mentions_type_var<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    any_over_type(db, ty, false, |nested| matches!(nested, Type::TypeVar(_)))
}

/// whether `ty` is an instance whose static shape includes a generic
/// specialization — the runtime-unverifiable part of an annotation like
/// `list[str]`. element projections out of such values (subscripts, method
/// results, iteration) are where the specialization's claim is consumed
pub fn is_specialized_generic_instance<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    match ty {
        Type::NominalInstance(instance) => {
            matches!(instance.class(db), ClassType::Generic(_))
        }
        // a generic protocol instance (`Iterator[str]`, `Sequence[int]`)
        // carries the same annotation-level specialization claim
        Type::ProtocolInstance(instance) => match instance.inner {
            Protocol::FromClass(class) => matches!(*class, ClassType::Generic(_)),
            Protocol::Synthesized(_) => false,
        },
        // a TypedDict value is a plain dict at runtime; its per-key value
        // types are annotation-level claims just like a specialization
        Type::TypedDict(_) => true,
        Type::Union(union) => union
            .elements(db)
            .iter()
            .any(|element| is_specialized_generic_instance(db, *element)),
        _ => false,
    }
}

/// render `ty` as a second argument for a runtime `isinstance` check
/// (e.g. `str`, `(int, type(None))`), or `None` when the type has no
/// faithful shallow runtime test (protocols, callables, dynamic types,
/// unsolved typevars) or its name cannot be resolved at module scope in
/// `file`. the check is deliberately shallow: `list[str]` validates as
/// `list` — the element claim is validated at its own projection sites
pub fn runtime_check_target<'db>(db: &'db dyn Db, file: File, ty: Type<'db>) -> Option<String> {
    target(db, file, ty, 0)
}

fn target<'db>(db: &'db dyn Db, file: File, ty: Type<'db>, depth: u8) -> Option<String> {
    if depth > 8 {
        return None;
    }
    // literals promote to their instance form (`Literal[3]` → `int`,
    // `LiteralString` → `str`, enum literals → the enum class)
    let ty = ty.promote(db);
    match ty {
        Type::NominalInstance(instance) => {
            if ty.is_none(db) {
                return Some("type(None)".to_owned());
            }
            let class = instance.class(db);
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
                let part = target(db, file, *element, depth + 1)?;
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
        Type::TypeAlias(alias) => target(db, file, alias.value_type(db), depth + 1),
        _ => None,
    }
}

/// resolve the runtime name for `literal` as seen from module scope in
/// `file`: either the module binds the class's name to this exact class
/// (definition or import), or the name is unbound and the class is a
/// builtin so the bare name reaches it
fn class_target<'db>(db: &'db dyn Db, file: File, literal: ClassLiteral<'db>) -> Option<String> {
    let name = literal.name(db).as_str();
    match explicit_global_symbol(db, file, name).place {
        Place::Defined(defined) if defined.ty == Type::ClassLiteral(literal) => {
            Some(name.to_owned())
        }
        Place::Undefined if class_is_builtin(db, literal) => Some(name.to_owned()),
        _ => None,
    }
}

fn class_is_builtin<'db>(db: &'db dyn Db, literal: ClassLiteral<'db>) -> bool {
    file_to_module(db, literal.file(db))
        .is_some_and(|module| module.is_known(db, KnownModule::Builtins))
}

/// a builtin referenced by bare name is only trustworthy when the module
/// does not rebind that name
fn builtin_target(db: &dyn Db, file: File, name: &str) -> Option<String> {
    match explicit_global_symbol(db, file, name).place {
        Place::Undefined => Some(name.to_owned()),
        Place::Defined(_) => None,
    }
}
