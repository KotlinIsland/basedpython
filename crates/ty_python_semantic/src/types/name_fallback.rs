//! the shared gate for "does anything already claim this name"
//!
//! three basedpython rules give an otherwise-unresolvable bare name a meaning,
//! each as the *last* fallback so nothing that resolves today changes meaning:
//!
//! - [context-sensitive resolution](crate::types::context_sensitive): an enum
//!   member unqualified where the expected type is known (`a: Color = Red`)
//! - [implicit receivers](crate::types::receivers): a trailing lambda block
//!   seeing its receiver's members unqualified
//! - [django lookup expressions](crate::types::dedicated::django): the root of
//!   `filter(author.name == "x")`
//!
//! all three ask the same question of the lexical scope chain, and this module
//! owns the one walk that answers it. two of them need a *wider* answer, and the
//! difference is load-bearing rather than accidental: it is whether the caller
//! sits downstream of ty's own name-resolution fallback chain.
//!
//! context-sensitive resolution is reached only from the very end of that chain,
//! so every name ty resolves without a binding — a basedpython implicit `typing`
//! name, `Character`, `Some` — has already claimed itself before the enum
//! fallback runs, and [`claimed_by_lexical_scope`] is enough. the receiver and
//! django rules are additionally asked *by the transpiler*, about a raw name with
//! no chain behind it, so they have to replicate those entries themselves:
//! [`claimed_by_name_resolution`].
//!
//! the two are not interchangeable. `Character` outside a type expression is the
//! witness: no earlier fallback claims it, so an enum member of that name
//! resolves context-sensitively today, and widening the enum gate to
//! [`claimed_by_name_resolution`] would silently take that away

use ruff_db::files::File;
use ty_python_core::scope::ScopeId;
use ty_python_core::{place_table, semantic_index};

use crate::Db;
use crate::place::{
    builtins_symbol, is_basedpython_implicit_typing_name, module_type_implicit_global_symbol,
};

/// whether an ordinary lexical lookup owns `name`: a binding or a declaration
/// anywhere in the visible scope chain, or a builtin
///
/// deliberately coarser than the flow-sensitive lookup that reaches the
/// fallbacks. the transpiler has no flow information — it qualifies a name that
/// no scope binds at all — so a scope that binds the name *anywhere* takes it
/// back everywhere, or `a: Color = Red` followed by `Red = 1` would check clean
/// and `NameError` at runtime.
///
/// the walk follows python's own rules (a class scope is not visible from a
/// nested scope), matching the free-variable walk of a name load
pub(crate) fn claimed_by_lexical_scope(
    db: &dyn Db,
    file: File,
    scope: ScopeId<'_>,
    name: &str,
) -> bool {
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
}

/// [`claimed_by_lexical_scope`], and additionally every name ty resolves with no
/// binding behind it: an implicit module global, a basedpython implicit `typing`
/// name, and the two implicit names that have no stub to resolve through.
///
/// for the callers the name-resolution fallback chain has *not* already run for,
/// so it errs towards "yes": a name this missed would resolve as something else
/// while the transpiler — which re-derives the rewrite from this same answer —
/// lowered it as a receiver member or a lookup path
pub(crate) fn claimed_by_name_resolution(
    db: &dyn Db,
    file: File,
    scope: ScopeId<'_>,
    name: &str,
) -> bool {
    claimed_by_lexical_scope(db, file, scope, name)
        // states the intent directly rather than leaning on the fact that the
        // builtins lookup above happens to fall back to `types.ModuleType` too
        || !module_type_implicit_global_symbol(db, file, name)
            .place
            .is_undefined()
        || is_basedpython_implicit_typing_name(name)
        // the implicit basedpython names that have no stub to resolve through
        || matches!(name, "Character" | "Some")
}

#[cfg(test)]
mod tests {
    use ruff_db::files::system_path_to_file;
    use ruff_db::system::DbWithWritableSystem as _;
    use ty_python_core::global_scope;

    use crate::db::tests::setup_db;

    use super::{claimed_by_lexical_scope, claimed_by_name_resolution};

    /// the two gates must disagree on *exactly* the implicitly-available names, so
    /// that a future edit cannot quietly collapse them into one
    #[test]
    fn the_gates_differ_only_on_implicitly_available_names() {
        let mut db = setup_db();
        db.write_file("/src/a.by", "x = 1\n").unwrap();
        let file = system_path_to_file(&db, "/src/a.by").unwrap();
        let scope = global_scope(&db, file);

        // a binding and a builtin: both gates own these
        for name in ["x", "int"] {
            assert!(claimed_by_lexical_scope(&db, file, scope, name), "{name}");
            assert!(claimed_by_name_resolution(&db, file, scope, name), "{name}");
        }

        // a name nothing claims: both gates leave it for the fallbacks
        for name in ["nonesuch", "Red"] {
            assert!(!claimed_by_lexical_scope(&db, file, scope, name), "{name}");
            assert!(
                !claimed_by_name_resolution(&db, file, scope, name),
                "{name}"
            );
        }

        // an implicit module global is *not* part of the difference: the shared
        // builtins lookup already falls back to `types.ModuleType`, so the wider
        // gate's own module-global check only ever repeats an answer it has
        for name in ["__name__", "__spec__", "__debug__", "__file__"] {
            assert!(claimed_by_lexical_scope(&db, file, scope, name), "{name}");
        }

        // the whole of the difference: the basedpython implicit `typing` names, and
        // the two implicit names that have no stub to resolve through
        for name in ["Optional", "Self", "Sequence", "Character", "Some"] {
            assert!(!claimed_by_lexical_scope(&db, file, scope, name), "{name}");
            assert!(claimed_by_name_resolution(&db, file, scope, name), "{name}");
        }
    }
}
