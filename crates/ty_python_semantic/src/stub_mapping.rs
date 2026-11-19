//! a stub and the runtime module it describes, paired declaration by declaration
//!
//! a stub (`.pyi`, `.byi`) says what a module (`.py`, `.by`) declares, and module resolution is
//! what pairs them: the stub is the file a module name resolves to for typing, the runtime module
//! the file it resolves to with stubs left out. either kind of stub pairs with either kind of
//! source, since resolution reads the stub first whatever language its module is written in.
//!
//! the declarations are paired by where they sit among the module's members, not by what they
//! are: `A.f` in the stub is `A.f` in the runtime module. a name can have several declarations on
//! either side, and all of them pair:
//!
//! ```python
//! # stub                                 # runtime
//! @overload
//! def f(x: int) -> int: ...              def f(x):
//! @overload                                  return x
//! def f(x: str) -> str: ...
//!
//! if sys.version_info >= (3, 12):        class A:
//!     class A:                               def __init__(self):
//!         x: int                                 self.x = 1
//! else:
//!     class A:
//!         x: str
//! ```
//!
//! a class member is the class body's own binding of the name, or, for a class at runtime, an
//! attribute its methods assign through `self`, which is where a runtime class usually binds what
//! its stub declares in the class body. a module member is any binding of the name at the top of
//! the module, an import included; in a stub only an import that re-exports the name counts, as
//! `from m import x as x` or a name `__all__` lists, since a stub's other imports are what it reads
//! rather than what it declares.

use ruff_db::files::vendored_path_to_file;
use ruff_db::parsed::parsed_module;
use ruff_db::system::SystemPath;
use ruff_db::vendored::VendoredPathBuf;
use ruff_python_ast::name::Name;
use ruff_text_size::Ranged as _;
use tracing::trace;
use ty_module_resolver::{
    ImportingFile, resolve_module, resolve_real_module, runtime_file_to_module,
    stub_file_to_real_module,
};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::place::ScopedPlaceId;
use ty_python_core::scope::{FileScopeId, NodeWithScopeKind, NodeWithScopeRef, ScopeKind};
use ty_python_core::{ProgramFile, attribute_scopes, semantic_index};

use crate::dunder_all::dunder_all_names;
use crate::reachability::is_range_reachable;
use crate::types::definition_resolution::{
    ImportAliasResolution, ResolvedDefinition, resolve_definition,
};
use crate::{Db, FxIndexMap, ProgramEnvironment};

/// which side of a stub pairing a file is on
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StubRole {
    /// the file is a stub, describing a runtime module
    Stub,
    /// the file is a runtime module, described by a stub
    Implementation,
}

/// a file and the file it pairs with
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StubCounterpart<'db> {
    /// which of the two the file asked about is
    pub role: StubRole,
    pub stub: ProgramFile<'db>,
    pub runtime: ProgramFile<'db>,
}

impl<'db> StubCounterpart<'db> {
    /// the file on the other side from the one asked about
    pub fn counterpart(self) -> ProgramFile<'db> {
        match self.role {
            StubRole::Stub => self.runtime,
            StubRole::Implementation => self.stub,
        }
    }
}

/// the stub `file` is described by, or the runtime module it describes
///
/// `None` for a file that is neither: a module with no stub, a stub of a module with no python
/// source (a builtin, an extension module), or a file no module name reaches.
///
/// `cached_vendored_typeshed` is where the vendored stubs are written on disk for an editor to open
/// them: a stub there is the vendored one, and pairs as it does (see [`map_stub_definition`]).
pub fn stub_counterpart<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    cached_vendored_typeshed: Option<&SystemPath>,
) -> Option<StubCounterpart<'db>> {
    let source_type = file.file(db).source_type(db);
    if source_type.is_stub() {
        let runtime = stub_runtime_module(db, file, cached_vendored_typeshed)?;
        Some(StubCounterpart {
            role: StubRole::Stub,
            stub: file,
            runtime,
        })
    } else if source_type.is_py_file() {
        let stub = runtime_module_stub(db, file)?;
        Some(StubCounterpart {
            role: StubRole::Implementation,
            stub,
            runtime: file,
        })
    } else {
        None
    }
}

/// the stub that describes the runtime module `file`, when it has one
///
/// the stub is what the module's name resolves to for typing, as long as resolving it with stubs
/// left out comes back to `file`: a `.py` that is not the module python imports by its name (a
/// `foo.py` beside a `foo/__init__.py`, or the generated `.py` beside the `.by` a `by.typed`
/// package declares its module) is described by no stub.
#[salsa::tracked(returns(copy), heap_size = ruff_memory_usage::heap_size)]
pub fn runtime_module_stub<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> Option<ProgramFile<'db>> {
    let source_file = file.file(db);
    if !source_file.source_type(db).is_py_file() {
        return None;
    }
    let resolver_file = file.resolver_file(db);
    let module = runtime_file_to_module(db, resolver_file)?;
    let importing = ImportingFile::ResolverFile(resolver_file);
    let stub = resolve_module(db, importing, module.name(db))?.file(db)?;
    if !stub.is_stub(db) {
        return None;
    }
    let real = resolve_real_module(db, importing, module.name(db))?.file(db)?;
    if real != source_file {
        trace!(
            "{} is not the runtime module {} names",
            source_file.path(db),
            module.name(db)
        );
        return None;
    }
    Some(ProgramFile::new(db, stub, file.program(db)))
}

/// the runtime module the stub `stub` describes, when it has python source
fn stub_runtime_module<'db>(
    db: &'db dyn Db,
    stub: ProgramFile<'db>,
    cached_vendored_typeshed: Option<&SystemPath>,
) -> Option<ProgramFile<'db>> {
    let stub_file = stub.file(db);
    if !stub_file.is_stub(db) {
        return None;
    }

    // We write vendored typeshed stubs to disk in the cache, and consequently "forget"
    // that they're typeshed when an IDE hands those paths back to us later. For most
    // purposes this seemingly doesn't matter at all, and avoids issues with someone
    // editing the cache by hand in their IDE and us getting confused about the contents
    // of the file (hello and welcome to anyone who has found Bigger Issues this causes).
    //
    // The major exception is in exactly stub-mapping, where we need to "remember" that
    // we're in typeshed to successfully stub-map to the Real Stdlib. So here we attempt
    // to do just that. The resulting file must not be used for anything other than
    // this module lookup, as the declarations being paired are read from `stub` itself.
    let mut stub_for_module_lookup = stub;
    if let Some(vendored_typeshed) = cached_vendored_typeshed
        && let Some(stub_path) = stub_file.path(db).as_system_path()
        && let Ok(rel_path) = stub_path.strip_prefix(vendored_typeshed)
        && let Ok(typeshed_file) =
            vendored_path_to_file(db, VendoredPathBuf::from(rel_path.as_str()))
    {
        trace!(
            "Stub is cached vendored typeshed: {}",
            typeshed_file.path(db)
        );
        stub_for_module_lookup = ProgramFile::new(db, typeshed_file, stub.program(db));
    }

    // It's definitely a stub, so now rerun module resolution but with stubs disabled.
    let real_module = stub_file_to_real_module(db, stub_for_module_lookup.resolver_file(db))?;
    trace!("Found real module: {}", real_module.name(db));
    let real_file = real_module.file(db)?;
    // an extension module is a runtime module with nothing to read declarations from
    if !real_file.source_type(db).is_py_file() {
        return None;
    }
    trace!("Found real file: {}", real_file.path(db));
    Some(ProgramFile::new(db, real_file, stub.program(db)))
}

/// a name declared by a stub, with its declarations on both sides
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub struct PairedMember<'db> {
    pub name: Name,
    /// the stub's declarations of the name, in source order: every overload, every branch
    pub stub: Box<[Definition<'db>]>,
    /// the runtime module's declarations of the name, in source order; empty for a member the
    /// stub declares and the runtime module does not
    pub runtime: Box<[Definition<'db>]>,
    /// for a class on both sides, the members of the stub's class, paired the same way
    pub members: Box<[PairedMember<'db>]>,
}

impl PairedMember<'_> {
    /// whether the runtime module declares the member too
    pub fn is_paired(&self) -> bool {
        !self.runtime.is_empty()
    }
}

/// every member a stub declares, paired with the runtime module's declarations of it, in the
/// stub's order. a member only the runtime module declares is left out: there is nothing to pair
/// it with
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
pub fn stub_pairing<'db>(
    db: &'db dyn Db,
    stub: ProgramFile<'db>,
    runtime: ProgramFile<'db>,
) -> Box<[PairedMember<'db>]> {
    let stub_side = Side::new(db, stub, true);
    let runtime_side = Side::new(db, runtime, false);
    pair_scopes(
        db,
        &stub_side,
        &[FileScopeId::global()],
        &runtime_side,
        &[FileScopeId::global()],
        false,
    )
}

/// one file of a pairing, with what reading its members needs
struct Side<'db> {
    file: ProgramFile<'db>,
    is_stub: bool,
    /// the names a stub's `__all__` lists, which it re-exports whatever its imports look like
    dunder_all: Option<&'db rustc_hash::FxHashSet<Name>>,
}

impl<'db> Side<'db> {
    fn new(db: &'db dyn Db, file: ProgramFile<'db>, is_stub: bool) -> Self {
        Self {
            file,
            is_stub,
            dunder_all: is_stub.then(|| dunder_all_names(db, file)).flatten(),
        }
    }

    /// whether `definition`, a binding at the top of a module or a class body, declares a member
    ///
    /// a loop variable or a `with` target binds a name without declaring anything a stub would,
    /// and an import in a stub is a member only when it re-exports what it imports
    fn declares_member(&self, db: &'db dyn Db, definition: Definition<'db>, name: &Name) -> bool {
        match definition.kind(db) {
            DefinitionKind::Function(_)
            | DefinitionKind::Class(_)
            | DefinitionKind::TypeAlias(_)
            | DefinitionKind::Assignment(_)
            | DefinitionKind::AnnotatedAssignment(_) => true,
            DefinitionKind::Import(_)
            | DefinitionKind::ImportFrom(_)
            | DefinitionKind::StarImport(_) => {
                !self.is_stub
                    || definition.is_reexported(db)
                    || self.dunder_all.is_some_and(|names| names.contains(name))
            }
            // `from ._impl import f` in a package's `__init__` binds `_impl` too, as the
            // submodule's import sets it on the package, but nothing declares it
            DefinitionKind::ImportFromSubmodule(_)
            | DefinitionKind::NamedExpression(_)
            | DefinitionKind::StatementExpressionValue(_)
            | DefinitionKind::AugmentedAssignment(_)
            | DefinitionKind::DictKeyAssignment(_)
            | DefinitionKind::For(_)
            | DefinitionKind::Comprehension(_)
            | DefinitionKind::Parameter(_)
            | DefinitionKind::LambdaParameter(_)
            | DefinitionKind::WithItem(_)
            | DefinitionKind::MatchPattern(_)
            | DefinitionKind::ExceptHandler(_)
            | DefinitionKind::TypeVar(_)
            | DefinitionKind::ParamSpec(_)
            | DefinitionKind::TypeVarTuple(_)
            | DefinitionKind::TypeMatchCapture(_)
            | DefinitionKind::LoopHeader(_)
            | DefinitionKind::NestedBindings(_) => false,
        }
    }

    /// the members `scopes` declare, by name, in the order each name is first declared
    ///
    /// `scopes` are one class's bodies, or the module's, more than one where the same class is
    /// declared in several branches. a class body's members include the attributes its methods
    /// assign through `self`, after the body's own
    fn members(
        &self,
        db: &'db dyn Db,
        scopes: &[FileScopeId],
        is_class: bool,
    ) -> FxIndexMap<Name, Vec<Definition<'db>>> {
        let index = semantic_index(db, self.file);
        let mut members: FxIndexMap<Name, Vec<Definition<'db>>> = FxIndexMap::default();
        for &scope in scopes {
            let table = index.place_table(scope);
            for (_, definition, _) in index.use_def_map(scope).definitions_with_usage() {
                let ScopedPlaceId::Symbol(symbol) = definition.place(db) else {
                    continue;
                };
                let name = table.symbol(symbol).name();
                if self.declares_member(db, definition, name) {
                    members.entry(name.clone()).or_default().push(definition);
                }
            }
        }
        if is_class {
            for &scope in scopes {
                for method_scope in attribute_scopes(db, scope.to_scope_id(db, self.file)) {
                    let table = index.place_table(method_scope);
                    for (_, definition, _) in
                        index.use_def_map(method_scope).definitions_with_usage()
                    {
                        let ScopedPlaceId::Member(member) = definition.place(db) else {
                            continue;
                        };
                        let Some(name) = table.member(member).as_instance_attribute() else {
                            continue;
                        };
                        if matches!(
                            definition.kind(db),
                            DefinitionKind::Assignment(_) | DefinitionKind::AnnotatedAssignment(_)
                        ) {
                            members.entry(Name::new(name)).or_default().push(definition);
                        }
                    }
                }
            }
        }
        members
    }

    /// the bodies of the classes among `definitions`
    fn class_bodies(&self, db: &'db dyn Db, definitions: &[Definition<'db>]) -> Vec<FileScopeId> {
        let index = semantic_index(db, self.file);
        let module = parsed_module(db, self.file.python_file(db)).load(db);
        definitions
            .iter()
            .filter_map(|definition| match definition.kind(db) {
                DefinitionKind::Class(class) => {
                    index.try_node_scope(NodeWithScopeRef::Class(class.node(&module)))
                }
                _ => None,
            })
            .collect()
    }
}

fn pair_scopes<'db>(
    db: &'db dyn Db,
    stub: &Side<'db>,
    stub_scopes: &[FileScopeId],
    runtime: &Side<'db>,
    runtime_scopes: &[FileScopeId],
    is_class: bool,
) -> Box<[PairedMember<'db>]> {
    let runtime_members = runtime.members(db, runtime_scopes, is_class);
    stub.members(db, stub_scopes, is_class)
        .into_iter()
        .map(|(name, stub_definitions)| {
            let runtime_definitions = runtime_members.get(&name).cloned().unwrap_or_default();
            let stub_classes = stub.class_bodies(db, &stub_definitions);
            let runtime_classes = runtime.class_bodies(db, &runtime_definitions);
            let members = if stub_classes.is_empty() || runtime_classes.is_empty() {
                Box::default()
            } else {
                pair_scopes(db, stub, &stub_classes, runtime, &runtime_classes, true)
            };
            PairedMember {
                name,
                stub: stub_definitions.into_boxed_slice(),
                runtime: runtime_definitions.into_boxed_slice(),
                members,
            }
        })
        .collect()
}

/// where `definition` sits among its module's members: `[A, f]` for a method `f` of a class `A`
/// at the top of the module, and `[A, x]` for an attribute `self.x` a method of `A` assigns.
/// `None` for a binding that is no member: a function's local, a parameter
fn member_path(db: &dyn Db, definition: Definition<'_>) -> Option<Vec<Name>> {
    let file = definition.program_file(db);
    let index = semantic_index(db, file);
    let module = parsed_module(db, file.python_file(db)).load(db);
    let mut scope = definition.file_scope(db);
    let table = index.place_table(scope);
    let mut path = match definition.place(db) {
        ScopedPlaceId::Symbol(symbol) => vec![table.symbol(symbol).name().clone()],
        ScopedPlaceId::Member(member) => {
            let name = Name::new(table.member(member).as_instance_attribute()?);
            // an attribute is assigned in a method, or in a comprehension inside one; the class
            // the method is in is what the attribute is a member of
            loop {
                let current = index.scope(scope);
                let parent = current.parent()?;
                if current.kind() == ScopeKind::Function {
                    scope = parent;
                    break;
                }
                scope = parent;
            }
            if index.scope(scope).kind() == ScopeKind::TypeParams {
                scope = index.scope(scope).parent()?;
            }
            if index.scope(scope).kind() != ScopeKind::Class {
                return None;
            }
            vec![name]
        }
    };
    loop {
        let current = index.scope(scope);
        match current.node() {
            NodeWithScopeKind::Module => break,
            NodeWithScopeKind::Class(class) => {
                path.push(class.node(&module).name.id.clone());
            }
            // a class with type parameters has a scope for them around its body
            NodeWithScopeKind::ClassTypeParameters(_) => {}
            _ => return None,
        }
        scope = current.parent()?;
    }
    path.reverse();
    Some(path)
}

/// the declarations of the member at `path` in `file`, read as `side` reads its members
fn definitions_at_path<'db>(
    db: &'db dyn Db,
    side: &Side<'db>,
    path: &[Name],
) -> Vec<Definition<'db>> {
    let Some((last, classes)) = path.split_last() else {
        return Vec::new();
    };
    let mut scopes = vec![FileScopeId::global()];
    let mut is_class = false;
    for class in classes {
        let Some(definitions) = side.members(db, &scopes, is_class).swap_remove(class) else {
            return Vec::new();
        };
        scopes = side.class_bodies(db, &definitions);
        if scopes.is_empty() {
            return Vec::new();
        }
        is_class = true;
    }
    side.members(db, &scopes, is_class)
        .swap_remove(last)
        .unwrap_or_default()
}

/// the stub's declarations of the member `definition`, a declaration in a runtime module, declares
///
/// `None` where the module has no stub, the definition is no member, or the stub does not declare
/// it. the declarations are as written, not resolved through imports: a stub that re-exports the
/// member answers with its import
pub fn stub_definitions<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Option<Vec<Definition<'db>>> {
    let stub = runtime_module_stub(db, definition.program_file(db))?;
    let path = member_path(db, definition)?;
    let definitions = definitions_at_path(db, &Side::new(db, stub, true), &path);
    (!definitions.is_empty()).then_some(definitions)
}

/// the runtime module's declarations of the member `definition`, a declaration in a stub,
/// declares, as written, not resolved through imports
///
/// `None` where the stub describes no runtime module with python source, the definition is no
/// member, or the runtime module does not declare it
pub fn runtime_definitions<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    cached_vendored_typeshed: Option<&SystemPath>,
) -> Option<Vec<Definition<'db>>> {
    let runtime = stub_runtime_module(db, definition.program_file(db), cached_vendored_typeshed)?;
    let path = member_path(db, definition)?;
    trace!("Built member path: {path:?}");
    let definitions = definitions_at_path(db, &Side::new(db, runtime, false), &path);
    (!definitions.is_empty()).then_some(definitions)
}

/// Given a definition that may be in a stub file, find the "real" definition in a non-stub.
///
/// What the runtime module binds the member to is resolved through its imports, so a stub's `f`
/// leads to the `def f` a runtime `from ._impl import f` brings in.
#[tracing::instrument(skip_all)]
pub fn map_stub_definition<'db>(
    db: &'db dyn Db,
    def: &ResolvedDefinition<'db>,
    cached_vendored_typeshed: Option<&SystemPath>,
) -> Option<Vec<ResolvedDefinition<'db>>> {
    let Some(stub_program_file) = def.program_file(db) else {
        trace!("Found arbitrary FileWithRange while stub mapping, giving up");
        return None;
    };
    trace!(
        "Stub mapping definition in: {}",
        stub_program_file.file(db).path(db)
    );

    match *def {
        ResolvedDefinition::Definition(definition) => {
            let definitions = runtime_definitions(db, definition, cached_vendored_typeshed)?;
            let resolved: Vec<_> = definitions
                .into_iter()
                .flat_map(|definition| resolve_member_definition(db, definition))
                .collect();
            trace!("Found {} definitions from stub mapping", resolved.len());
            (!resolved.is_empty()).then_some(resolved)
        }
        ResolvedDefinition::Module(_) => {
            let real = stub_runtime_module(db, stub_program_file, cached_vendored_typeshed)?;
            Some(vec![ResolvedDefinition::Module(real)])
        }
        ResolvedDefinition::FileWithRange(_) => {
            // Not yet implemented -- in this case we want to recover something like a Definition
            // and build a member path, but this input is a bit too abstract for now.
            trace!("Found arbitrary FileWithRange while stub mapping, giving up");
            None
        }
    }
}

/// the name `definition` binds, as resolving an import of it needs
fn definition_name(db: &dyn Db, definition: Definition<'_>) -> Option<Name> {
    let file = definition.program_file(db);
    let table = semantic_index(db, file).place_table(definition.file_scope(db));
    match definition.place(db) {
        ScopedPlaceId::Symbol(symbol) => Some(table.symbol(symbol).name().clone()),
        ScopedPlaceId::Member(member) => {
            table.member(member).as_instance_attribute().map(Name::new)
        }
    }
}

/// what a member's declaration binds the name to, followed through imports: the `def f` a
/// `from ._impl import f` brings in, and the declaration itself where it is no import
pub fn resolve_member_definition<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Vec<ResolvedDefinition<'db>> {
    let env = ProgramEnvironment::from_file(definition.program_file(db));
    let name = definition_name(db, definition);
    resolve_definition(
        db,
        &env,
        definition,
        name.as_deref(),
        ImportAliasResolution::ResolveAliases,
    )
}

/// whether `definition` is in code that runs for the configured python version and platform: of
/// a stub's `if sys.version_info >= (3, 12):` branches, the one taken
pub fn is_definition_reachable(db: &dyn Db, definition: Definition<'_>) -> bool {
    let file = definition.program_file(db);
    let module = parsed_module(db, file.python_file(db)).load(db);
    is_range_reachable(
        db,
        semantic_index(db, file),
        definition.file_scope(db),
        definition.full_range(db, &module).range(),
    )
}
