//! basedpython implicit receivers (`int.() -> str`)
//!
//! a callable type may declare a *receiver*: the leading positional parameter of
//! `int.() -> str` is bound as the receiver rather than passed like an ordinary
//! argument. it stays a real parameter — any function of the same shape satisfies
//! the type, and the callable can be called directly — and additionally unlocks
//! two forms this module answers for the checker and the transpiler:
//!
//! - `x.fn()`, where `fn` is a name in scope declared as a receiver callable that
//!   accepts `x`. only reached when `x` has no member `fn` of its own
//! - the body of a [trailing lambda] block bound to a receiver callback, where the
//!   receiver's members are in scope unqualified (`imag` for an `int` receiver)
//!
//! both are *last* fallbacks: a declared member, and any name bound anywhere in
//! the lexical chain, keeps its ordinary meaning. that is what makes the forms
//! purely additive — nothing that resolves today changes meaning
//!
//! [trailing lambda]: crate::types::trailing_lambda

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ty_python_core::scope::{ScopeId, ScopeKind};
use ty_python_core::{place_table, semantic_index};

use crate::Db;
use crate::place::{
    ConsideredDefinitions, builtins_symbol, is_basedpython_implicit_typing_name,
    module_type_implicit_global_symbol, symbol,
};
use crate::types::signatures::{Parameters, Signature};
use crate::types::{Type, TypeContext, infer_expression_types};

/// the single signature of `ty` when it is a callable that declares a receiver
fn receiver_signature<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<&'db Signature<'db>> {
    let Type::Callable(callable) = ty else {
        return None;
    };
    let [signature] = callable.signatures(db).overloads.as_slice() else {
        return None;
    };
    signature
        .parameters()
        .iter()
        .next()?
        .is_receiver()
        .then_some(signature)
}

/// the type `ty` binds as its receiver, when it is a receiver callable
pub(crate) fn receiver_type<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<Type<'db>> {
    Some(
        receiver_signature(db, ty)?
            .parameters()
            .get_positional(0)?
            .annotated_type(),
    )
}

/// `ty` with its receiver supplied — the callable `x.fn` evaluates to. `None`
/// when `ty` is not a receiver callable, or its receiver does not accept
/// `receiver_ty`
fn bind_receiver<'db>(db: &'db dyn Db, ty: Type<'db>, receiver_ty: Type<'db>) -> Option<Type<'db>> {
    let signature = receiver_signature(db, ty)?;
    let receiver = signature.parameters().get_positional(0)?;
    if !receiver_ty.is_assignable_to(db, receiver.annotated_type()) {
        return None;
    }
    let rest = signature.parameters().iter().skip(1).cloned();
    Some(Type::single_callable(
        db,
        Signature::new_generic(
            signature.generic_context,
            Parameters::from_annotation(db, rest),
            signature.return_ty,
        ),
    ))
}

/// basedpython: the callable `x.fn` resolves to when `fn` names a receiver
/// callable in scope that accepts `x`, with the receiver already bound. The
/// name must be *declared* — a receiver callable is only ever spelled as an
/// annotation, and a declaration has one type wherever it is visible. A scope
/// that binds the name to anything else shadows it, the same way it would shadow
/// the name in an ordinary load
pub(crate) fn resolve_receiver_attribute<'db>(
    db: &'db dyn Db,
    file: File,
    scope: ScopeId<'db>,
    receiver_ty: Type<'db>,
    name: &str,
) -> Option<Type<'db>> {
    let index = semantic_index(db, file);
    for (ancestor_id, _) in index.visible_ancestor_scopes(scope.file_scope_id(db)) {
        let ancestor_scope = ancestor_id.to_scope_id(db, file);
        let Some(place) = place_table(db, ancestor_scope).symbol_by_name(name) else {
            continue;
        };
        if !(place.is_bound() || place.is_declared()) {
            continue;
        }
        // the first scope that gives the name a value decides it, exactly as a
        // name load would. a scope that only *binds* it holds some other value,
        // which shadows an outer receiver callable rather than deferring to it
        if !place.is_declared() {
            return None;
        }
        let declared = symbol(
            db,
            ancestor_scope,
            name,
            ConsideredDefinitions::AllReachable,
        )
        .place
        .ignore_possibly_undefined()?;
        return bind_receiver(db, declared, receiver_ty);
    }
    None
}

/// basedpython: the receiver member a bare `name` in a trailing lambda block
/// resolves to, when the block's callback declares a receiver. `None` for a name
/// that resolves anywhere else — the receiver's members are the last fallback, so
/// no existing binding is ever captured
pub(crate) fn implicit_receiver_member<'db>(
    db: &'db dyn Db,
    file: File,
    scope: ScopeId<'db>,
    name: &str,
) -> Option<Type<'db>> {
    let receiver = trailing_lambda_scope_receiver(db, file, scope)?;
    if resolves_elsewhere(db, file, scope, name) {
        return None;
    }
    receiver.member(db, name).place.ignore_possibly_undefined()
}

/// the receiver of the trailing lambda block `scope` is the body of. Walks out
/// through comprehension scopes (which the block's body may open) but stops at the
/// first function, class, or module scope: a nested definition is its own body,
/// not the block's
fn trailing_lambda_scope_receiver<'db>(
    db: &'db dyn Db,
    file: File,
    scope: ScopeId<'db>,
) -> Option<Type<'db>> {
    let index = semantic_index(db, file);
    let module = parsed_module(db, file).load(db);
    for (_, ancestor) in index.visible_ancestor_scopes(scope.file_scope_id(db)) {
        match ancestor.kind() {
            ScopeKind::Comprehension => continue,
            ScopeKind::Function => {}
            _ => return None,
        }
        let function = ancestor.node().as_function()?.node(&module);
        if !function.is_trailing_lambda {
            return None;
        }
        let callee = function.trailing_lambda_callee()?;
        let expression = index.try_expression(callee)?;
        let callee_ty = infer_expression_types(db, expression, TypeContext::default())
            .try_expression_type(callee)?;
        return crate::types::trailing_lambda::trailing_lambda_receiver_type(db, callee_ty);
    }
    None
}

/// whether `name` resolves to anything other than a receiver member at `scope`.
///
/// This mirrors the fallback chain of an ordinary name load, deliberately erring
/// towards "yes": a name this misses would resolve as something else while the
/// transpiler — which re-derives the rewrite from this same answer — lowered it
/// as a receiver member. The scope walk follows python's own rules (class scopes
/// are not visible from a nested scope), matching the free-variable walk of a
/// name load.
fn resolves_elsewhere(db: &dyn Db, file: File, scope: ScopeId<'_>, name: &str) -> bool {
    let index = semantic_index(db, file);
    for (ancestor_id, _) in index.visible_ancestor_scopes(scope.file_scope_id(db)) {
        let ancestor_scope = ancestor_id.to_scope_id(db, file);
        if place_table(db, ancestor_scope)
            .symbol_by_name(name)
            .is_some_and(|symbol| symbol.is_bound() || symbol.is_declared())
        {
            return true;
        }
    }
    !builtins_symbol(db, name).place.is_undefined()
        || !module_type_implicit_global_symbol(db, file, name)
            .place
            .is_undefined()
        || is_basedpython_implicit_typing_name(name)
        // the implicit basedpython names that have no stub to resolve through
        || matches!(name, "Character" | "Some")
}
