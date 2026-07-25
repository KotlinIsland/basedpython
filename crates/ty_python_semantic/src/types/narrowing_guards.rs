//! basedpython: resolving the place a narrowing return annotation names, at a call site.
//!
//! `def check(x: int | None) -> asserts x` names a parameter, so the place a call narrows is
//! whatever it passed for that parameter. `def ensure(self) -> asserts self.data is not None`
//! names a member of the receiver, so `h.ensure()` narrows `h.data`. A name that isn't a
//! parameter at all names a place in the calling scope.

use ruff_python_ast as ast;
use ruff_python_ast::name::Name;
use ty_python_core::place::{PlaceExpr, ScopedPlaceId};
use ty_python_core::place_table;
use ty_python_core::scope::ScopeId;

use crate::Db;
use crate::types::signatures::{NarrowingGuard, Parameters};

/// What a guard's root name refers to at a call site.
pub(crate) enum GuardRoot<'ast> {
    /// The parameter at this index of the callee's parameters, whose argument the caller
    /// has to find.
    Parameter(usize),
    /// The receiver of a bound call, which is not among the call's arguments.
    Receiver(&'ast ast::Expr),
    /// Not a parameter of the callee: a place of that name in the calling scope.
    Scope,
}

/// Resolve what `guard`'s root name refers to for this call.
pub(crate) fn guard_root<'ast, 'db>(
    guard: &NarrowingGuard<'db>,
    parameters: &Parameters<'db>,
    call: &'ast ast::ExprCall,
) -> GuardRoot<'ast> {
    if let Some(index) = parameters
        .iter()
        .position(|parameter| parameter.name() == Some(&guard.name))
    {
        return GuardRoot::Parameter(index);
    }

    // A bound call consumes the first parameter as its receiver, so that parameter is gone
    // from the signature and its argument is the callee's own value: `h` in `h.ensure()`.
    if guard.root_is_first_parameter
        && let ast::Expr::Attribute(attribute) = call.func.as_ref()
    {
        return GuardRoot::Receiver(&attribute.value);
    }

    GuardRoot::Scope
}

/// The place `guard` narrows below the call-site expression its root resolved to.
pub(crate) fn narrowed_place<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    guard: &NarrowingGuard<'db>,
    root: &ast::Expr,
) -> Option<ScopedPlaceId> {
    let place = PlaceExpr::try_from_expr_with_members(root, &guard.members)?;
    place_table(db, scope).place_id(&place)
}

/// The place `guard` narrows in the calling scope, for a root that is not a parameter.
pub(crate) fn narrowed_scope_place<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    guard: &NarrowingGuard<'db>,
) -> Option<ScopedPlaceId> {
    let place = PlaceExpr::from_symbol_with_members(&guard.name, &guard.members)?;
    place_table(db, scope).place_id(&place)
}

/// The names a guard's place is made of, root first, for diagnostics.
pub(crate) fn guard_place_display(guard: &NarrowingGuard<'_>) -> String {
    std::iter::once(&guard.name)
        .chain(&guard.members)
        .map(Name::as_str)
        .collect::<Vec<_>>()
        .join(".")
}
