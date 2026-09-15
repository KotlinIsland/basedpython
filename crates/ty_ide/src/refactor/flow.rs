//! Which bindings a name reads, from the semantic index's use-def map.

use ruff_db::parsed::ParsedModuleRef;
use ruff_python_ast as ast;
use rustc_hash::FxHashSet;
use ty_project::Db;
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::place::ScopedPlaceId;
use ty_python_core::scope::FileScopeId;
use ty_python_core::symbol::ScopedSymbolId;
use ty_python_core::{DefinitionState, SemanticIndex};

/// The bindings a load of a name may read.
///
/// Reachability is ignored, so this over-approximates: a binding that can never
/// reach the load at runtime is still listed. Every caller asks "is this load
/// guaranteed to read only these bindings?", where listing too much only makes
/// the answer more cautious.
#[derive(Debug, Default)]
pub(crate) struct Reaching<'db> {
    pub(crate) definitions: Vec<Definition<'db>>,
    /// Whether the load may find the name unbound or deleted.
    pub(crate) maybe_unbound: bool,
}

impl<'db> Reaching<'db> {
    /// Whether the load reads `definition` and nothing else.
    pub(crate) fn is_exactly(&self, definition: Definition<'db>) -> bool {
        !self.maybe_unbound
            && self
                .definitions
                .iter()
                .all(|reached| *reached == definition)
            && !self.definitions.is_empty()
    }
}

/// The bindings the load `name`, in `scope`, may read from that scope.
///
/// A name that is not local to `scope` reads nothing from it, which is reported
/// as possibly unbound: the caller has to look in the scope the name resolves to.
pub(crate) fn reaching_definitions<'db>(
    db: &'db dyn Db,
    index: &SemanticIndex<'db>,
    scope: FileScopeId,
    name: &ast::ExprName,
) -> Reaching<'db> {
    let Some(use_id) = index.try_use_id(ast::ExprRef::Name(name)) else {
        return Reaching {
            definitions: Vec::new(),
            maybe_unbound: true,
        };
    };
    let use_def = index.use_def_map(scope);
    let mut reaching = Reaching::default();
    let mut seen = FxHashSet::default();
    let mut pending: Vec<DefinitionState<'db>> = use_def
        .bindings_at_use(use_id)
        .map(|binding| binding.binding)
        .collect();

    while let Some(state) = pending.pop() {
        match state {
            DefinitionState::Undefined | DefinitionState::Deleted => reaching.maybe_unbound = true,
            DefinitionState::Defined(definition) => {
                if !seen.insert(definition) {
                    continue;
                }
                // a loop header stands for the bindings that flow back around the
                // loop into its start
                if let DefinitionKind::LoopHeader(header) = definition.kind(db) {
                    pending.extend(
                        use_def
                            .loop_header(header.loop_header_id())
                            .bindings_for_place(header.place())
                            .map(|live| use_def.definition(live.binding())),
                    );
                } else {
                    reaching.definitions.push(definition);
                }
            }
        }
    }
    reaching
}

/// Every definition of `symbol` in `scope` that binds a value, including the
/// synthetic ones that stand for a nested scope's `global` or `nonlocal` write.
pub(crate) fn bindings_of_symbol<'db>(
    db: &'db dyn Db,
    index: &SemanticIndex<'db>,
    parsed: &ParsedModuleRef,
    scope: FileScopeId,
    symbol: ScopedSymbolId,
    in_stub: bool,
) -> Vec<Definition<'db>> {
    index
        .use_def_map(scope)
        .definitions_with_usage()
        .map(|(_, definition, _)| definition)
        .filter(|definition| definition.place(db) == ScopedPlaceId::Symbol(symbol))
        .filter(|definition| {
            let kind = definition.kind(db);
            !matches!(kind, DefinitionKind::LoopHeader(_))
                && kind.category(in_stub, parsed).is_binding()
        })
        .collect()
}

/// The scope a load of `name` in `scope` resolves to, following python's rules:
/// a class body is invisible to the scopes nested in it, `global` goes to the
/// module and `nonlocal` to an enclosing function. `None` for a name no scope in
/// the file binds, such as a builtin.
pub(crate) fn resolving_scope(
    index: &SemanticIndex<'_>,
    scope: FileScopeId,
    name: &str,
) -> Option<FileScopeId> {
    for (scope_id, _) in index.visible_ancestor_scopes(scope) {
        let table = index.place_table(scope_id);
        let Some(symbol_id) = table.symbol_id(name) else {
            continue;
        };
        let symbol = table.symbol(symbol_id);
        if symbol.is_global() {
            return Some(FileScopeId::global());
        }
        if symbol.is_nonlocal() {
            continue;
        }
        if symbol.is_local() {
            return Some(scope_id);
        }
    }
    None
}

/// Whether any scope nested in `scope` declares `name` `global` or `nonlocal`
/// and so may rebind the `name` that `scope` owns.
///
/// A `nonlocal` is counted even when it names an intermediate function's binding
/// rather than `scope`'s, which only makes callers more cautious.
pub(crate) fn rebound_from_nested_scope(
    index: &SemanticIndex<'_>,
    scope: FileScopeId,
    name: &str,
) -> bool {
    descendants(index, scope).any(|nested| {
        let table = index.place_table(nested);
        table.symbol_id(name).is_some_and(|symbol_id| {
            let symbol = table.symbol(symbol_id);
            symbol.is_bound() && ((symbol.is_global() && scope.is_global()) || symbol.is_nonlocal())
        })
    })
}

/// The scopes nested, at any depth, inside `scope`.
fn descendants<'a>(
    index: &'a SemanticIndex<'_>,
    scope: FileScopeId,
) -> impl Iterator<Item = FileScopeId> + 'a {
    let mut stack: Vec<FileScopeId> = index.child_scopes(scope).map(|(id, _)| id).collect();
    std::iter::from_fn(move || {
        let next = stack.pop()?;
        stack.extend(index.child_scopes(next).map(|(id, _)| id));
        Some(next)
    })
}
