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
//!   and the receiver itself is spelled `self`
//!
//! both are *last* fallbacks: a declared member, and any name something else
//! already claims, keeps its ordinary meaning. that is what makes the forms
//! purely additive — nothing that resolves today changes meaning. the block form
//! gates on [`claimed_by_name_resolution`], the wider of the two shared
//! [name-fallback](crate::types::name_fallback) gates, because the transpiler
//! asks it about a raw name with no fallback chain behind it
//!
//! [trailing lambda]: crate::types::trailing_lambda

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_python_ast::{self as ast, Expr};
use ty_python_core::scope::{ScopeId, ScopeKind};
use ty_python_core::{place_table, semantic_index};

use crate::Db;
use crate::place::{ConsideredDefinitions, symbol};
use crate::types::name_fallback::claimed_by_name_resolution;
use crate::types::signatures::{Parameters, Signature};
use crate::types::{Type, TypeContext, UnionType, infer_expression_types};

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

/// basedpython: whether `attribute` resolves through an *implicit receiver* —
/// `x.fn` where `fn` names a receiver callable (`int.() -> str`) in scope rather
/// than a member of `x`. `receiver_ty` is the type of the attribute's own value.
/// The receiver form is the last fallback, so a declared member and an
/// applicable extension member both win over it
pub(crate) fn is_implicit_receiver_attribute<'db>(
    db: &'db dyn Db,
    file: File,
    scope: ScopeId<'db>,
    attribute: &ast::ExprAttribute,
    receiver_ty: Type<'db>,
) -> bool {
    // an optional-chain link resolves against the chain's *present* type — the
    // `None` it short-circuits with is not part of the receiver
    let receiver_ty = if attribute.optional || spine_has_optional(&attribute.value) {
        strip_none(db, receiver_ty)
    } else {
        receiver_ty
    };
    let name = attribute.attr.as_str();
    if !receiver_ty.member(db, name).place.is_undefined() {
        return false;
    }
    // an extension member wins over a receiver callable, matching the order the
    // two fallbacks run in during inference. resolving again here is near-free
    // in a file with no extensions: the applicable-extension list is a cached
    // query that comes back empty
    if crate::types::extensions::resolve_extension_member(db, file, receiver_ty, name).is_some() {
        return false;
    }
    resolve_receiver_attribute(db, file, scope, receiver_ty, name).is_some()
}

/// whether any link of the attribute spine `expr` is an optional access
fn spine_has_optional(expr: &Expr) -> bool {
    match expr {
        Expr::Attribute(attribute) => attribute.optional || spine_has_optional(&attribute.value),
        Expr::Subscript(subscript) => spine_has_optional(&subscript.value),
        Expr::Call(call) => spine_has_optional(&call.func),
        _ => false,
    }
}

/// basedpython: `ty` without the `None` an optional chain unions in
fn strip_none<'db>(db: &'db dyn Db, ty: Type<'db>) -> Type<'db> {
    let Type::Union(union) = ty else {
        return ty;
    };
    UnionType::from_elements(
        db,
        union
            .elements(db)
            .iter()
            .copied()
            .filter(|element| !element.is_none(db)),
    )
}

/// basedpython: what a bare name in a trailing lambda block resolves to through
/// the block's receiver
pub(crate) enum ImplicitReceiverName<'db> {
    /// `self` — the receiver itself
    Receiver(Type<'db>),
    /// a member of the receiver, read off it in the lowering
    Member(Type<'db>),
    /// a member an applicable `extension` supplies for the receiver, lowered to
    /// its backing function rather than an attribute read
    ExtensionMember {
        ty: Type<'db>,
        resolution: crate::types::extensions::ExtensionMemberResolution<'db>,
    },
}

impl<'db> ImplicitReceiverName<'db> {
    pub(crate) fn ty(&self) -> Type<'db> {
        match self {
            Self::Receiver(ty) | Self::Member(ty) | Self::ExtensionMember { ty, .. } => *ty,
        }
    }
}

/// basedpython: what a bare `name` in a trailing lambda block resolves to when
/// the block's callback declares a receiver: `self` is the receiver, and any
/// other name is looked up as a member of it. `None` for a name that resolves
/// anywhere else — both are the last fallback, so no existing binding is ever
/// captured (a method's own `self` keeps its meaning)
pub(crate) fn implicit_receiver_name<'db>(
    db: &'db dyn Db,
    file: File,
    scope: ScopeId<'db>,
    name: &str,
) -> Option<ImplicitReceiverName<'db>> {
    let receiver = trailing_lambda_scope_receiver(db, file, scope)?;
    if claimed_by_name_resolution(db, file, scope, name) {
        return None;
    }
    if name == "self" {
        return Some(ImplicitReceiverName::Receiver(receiver));
    }
    if let Some(member) = receiver.member(db, name).place.ignore_possibly_undefined() {
        return Some(ImplicitReceiverName::Member(member));
    }
    // an extension of the receiver's type supplies members too, and the block's
    // scope is the receiver's — so `p:` inside a `div:` block reaches an
    // `extension Tag: def p` exactly as `self.p:` does. reached last, after the
    // receiver's own members, like every other extension lookup
    let resolution = crate::types::extensions::resolve_extension_member(db, file, receiver, name)?;
    Some(ImplicitReceiverName::ExtensionMember {
        ty: resolution.ty,
        resolution,
    })
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
