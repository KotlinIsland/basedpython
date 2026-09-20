//! basedpython: whether a use of an underscore name reads a name a `.by` file chose
//!
//! basedpython spells privacy with `private` and `protected`, so in a `.by` file a
//! leading underscore says only that the name is unused. [`USED_UNDERSCORE_NAME`]
//! reports a use of one. the report is only fair where the author of a `.by` file
//! picked the spelling: a name read from python or from a stub (`_asdict`, a
//! library's `_internal`, an enum's `_missing_`) is spelled by someone else, and
//! renaming it is not an option
//!
//! so each question here resolves the use to every declaration of the name, and
//! answers yes only when there is at least one and every one of them is written in
//! a `.by` source file. an import without an alias passes the spelling through to
//! what it imports, while `from m import x as _y` spells `_y` itself. a member is
//! declared by every class in the MRO that declares it: an override of a python
//! member keeps the python member's name. anything that cannot be traced — an
//! unresolved import, a dynamic base class — answers no
//!
//! [`USED_UNDERSCORE_NAME`]: super::diagnostic::USED_UNDERSCORE_NAME

use itertools::Either;
use ruff_db::files::{File, FileRange};
use ruff_db::parsed::parsed_module;
use ruff_python_ast::generated_names::GeneratedName;
use ruff_python_ast::helpers::is_dunder;
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, PySourceType};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::{FxHashMap, FxHashSet};
use ty_module_resolver::{ImportingFile, ModuleName, resolve_module};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::scope::ScopeId;
use ty_python_core::{ProgramFile, attribute_scopes, global_scope, semantic_index, use_def_map};

use crate::types::definition_resolution::{
    AttributeRead, ImportAliasResolution, ResolvedDefinition, definitions_for_name,
    fallback_attribute_definitions, find_symbol_in_scope, resolve_definition,
    resolve_from_import_definitions,
};
use crate::types::receivers::{self, ImplicitReceiverName};
use crate::types::{ClassBase, ClassLiteral, StaticClassLiteral, SubclassOfInner, Type};
use crate::{Db, ProgramEnvironment};

/// whether `name` is spelled as unused: a leading underscore, other than a dunder
/// or a name made only of underscores, which python gives meanings of their own
pub(crate) fn is_underscore_name(name: &str) -> bool {
    name.starts_with('_') && !is_dunder(name) && !name.trim_start_matches('_').is_empty()
}

/// whether the name `name`, read in `scope` of `file`, was spelled by a `.by`
/// file
///
/// the lookup takes the path inference does — inside a trailing lambda block the
/// receiver claims a name before any enclosing scope can
pub(crate) fn name_is_chosen<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    scope: ScopeId<'db>,
    name: &ast::ExprName,
) -> Option<Spelled> {
    let id = name.id.as_str();
    if let Some(resolved) = receivers::implicit_receiver_name(db, env, file, scope, id, Some(name))
    {
        return match resolved {
            ImplicitReceiverName::Member(_) => {
                let receiver = receivers::block_receiver_type(db, scope)?;
                let access = AttributeRead {
                    file,
                    scope,
                    receiver,
                    receiver_expr: None,
                    optional: false,
                };
                member_is_chosen(db, env, &access, id)
            }
            ImplicitReceiverName::ExtensionMember { resolution, .. } => {
                match underscore_members(db, resolution.extension).get(id) {
                    Some(UnderscoreMember::AuthorSpelled { declaration }) => Some(Spelled {
                        declaration: Some(*declaration),
                    }),
                    Some(UnderscoreMember::ParserSpelled) | None => None,
                }
            }
            ImplicitReceiverName::Receiver(_) => None,
        };
    }
    let resolved = definitions_for_name(db, scope, id, ImportAliasResolution::PreserveAliases);
    all_chosen(db, &resolved, id)
}

/// where a name a `.by` file spelled was declared, when the declaration can be
/// pointed at — an imported name is declared in another file, and saying where is
/// most of what a reader needs
#[derive(Clone, Copy, Debug)]
pub(crate) struct Spelled {
    pub(crate) declaration: Option<FileRange>,
}

/// whether `name`, imported by `import_from` in `importing_file`, was spelled by a
/// `.by` file
pub(crate) fn imported_name_is_chosen<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    importing_file: ImportingFile<'db>,
    import_from: &ast::StmtImportFrom,
    name: &str,
) -> Option<Spelled> {
    let resolved = resolve_from_import_definitions(
        db,
        env,
        importing_file,
        import_from,
        name,
        &mut FxHashSet::default(),
        ImportAliasResolution::PreserveAliases,
    );
    all_chosen(db, &resolved, name)
}

/// whether the module a dotted `import` names is a `.by` file that spelled the
/// segment `name` itself — `import pkg._sub` reads `_sub` from `pkg`
pub(crate) fn imported_module_is_chosen<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    importing_file: File,
    module: &ModuleName,
) -> Option<Spelled> {
    let importing = ImportingFile::File(importing_file, env.resolver_environment(db));
    let file = resolve_module(db, importing, module).and_then(|module| module.file(db))?;
    // a module is named by its file, which has nowhere in it to point at
    is_basedpython_source(db, file).then_some(Spelled { declaration: None })
}

/// whether the member `name`, read at `access`, was spelled by a `.by` file on
/// every type the receiver can be that declares it
pub(crate) fn member_is_chosen<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    access: &AttributeRead<'db, '_>,
    name: &str,
) -> Option<Spelled> {
    let elements = match access.receiver {
        Type::Union(union) => union.elements(db),
        _ => std::slice::from_ref(&access.receiver),
    };
    let mut declared: Option<Spelled> = None;
    for element in elements.iter().flat_map(|element| match element {
        Type::Intersection(intersection) => Either::Left(intersection.positive(db).iter()),
        _ => Either::Right(std::iter::once(element)),
    }) {
        match member_spelling(db, env, access.file, *element, name) {
            Spelling::Undeclared => {}
            Spelling::Chosen(spelled) => declared = declared.or(Some(spelled)),
            Spelling::Dictated => return None,
        }
    }
    if declared.is_some() {
        return declared;
    }

    // what no class declares, inference looks for in the same three places, in
    // this order — an extension member, an inline protocol's member, a receiver
    // callable — so the lint asks the resolver that answers for all three
    let resolved = fallback_attribute_definitions(db, env, access, name);
    all_chosen(db, &resolved, name)
}

/// whether every declaration in `definitions` was spelled by the author of a
/// `.by` file, and there is at least one
pub(crate) fn definitions_are_chosen<'db>(
    db: &'db dyn Db,
    definitions: impl IntoIterator<Item = Definition<'db>>,
    name: &str,
) -> Option<Spelled> {
    let mut declaration = None;
    for definition in definitions {
        if !*definition_is_author_spelled(db, definition, Name::new(name)) {
            return None;
        }
        declaration = declaration.or_else(|| Some(definition_range(db, definition)));
    }
    declaration.map(|declaration| Spelled {
        declaration: Some(declaration),
    })
}

/// where a definition names what it declares
fn definition_range<'db>(db: &'db dyn Db, definition: Definition<'db>) -> FileRange {
    let parsed = parsed_module(db, definition.python_file(db)).load(db);
    definition.focus_range(db, &parsed)
}

#[derive(Clone, Copy)]
enum Spelling {
    /// nothing on this type declares the name
    Undeclared,
    /// every declaration of the name is in a `.by` source file
    Chosen(Spelled),
    /// some declaration is elsewhere, or could not be traced
    Dictated,
}

fn member_spelling<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    ty: Type<'db>,
    name: &str,
) -> Spelling {
    if let Type::ModuleLiteral(module) = ty {
        let Some(module_file) = module.module(db).file(db) else {
            return Spelling::Undeclared;
        };
        let module_scope = global_scope(db, ProgramFile::new(db, module_file, env.program(db)));
        let resolved: Vec<_> = find_symbol_in_scope(db, module_scope, name)
            .into_iter()
            .flat_map(|definition| {
                resolve_definition(
                    db,
                    env,
                    definition,
                    Some(name),
                    ImportAliasResolution::PreserveAliases,
                )
            })
            .collect();
        if resolved.is_empty() {
            // a submodule is not a symbol in its package's scope, so the name is
            // the file's own
            return submodule_spelling(db, env, file, module.module(db).name(db), name);
        }
        return match all_chosen(db, &resolved, name) {
            Some(spelled) => Spelling::Chosen(spelled),
            None => Spelling::Dictated,
        };
    }

    // `super()._x` reads the member off the owner's MRO after the pivot class
    if let Type::BoundSuper(bound_super) = ty {
        let Some(mro) = bound_super.lookup_mro_after_pivot(db, env) else {
            return Spelling::Undeclared;
        };
        return mro_spelling(db, mro, name);
    }

    let meta_type = ty.to_meta_type(db, env);
    let lookup_type = match ty {
        Type::ClassLiteral(_) | Type::SubclassOf(_) | Type::GenericAlias(_) => ty,
        _ => meta_type,
    };
    let Some(class) = class_of(db, env, lookup_type) else {
        return Spelling::Undeclared;
    };
    let spelling = mro_spelling(db, class.iter_mro(db), name);
    // a class object also reads what its metaclass declares, which is only
    // reached when the class hierarchy declares nothing
    if matches!(spelling, Spelling::Undeclared)
        && meta_type != lookup_type
        && let Some(metaclass) = class_of(db, env, meta_type)
    {
        return mro_spelling(db, metaclass.iter_mro(db), name);
    }
    spelling
}

fn class_of<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<ClassLiteral<'db>> {
    match ty {
        Type::ClassLiteral(class) => Some(class),
        Type::GenericAlias(alias) => Some(ClassLiteral::Static(alias.origin(db))),
        Type::SubclassOf(subclass) => match subclass.subclass_of() {
            SubclassOfInner::Protocol(protocol) => protocol
                .class_origin(db)
                .map(|origin| origin.class_literal(db)),
            inner => inner
                .into_class(db, env)
                .map(|class| class.class_literal(db)),
        },
        _ => None,
    }
}

/// the spelling of `name` across every class in `mro` that declares it
fn mro_spelling<'db>(
    db: &'db dyn Db,
    mro: impl Iterator<Item = ClassBase<'db>>,
    name: &str,
) -> Spelling {
    let mut spelling = Spelling::Undeclared;
    for base in mro {
        let ancestor = match base {
            ClassBase::Class(class) => match class.static_class_literal(db) {
                Some((literal, _)) => literal,
                None => return Spelling::Dictated,
            },
            // an unknown base may declare any member at all
            ClassBase::Any | ClassBase::Dynamic(_) | ClassBase::Divergent(_) => {
                return Spelling::Dictated;
            }
            ClassBase::Protocol | ClassBase::Generic | ClassBase::TypedDict(_) => continue,
        };
        match underscore_members(db, ancestor).get(name) {
            None => continue,
            Some(UnderscoreMember::ParserSpelled) => return Spelling::Dictated,
            Some(UnderscoreMember::AuthorSpelled { declaration }) => {
                spelling = Spelling::Chosen(Spelled {
                    declaration: Some(*declaration),
                });
            }
        }
    }
    spelling
}

/// who spelled an underscore member a class declares
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnderscoreMember {
    /// the author, at this declaration
    AuthorSpelled { declaration: FileRange },
    /// the parser: a property's backing storage, an enum variant's anonymous field
    ParserSpelled,
}

// a `FileRange` is a file handle and two offsets, all of it inline
impl get_size2::GetSize for UnderscoreMember {}

/// every underscore member `class` itself declares, and who spelled the name
///
/// only the underscore names are collected, because they are the only ones the
/// lint can ask about, and the answer is cached per class: a member read is on the
/// hot path of inference, and every read of one would otherwise walk the class
/// body and each of its method scopes again
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
fn underscore_members<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> FxHashMap<Name, UnderscoreMember> {
    let class_scope = class.body_scope(db);
    let mut members: FxHashMap<Name, UnderscoreMember> = FxHashMap::default();
    let mut record = |name: &str, definition: Definition<'db>| {
        if !is_underscore_name(name) {
            return;
        }
        let spelling = if *definition_is_author_spelled(db, definition, Name::new(name)) {
            UnderscoreMember::AuthorSpelled {
                declaration: definition_range(db, definition),
            }
        } else {
            UnderscoreMember::ParserSpelled
        };
        members
            .entry(Name::new(name))
            .and_modify(|previous| {
                // a name any one declaration spells for the author is the
                // parser's, whichever of them a read resolves to
                if spelling == UnderscoreMember::ParserSpelled {
                    *previous = UnderscoreMember::ParserSpelled;
                }
            })
            .or_insert(spelling);
    };

    let place_table = ty_python_core::place_table(db, class_scope);
    let use_def = use_def_map(db, class_scope);
    for symbol in place_table.symbols() {
        let name = symbol.name().as_str();
        if !is_underscore_name(name) {
            continue;
        }
        let symbol_id = place_table
            .symbol_id(name)
            .expect("symbol is in this table");
        for definition in use_def
            .reachable_symbol_declarations(symbol_id)
            .filter_map(|declaration| declaration.declaration.definition())
            .chain(
                use_def
                    .reachable_symbol_bindings(symbol_id)
                    .filter_map(|binding| binding.binding.definition()),
            )
        {
            record(name, definition);
        }
    }

    let index = semantic_index(db, class_scope.program_file(db));
    for method_scope in attribute_scopes(db, class_scope) {
        let method_places = index.place_table(method_scope);
        let method_use_def = index.use_def_map(method_scope);
        for member in method_places.members() {
            let Some(name) = member.as_instance_attribute() else {
                continue;
            };
            if !is_underscore_name(name) {
                continue;
            }
            let Some(member_id) = method_places.member_id_by_instance_attribute_name(name) else {
                continue;
            };
            for definition in method_use_def
                .reachable_member_declarations(member_id)
                .filter_map(|declaration| declaration.declaration.definition())
                .chain(
                    method_use_def
                        .reachable_member_bindings(member_id)
                        .filter_map(|binding| binding.binding.definition()),
                )
            {
                record(name, definition);
            }
        }
    }

    members.shrink_to_fit();
    members
}

fn all_chosen(db: &dyn Db, resolved: &[ResolvedDefinition<'_>], name: &str) -> Option<Spelled> {
    if resolved.is_empty() {
        return None;
    }
    let chosen = resolved.iter().all(|resolved| match resolved {
        ResolvedDefinition::Definition(definition) => {
            *definition_is_author_spelled(db, *definition, Name::new(name))
        }
        ResolvedDefinition::Module(file) => is_basedpython_source(db, file.file(db)),
        ResolvedDefinition::FileWithRange(range) => {
            is_basedpython_source(db, range.file())
                && written_by_author(db, range.file(), range.range(), name)
        }
    });
    chosen.then(|| Spelled {
        declaration: resolved.iter().find_map(|resolved| match resolved {
            ResolvedDefinition::Definition(definition) => Some(definition_range(db, *definition)),
            ResolvedDefinition::FileWithRange(range) => Some(*range),
            // a module is declared by its own file, which has no range to point at
            ResolvedDefinition::Module(_) => None,
        }),
    })
}

/// whether `definition` is in a `.by` source file and its name written there by
/// its author
///
/// the parser declares names nobody wrote — a property's backing storage, the
/// anonymous fields of an enum variant `case Same(int)` — and those are
/// basedpython's to spell, not the author's. it records each one, so the question
/// is answered by asking it rather than by reading the source back
///
/// cached because it reads the *declaring* file's syntax tree, which inference of
/// every file that uses the name would otherwise depend on
#[salsa::tracked]
fn definition_is_author_spelled<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    name: Name,
) -> bool {
    let file = definition.file(db);
    if !is_basedpython_source(db, file) || is_unresolved_import(db, definition) {
        return false;
    }
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let range = definition.kind(db).target_range(&module);
    module.generated_names().get(range, name) != Some(GeneratedName::ParserName)
}

/// whether `definition` is an import that resolution handed back as it was: one
/// without an alias only does that when its target could not be found
fn is_unresolved_import<'db>(db: &'db dyn Db, definition: Definition<'db>) -> bool {
    let module = parsed_module(db, definition.python_file(db)).load(db);
    match definition.kind(db) {
        DefinitionKind::Import(import) => import.alias(&module).asname.is_none(),
        DefinitionKind::ImportFrom(import) => import.alias(&module).asname.is_none(),
        DefinitionKind::StarImport(_) => true,
        _ => false,
    }
}

fn is_basedpython_source(db: &dyn Db, file: File) -> bool {
    file.source_type(db) == PySourceType::BasedPython
}

/// whether the author wrote the name at `range` in `file`, rather than the parser
fn written_by_author(db: &dyn Db, file: File, range: TextRange, name: &str) -> bool {
    let module = parsed_module(db, db.program_file(file).python_file(db)).load(db);
    module
        .generated_names()
        .get(range, Name::new(name))
        .is_none()
}

/// whether the submodule `name` of the package `package` is a `.by` file: a name
/// a module reaches as an attribute of its package, which no symbol in the
/// package's own scope declares
fn submodule_spelling<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    importing_file: File,
    package: &ModuleName,
    name: &str,
) -> Spelling {
    let Some(name) = ModuleName::new(name) else {
        return Spelling::Undeclared;
    };
    let mut submodule = package.clone();
    submodule.extend(&name);
    let importing = ImportingFile::File(importing_file, env.resolver_environment(db));
    let Some(file) = resolve_module(db, importing, &submodule).and_then(|module| module.file(db))
    else {
        return Spelling::Undeclared;
    };
    if is_basedpython_source(db, file) {
        Spelling::Chosen(Spelled { declaration: None })
    } else {
        Spelling::Dictated
    }
}
