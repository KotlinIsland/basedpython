//! pulling a class's members up into one of its superclasses, and pushing them down into its
//! subclasses
//!
//! ```py
//! class Shape:                          class Shape:
//!     pass                                  def describe(self):
//!                               ──►             return type(self).__name__
//! class Square(Shape):
//!     def describe(self):               class Square(Shape):
//!         return type(self).__name__        pass
//! ```
//!
//! a member is every statement of a class body that binds its name: a method (with its property
//! accessors or overloads, which share the name), or a class attribute. it moves with its
//! decorators and the comments directly above it, reindented for the body it lands in
//!
//! either move is refused, with the reason, where the program would no longer mean what it
//! meant:
//!
//! - pulled up, a member that reads an attribute of its receiver the superclass does not have,
//!   which the superclass's other subclasses would then miss; that reads, where the class body
//!   evaluates it — a value, a default, a decorator — a name the class body binds and that is
//!   not pulled with it, or a name bound after the superclass is defined; that uses `super()`
//!   from a class whose base is not the superclass, since `super()` would then start from
//!   elsewhere; that a class below the superclass would then find in another class than it does,
//!   by its method resolution order — another subclass that finds it in a mixin or above the
//!   superclass, or the class itself, in a class it finds before the superclass; or that the
//!   superclass binds already
//! - pushed down, a member something reads off the class itself or an instance that may be the
//!   class rather than a subclass — another of its methods, through `self`, included — or of a
//!   class without subclasses in the project
//! - either way, a name-mangled `__member`, whose name depends on the class it is written in,
//!   a method that reads `__class__`, which is the class it is written in, a member that reads a
//!   name one class finds in a scope around it — a function it is defined in, say — and the
//!   other finds elsewhere, and, between modules, a member that reads a name of its module that
//!   the other would have to import from a module that imports it
//!
//! `override` follows the move: a method pulled up overrides nothing where it lands unless the
//! superclass inherits the name, so it loses the modifier there, and the methods of the same
//! name below the superclass then override it and gain the modifier; a method pushed down keeps
//! it only where something above still has the name

use ruff_db::files::File;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::source_text;
use ruff_diagnostics::Edit;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor, walk_expr};
use ruff_python_ast::{self as ast, Expr, ExprContext, Stmt};
use ruff_python_codegen::Stylist;
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::FxHashSet;
use ty_module_resolver::file_to_module;
use ty_project::Db;
use ty_python_core::ProgramFile;
use ty_python_core::scope::{FileScopeId, NodeWithScopeRef};
use ty_python_core::semantic_index;
use ty_python_semantic::importer::{ImportAction, ImportRequest, MembersInScope};
use ty_python_semantic::types::Type;
use ty_python_semantic::types::ide_support::{
    decorated_function, is_enum_class, is_override_function, is_super, reads_class_cell,
};
use ty_python_semantic::{
    HasDefinition, HasType, ProgramEnvironment, SemanticModel, class_bases, instance_member_names,
    is_within_classes, type_hierarchy_prepare, type_hierarchy_subtypes,
};

use super::flow::{bindings_of_symbol, resolving_scope, resolving_scope_among};
use super::move_symbol::{
    Dependency, OwnedRequest, SymbolMove, checked_dependencies, cut_range, imports_module,
    line_number, loads,
};
use crate::code_action::FileEdit;
use crate::goto::find_goto_target;
use crate::import_order;
use crate::method_kind::{MethodKind, resolved_method_kind};
use crate::references::{ReferencesMode, references};

mod bases;

/// what kind of member of a class a member is
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Method,
    ClassMethod,
    StaticMethod,
    Property,
    Attribute,
    /// a base the class statement names, which a pull up moves into the superclass's bases
    Base,
}

/// a member [`pull_members_up`] or [`push_members_down`] can move, or a base a pull up can
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassMember {
    /// its name, or a base as the class statement writes it
    pub name: String,
    pub kind: MemberKind,
    /// the range of its name in its first statement, or of the base
    pub name_range: TextRange,
    /// how it reads in a list of members: a method or property as its name and its parameters
    /// as its first statement writes them, `fetch(self, key: str)`, an attribute as its name
    pub detail: String,
}

/// a class of the project, where its name is
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HierarchyClass {
    pub file: File,
    pub name: String,
    /// its module's dotted name and its own, the way [`pull_members_up`] is told which
    /// superclass to pull into
    pub qualified_name: Option<String>,
    pub name_range: TextRange,
}

/// the members of the class at a position, and the classes of the project above and below it
/// they could move to
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovableMembers {
    pub class: HierarchyClass,
    pub members: Vec<ClassMember>,
    /// the project's classes the class inherits from, nearest first
    pub superclasses: Vec<HierarchyClass>,
    /// the project's classes that name the class as a base
    pub subclasses: Vec<HierarchyClass>,
}

/// the class around `offset`, its members, and where they could go
pub fn movable_members(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
) -> Option<MovableMembers> {
    let class = ClassSite::at(db, file, offset)?;
    let def = class.def()?;
    let source = source_text(db, class.file_id);
    let model = SemanticModel::new(db, class.file);
    let mut members: Vec<ClassMember> = Vec::new();
    for statement in &def.body {
        let (Some(name), Some(kind)) = (member_name(statement), member_kind(&model, statement))
        else {
            continue;
        };
        if let Some(existing) = members
            .iter_mut()
            .find(|member| member.name == name.as_str())
        {
            if kind == MemberKind::Property {
                existing.kind = MemberKind::Property;
            }
            continue;
        }
        members.push(ClassMember {
            name: name.to_string(),
            kind,
            name_range: member_name_range(statement)?,
            detail: member_detail(source.as_str(), statement),
        });
    }
    members.extend(
        bases::written_bases(db, &class, def, source.as_str())
            .into_iter()
            .map(|base| ClassMember {
                name: base.text.to_string(),
                kind: MemberKind::Base,
                name_range: base.expression.range(),
                detail: base.text.to_string(),
            }),
    );
    Some(MovableMembers {
        class: class.item(db),
        members,
        superclasses: class
            .ancestors(db)
            .iter()
            .map(|class| class.item(db))
            .collect(),
        subclasses: class
            .subclasses(db)
            .iter()
            .map(|class| class.item(db))
            .collect(),
    })
}

/// moves the members `names` and the bases `bases`, each as the class statement writes it, of the
/// class around `offset` into its superclass `target`, named by its qualified name as
/// [`movable_members`] lists it. `Err` says why they cannot be moved
pub fn pull_members_up(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
    names: &[String],
    bases: &[String],
    target: &str,
) -> Result<SymbolMove, String> {
    let class =
        ClassSite::at(db, file, offset).ok_or_else(|| "there is no class here".to_string())?;
    let def = class
        .def()
        .ok_or_else(|| "there is no class here".to_string())?;
    let ancestors = class.ancestors(db);
    let superclass = ancestors
        .iter()
        .find(|ancestor| ancestor.qualified_name.as_deref() == Some(target))
        .ok_or_else(|| {
            format!(
                "{target} is not a class of the project {} inherits from",
                class.name
            )
        })?;
    let super_def = superclass
        .def()
        .ok_or_else(|| format!("{target} cannot be read"))?;
    let env = ProgramEnvironment::from_file(class.file);
    let mut super_members = instance_member_names(db, &env, superclass.ty);
    let source = source_text(db, class.file.file(db));
    let source = source.as_str();
    let model = SemanticModel::new(db, class.file);
    let super_model = SemanticModel::new(db, superclass.file);

    let mut moved = SymbolMove::default();
    if !bases.is_empty() {
        let pulled = bases::pull_bases_up(
            db,
            &class,
            superclass,
            &bases.iter().map(String::as_str).collect::<Vec<_>>(),
            source,
        )?;
        // what the bases bring is the superclass's from now on, for the members pulled with them
        super_members.extend(pulled.gained);
        moved.edits.extend(pulled.edits);
    }
    let pulled: FxHashSet<&str> = names.iter().map(String::as_str).collect();
    let below_superclass = superclass.descendants(db);

    let mut texts: Vec<(usize, MemberKind, String)> = Vec::new();
    let mut removed: Vec<usize> = Vec::new();
    let mut target_imports: Vec<OwnedRequest> = Vec::new();
    let mut overriding: Vec<String> = Vec::new();
    for name in names {
        refuse_mangled(name)?;
        let statements = member_statements(def, name);
        if statements.is_empty() {
            return Err(format!("{} has no member `{name}`", class.name));
        }
        if super_def
            .body
            .iter()
            .any(|statement| binds(statement, name))
            || assigns_on_receiver(&super_model, super_def, name)?
        {
            return Err(format!("{} has a `{name}` already", superclass.name));
        }
        let inherited_above = super_members.contains(&Name::new(name));
        bases::refuse_changed_lookups(db, &class, superclass, &below_superclass, name)?;
        for &index in &statements {
            let statement = &def.body[index];
            let kind = member_kind(&model, statement).ok_or_else(|| unknown_kind(name))?;
            // what it reads off its receiver has to be there in the superclass
            for (attribute, range) in receiver_attributes(&model, statement)? {
                if pulled.contains(attribute.as_str()) || super_members.contains(&attribute) {
                    continue;
                }
                refuse_mangled(&attribute)?;
                let is_member = def
                    .body
                    .iter()
                    .any(|statement| binds(statement, &attribute));
                return Err(format!(
                    "`{name}` uses `{}` on line {}, which {} does not have{}",
                    &source[range],
                    line_number(source, range.start()),
                    superclass.name,
                    if is_member { "; pull it up too" } else { "" }
                ));
            }
            for read in class_body_reads(db, &class, def, statement) {
                match read {
                    BodyRead::Member {
                        name: sibling,
                        index: read_index,
                    } => {
                        if !pulled.contains(sibling.as_str()) {
                            return Err(format!(
                                "`{name}` reads {}'s `{sibling}`, which it would no longer read \
                                 in {}'s body; pull it up too",
                                class.name, superclass.name
                            ));
                        }
                        // an attribute lands before the superclass's methods, and a method
                        // after them
                        if is_attribute(statement)
                            && read_index
                                .is_some_and(|read_index| !is_attribute(&def.body[read_index]))
                        {
                            return Err(format!(
                                "`{name}` reads `{sibling}`, which {} would define after it, \
                                 among its methods",
                                superclass.name
                            ));
                        }
                    }
                    BodyRead::Outside {
                        name: read,
                        resolves_to,
                    } => {
                        if superclass.file_id != class.file_id {
                            // what another module would have to import is asked below
                            continue;
                        }
                        let index = semantic_index(db, class.file);
                        let super_scope = index
                            .try_node_scope(NodeWithScopeRef::Class(super_def))
                            .ok_or_else(|| format!("{} cannot be read", superclass.name))?;
                        if resolving_scope(index, super_scope, read.as_str()) != resolves_to {
                            return Err(format!(
                                "`{name}` reads `{read}`, which is another name where {} is \
                                 defined",
                                superclass.name
                            ));
                        }
                        if let Some(scope) = resolves_to
                            && bound_between(db, &class, scope, read.as_str(), super_def, def)
                        {
                            return Err(format!(
                                "`{name}` reads `{read}`, which is bound after {} is defined",
                                superclass.name
                            ));
                        }
                    }
                }
            }
            if let Some(read) =
                read_rebound_around(db, &class, def, statement, superclass, super_def)
            {
                return Err(format!(
                    "`{name}` reads `{read}`, which is another name where {} is defined",
                    superclass.name
                ));
            }
            if uses_super(&model, statement) && !class.has_only_base(db, superclass) {
                return Err(format!(
                    "`{name}` calls `super()`, which would start from {} rather than from {}",
                    superclass.name, class.name
                ));
            }
            if member_reads_class_cell(&model, statement) {
                return Err(class_cell_refusal(name, &superclass.name, &class.name));
            }
            if superclass.file_id != class.file_id {
                for dependency in checked_dependencies(
                    db,
                    class.file,
                    &class.parsed,
                    statement,
                    name,
                    None,
                    superclass.file,
                )? {
                    match dependency {
                        Dependency::Imported { request, .. } => target_imports.push(request),
                        Dependency::Sibling(sibling) => {
                            let module = class.module(db).unwrap_or_default();
                            return Err(format!(
                                "`{name}` uses `{sibling}`, which {} would have to import from \
                                 {module}, which imports it",
                                superclass.module(db).unwrap_or_default()
                            ));
                        }
                    }
                }
            }
            texts.push((
                index,
                kind,
                member_text(
                    source,
                    &def.body,
                    index,
                    override_marker(db, &class, statement).filter(|_| !inherited_above),
                ),
            ));
            removed.push(index);
        }
        if !inherited_above {
            overriding.push(name.clone());
        }
    }

    // the subclass loses them, and the superclass gains them in the order they were written
    texts.sort_by_key(|(index, _, _)| *index);
    moved
        .edits
        .extend(remove_members(class.file.file(db), source, def, &removed));

    // the superclass gains them, attributes before its methods and methods after them
    let super_source = source_text(db, superclass.file.file(db));
    let super_source = super_source.as_str();
    let from_indent = body_indentation(source, def);
    let to_indent = body_indentation(super_source, super_def);
    moved.edits.extend(insert_members(
        superclass.file.file(db),
        super_source,
        super_def,
        &texts
            .iter()
            .map(|(_, kind, text)| (*kind, reindent(text, &from_indent, &to_indent)))
            .collect::<Vec<_>>(),
        Stylist::from_tokens(superclass.parsed.tokens(), super_source)
            .line_ending()
            .as_str(),
    ));
    if !target_imports.is_empty() {
        moved
            .edits
            .extend(add_imports(db, superclass, &target_imports)?);
    }

    // what defines a name pulled up below the superclass now overrides it
    for name in &overriding {
        for below in superclass.descendants(db) {
            if below.same(&class) || class.is_ancestor_of(db, &below) {
                continue;
            }
            if let Some(below_def) = below.def()
                && let Some(&first) = member_statements(below_def, name).first()
                && override_marker(db, &below, &below_def.body[first]).is_none()
            {
                moved
                    .edits
                    .extend(add_override(db, &below, &below_def.body[first]));
            }
        }
    }
    Ok(moved)
}

/// moves the members `names` of the class around `offset` into each of the project's classes
/// that name it as a base, as a copy into each that does not define the member itself. `Err`
/// says why they cannot be moved
pub fn push_members_down(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
    names: &[String],
) -> Result<SymbolMove, String> {
    let class =
        ClassSite::at(db, file, offset).ok_or_else(|| "there is no class here".to_string())?;
    let def = class
        .def()
        .ok_or_else(|| "there is no class here".to_string())?;
    let subclasses = class.subclasses(db);
    if subclasses.is_empty() {
        return Err(format!(
            "no class of the project names {} as a base, so there is nowhere to push to",
            class.name
        ));
    }
    let env = ProgramEnvironment::from_file(class.file);
    let subclass_types: Vec<Type> = subclasses.iter().map(|subclass| subclass.ty).collect();
    let inherited: FxHashSet<Name> = class_bases(db, &env, class.ty)
        .into_iter()
        .flat_map(|base| instance_member_names(db, &env, base))
        .collect();
    let source = source_text(db, class.file.file(db));
    let source = source.as_str();
    let model = SemanticModel::new(db, class.file);

    let mut moved = SymbolMove::default();
    let mut removed: Vec<usize> = Vec::new();
    let mut texts: Vec<(MemberKind, String, Vec<usize>)> = Vec::new();
    for name in names {
        refuse_mangled(name)?;
        let statements = member_statements(def, name);
        let Some(&first) = statements.first() else {
            return Err(format!("{} has no member `{name}`", class.name));
        };
        let inherited_above = inherited.contains(&Name::new(name));

        // nothing may read it off the class, or what may be an instance of the class itself
        let statement_ranges: Vec<TextRange> = statements
            .iter()
            .map(|&index| def.body[index].range())
            .collect();
        if let Some(name_range) = member_name_range(&def.body[first])
            && let Some(goto) = find_goto_target(&model, &class.parsed, name_range.start())
        {
            for reference in references(db, class.file, &goto, ReferencesMode::RenameMultiFile)
                .unwrap_or_default()
            {
                if reference.file() == class.file.file(db)
                    && statement_ranges
                        .iter()
                        .any(|range| range.contains_range(reference.range()))
                {
                    continue;
                }
                let reference_file = ProgramFile::new(db, reference.file(), class.file.program(db));
                let Some(receiver) = attribute_receiver_type(db, reference_file, reference.range())
                else {
                    continue;
                };
                let reference_env = ProgramEnvironment::from_file(reference_file);
                if !is_within_classes(db, &reference_env, receiver, &subclass_types) {
                    let text = source_text(db, reference.file());
                    let path = reference.file().path(db);
                    let root = db.project().root(db);
                    let path = path
                        .as_system_path()
                        .and_then(|path| path.strip_prefix(root).ok())
                        .map_or_else(|| path.to_string(), ToString::to_string);
                    return Err(format!(
                        "{}:{} uses `{name}` on what may be a {} rather than a subclass",
                        path,
                        line_number(text.as_str(), reference.range().start()),
                        class.name
                    ));
                }
            }
        }

        for &index in &statements {
            let statement = &def.body[index];
            if member_reads_class_cell(&model, statement) {
                return Err(class_cell_refusal(
                    name,
                    "the subclass it lands in",
                    &class.name,
                ));
            }
            if uses_super(&model, statement) {
                for subclass in &subclasses {
                    if !subclass.has_only_base(db, &class) {
                        return Err(format!(
                            "`{name}` calls `super()`, which in {} would not start from {}",
                            subclass.name, class.name
                        ));
                    }
                }
            }
            texts.push((
                member_kind(&model, statement).ok_or_else(|| unknown_kind(name))?,
                member_text(
                    source,
                    &def.body,
                    index,
                    override_marker(db, &class, statement).filter(|_| !inherited_above),
                ),
                vec![index],
            ));
            removed.push(index);
        }
    }

    moved
        .edits
        .extend(remove_members(class.file.file(db), source, def, &removed));

    for subclass in &subclasses {
        let Some(sub_def) = subclass.def() else {
            continue;
        };
        let sub_source = source_text(db, subclass.file.file(db));
        let sub_source = sub_source.as_str();
        let from_indent = body_indentation(source, def);
        let to_indent = body_indentation(sub_source, sub_def);
        let mut copies = Vec::new();
        let mut imports = Vec::new();
        for (kind, text, indices) in &texts {
            let statement = &def.body[indices[0]];
            let Some(name) = member_name(statement) else {
                continue;
            };
            let own = member_statements(sub_def, name.as_str());
            if let Some(&first) = own.first() {
                // it keeps its own, which overrides nothing once the class no longer has it
                let own_statement = &sub_def.body[first];
                if !inherited.contains(name)
                    && override_marker(db, subclass, own_statement).is_some()
                {
                    moved
                        .edits
                        .extend(remove_override(db, subclass, own_statement));
                }
                continue;
            }
            if let Some(read) = read_rebound_around(db, &class, def, statement, subclass, sub_def) {
                return Err(format!(
                    "`{name}` reads `{read}`, which is another name where {} is defined",
                    subclass.name
                ));
            }
            copies.push((*kind, reindent(text, &from_indent, &to_indent)));
            if subclass.file_id != class.file_id {
                for dependency in checked_dependencies(
                    db,
                    class.file,
                    &class.parsed,
                    statement,
                    name,
                    None,
                    subclass.file,
                )? {
                    match dependency {
                        Dependency::Imported { request, .. } => imports.push(request),
                        Dependency::Sibling(sibling) => {
                            let module = class.module(db).unwrap_or_default();
                            let sub_module = file_to_module(db, subclass.file.resolver_file(db))
                                .map(|module| module.name(db).clone());
                            if let Some(sub_module) = &sub_module
                                && imports_module(db, class.file, &class.parsed, sub_module)
                            {
                                return Err(format!(
                                    "`{name}` uses `{sibling}`, which {sub_module} would have to \
                                     import from {module}, which imports it"
                                ));
                            }
                            imports.push(OwnedRequest {
                                module,
                                member: Some(sibling.clone()),
                                asname: None,
                                binds: sibling,
                            });
                        }
                    }
                }
            }
        }
        if copies.is_empty() {
            continue;
        }
        moved.edits.extend(insert_members(
            subclass.file.file(db),
            sub_source,
            sub_def,
            &copies,
            Stylist::from_tokens(subclass.parsed.tokens(), sub_source)
                .line_ending()
                .as_str(),
        ));
        if !imports.is_empty() {
            moved.edits.extend(add_imports(db, subclass, &imports)?);
        }
    }
    Ok(moved)
}

/// makes a new class `name`, directly above the class around `offset` in its module, that takes
/// the class's bases and its members `names`, and makes it the class's one base. the class's
/// method resolution order is what it was with the new class inserted right after it, so every
/// member is found where it was found, and `super()` in a moved method starts where it started.
/// a class that passes a keyword other than `metaclass` when it is made is refused, since the
/// keyword would be passed for the new class instead. `Err` says why it cannot be extracted
pub fn extract_superclass(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
    names: &[String],
    name: &str,
) -> Result<SymbolMove, String> {
    let class =
        ClassSite::at(db, file, offset).ok_or_else(|| "there is no class here".to_string())?;
    let def = class
        .def()
        .ok_or_else(|| "there is no class here".to_string())?;
    if !ruff_python_stdlib::identifiers::is_identifier(name)
        || ruff_python_stdlib::keyword::is_keyword(name)
    {
        return Err(format!("`{name}` is not a name a class can have"));
    }
    if def.type_params.is_some() {
        return Err(format!(
            "{} is generic, and its type parameters would have to be shared",
            class.name
        ));
    }
    if all_classes(&class.parsed)
        .iter()
        .any(|other| other.name.as_str() == name)
        || module_binds(&class, db, name)
    {
        return Err(format!(
            "{} has a `{name}` already",
            class.module(db).unwrap_or_default()
        ));
    }
    let source = source_text(db, class.file.file(db));
    let source = source.as_str();
    let env = ProgramEnvironment::from_file(class.file);
    let bases = class_bases(db, &env, class.ty);
    // a base that is a special form rather than a class — `Protocol`, `NamedTuple`, `TypedDict`,
    // `Generic[T]` — or an enum makes a subclass something other than what the base itself is,
    // so the class would stop being what it is once it subclassed the new class instead
    let written_bases = def
        .arguments
        .as_ref()
        .map_or(0, |arguments| arguments.args.len());
    if bases.len() != written_bases || bases.iter().any(|base| is_enum_class(db, &env, *base)) {
        return Err(format!(
            "{} has a base whose subclasses python treats specially, which {name} would take \
             in its place",
            class.name
        ));
    }
    // a keyword is passed to `__init_subclass__` and the metaclass for the class it is written on
    // alone, while a metaclass the new class is made by makes the class too
    if let Some(keyword) = def.arguments.as_ref().and_then(|arguments| {
        arguments
            .keywords
            .iter()
            .find(|keyword| keyword.arg.as_ref().is_none_or(|arg| arg != "metaclass"))
    }) {
        return Err(format!(
            "{} passes `{}` when it is made, which {name} would be passed instead",
            class.name,
            &source[keyword.range()]
        ));
    }
    // what the new class has: everything the class's bases give it, and what moves into it
    let inherited: FxHashSet<Name> = bases
        .into_iter()
        .flat_map(|base| instance_member_names(db, &env, base))
        .collect();
    let extracted: FxHashSet<&str> = names.iter().map(String::as_str).collect();
    let model = SemanticModel::new(db, class.file);

    let mut texts: Vec<(usize, MemberKind, String)> = Vec::new();
    let mut removed: Vec<usize> = Vec::new();
    for member in names {
        refuse_mangled(member)?;
        let statements = member_statements(def, member);
        if statements.is_empty() {
            return Err(format!("{} has no member `{member}`", class.name));
        }
        for &index in &statements {
            let statement = &def.body[index];
            if !def.decorator_list.is_empty() && is_attribute(statement) {
                return Err(format!(
                    "`{member}` is an attribute of a decorated class, which its decorator may \
                     read as a field"
                ));
            }
            for (attribute, range) in receiver_attributes(&model, statement)? {
                if extracted.contains(attribute.as_str()) || inherited.contains(&attribute) {
                    continue;
                }
                refuse_mangled(&attribute)?;
                let is_member = def
                    .body
                    .iter()
                    .any(|statement| binds(statement, &attribute));
                return Err(format!(
                    "`{member}` uses `{}` on line {}, which {name} would not have{}",
                    &source[range],
                    line_number(source, range.start()),
                    if is_member { "; extract it too" } else { "" }
                ));
            }
            if member_reads_class_cell(&model, statement) {
                return Err(class_cell_refusal(member, name, &class.name));
            }
            // the new class is written right above the class, so a name read from outside the
            // class body is the same name there
            for read in class_body_reads(db, &class, def, statement) {
                if let BodyRead::Member { name: sibling, .. } = read
                    && !extracted.contains(sibling.as_str())
                {
                    return Err(format!(
                        "`{member}` reads {}'s `{sibling}`, which it would no longer read in \
                         {name}'s body; extract it too",
                        class.name
                    ));
                }
            }
            texts.push((
                index,
                member_kind(&model, statement).ok_or_else(|| unknown_kind(member))?,
                member_text(source, &def.body, index, None),
            ));
            removed.push(index);
        }
    }
    texts.sort_by_key(|(index, _, _)| *index);

    let file_id = class.file.file(db);
    let line_ending = Stylist::from_tokens(class.parsed.tokens(), source)
        .line_ending()
        .as_str()
        .to_string();
    let mut moved = SymbolMove::default();
    moved
        .edits
        .extend(remove_members(file_id, source, def, &removed));

    // the class takes the new one as its only base, which takes its bases
    let base_text = def
        .arguments
        .as_ref()
        .map(|arguments| {
            let text = &source[arguments.range()];
            text[1..text.len() - 1].trim().to_string()
        })
        .filter(|bases| !bases.is_empty());
    let new_bases = format!("({name})");
    moved.edits.push(FileEdit {
        file: file_id,
        edit: match &def.arguments {
            Some(arguments) => Edit::range_replacement(new_bases, arguments.range()),
            None => Edit::insertion(new_bases, def.name.end()),
        },
    });

    // the new class, above the class and the comments and decorators that go with it
    let class_indent = {
        let start = source.line_start(def.start());
        source[TextRange::new(start, def.start())].to_string()
    };
    let body_indent = format!("{class_indent}    ");
    let from_indent = body_indentation(source, def);
    let mut text = format!(
        "{class_indent}class {name}{}:{line_ending}",
        base_text
            .map(|bases| format!("({bases})"))
            .unwrap_or_default()
    );
    let mut previous: Option<MemberKind> = None;
    for (_, kind, member) in &texts {
        if previous.is_some_and(|previous| {
            previous != MemberKind::Attribute || *kind != MemberKind::Attribute
        }) {
            text.push_str(&line_ending);
        }
        text.push_str(&reindent(member, &from_indent, &body_indent));
        previous = Some(*kind);
    }
    if texts.is_empty() {
        text.push_str(&body_indent);
        text.push_str("pass");
        text.push_str(&line_ending);
    }
    let blank_lines = if class_indent.is_empty() { 2 } else { 1 };
    text.push_str(&line_ending.repeat(blank_lines));
    let mut start = source.line_start(def.start());
    while start > TextSize::default() {
        let previous = source.line_start(start - TextSize::from(1));
        if !source[TextRange::new(previous, start)]
            .trim_start()
            .starts_with('#')
        {
            break;
        }
        start = previous;
    }
    moved.edits.push(FileEdit {
        file: file_id,
        edit: Edit::insertion(text, start),
    });
    Ok(moved)
}

/// whether the module of `class` binds `name` at its top level
fn module_binds(class: &ClassSite<'_>, db: &dyn Db, name: &str) -> bool {
    let index = ty_python_core::semantic_index(db, class.file);
    let globals = index.place_table(ty_python_core::scope::FileScopeId::global());
    globals
        .symbol_id(name)
        .is_some_and(|symbol| globals.symbol(symbol).is_bound())
}

/// a class of the project, parsed
struct ClassSite<'db> {
    file: ProgramFile<'db>,
    file_id: File,
    parsed: ParsedModuleRef,
    name: String,
    name_range: TextRange,
    ty: Type<'db>,
    qualified_name: Option<String>,
}

impl<'db> ClassSite<'db> {
    /// the innermost class whose definition `offset` is in
    fn at(db: &'db dyn Db, file: ProgramFile<'db>, offset: TextSize) -> Option<Self> {
        let parsed = parsed_module(db, file.python_file(db)).load(db);
        let def = all_classes(&parsed)
            .into_iter()
            .filter(|class| class.range().contains_inclusive(offset))
            .min_by_key(|class| class.range().len())?;
        let name_range = def.name.range();
        let ty = def.inferred_type(&SemanticModel::new(db, file))?;
        Self::new(db, file, parsed, name_range, ty)
    }

    /// the class whose name is at `name_range` in `file`, of the type `ty`
    fn new(
        db: &'db dyn Db,
        file: ProgramFile<'db>,
        parsed: ParsedModuleRef,
        name_range: TextRange,
        ty: Type<'db>,
    ) -> Option<Self> {
        if !db.project().files(db).contains(file.file(db)) {
            return None;
        }
        let def = all_classes(&parsed)
            .into_iter()
            .find(|class| class.name.range() == name_range)?;
        let name = def.name.to_string();
        let qualified_name = file_to_module(db, file.resolver_file(db))
            .map(|module| format!("{}.{name}", module.name(db)));
        Some(Self {
            file,
            file_id: file.file(db),
            parsed,
            name,
            name_range,
            ty,
            qualified_name,
        })
    }

    /// the class described by `class`, a class of the type hierarchy, of the type `ty`
    fn of_hierarchy(
        db: &'db dyn Db,
        program: ProgramFile<'db>,
        class: &ty_python_semantic::TypeHierarchyClass<'db>,
        ty: Type<'db>,
    ) -> Option<Self> {
        let file = ProgramFile::new(db, class.file.file(db), program.program(db));
        let parsed = parsed_module(db, file.python_file(db)).load(db);
        Self::new(db, file, parsed, class.selection_range, ty)
    }

    fn def(&self) -> Option<&ast::StmtClassDef> {
        all_classes(&self.parsed)
            .into_iter()
            .find(|class| class.name.range() == self.name_range)
    }

    fn same(&self, other: &ClassSite<'_>) -> bool {
        self.file_id == other.file_id && self.name_range == other.name_range
    }

    fn module(&self, db: &dyn Db) -> Option<String> {
        file_to_module(db, self.file.resolver_file(db)).map(|module| module.name(db).to_string())
    }

    fn item(&self, db: &dyn Db) -> HierarchyClass {
        HierarchyClass {
            file: self.file.file(db),
            name: self.name.clone(),
            qualified_name: self.qualified_name.clone(),
            name_range: self.name_range,
        }
    }

    /// the project's classes this class names as its bases
    fn bases(&self, db: &'db dyn Db) -> Vec<ClassSite<'db>> {
        let env = ProgramEnvironment::from_file(self.file);
        class_bases(db, &env, self.ty)
            .into_iter()
            .filter_map(|base| {
                let hierarchy = type_hierarchy_prepare(db, &env, base)?;
                Self::of_hierarchy(db, self.file, &hierarchy, base)
            })
            .collect()
    }

    /// the project's classes this class inherits from, nearest first
    fn ancestors(&self, db: &'db dyn Db) -> Vec<ClassSite<'db>> {
        let mut found: Vec<ClassSite<'db>> = Vec::new();
        let mut pending = self.bases(db);
        pending.reverse();
        while let Some(next) = pending.pop() {
            if found.iter().any(|seen| seen.same(&next)) || next.same(self) {
                continue;
            }
            let mut bases = next.bases(db);
            bases.reverse();
            found.push(next);
            pending.extend(bases);
        }
        found
    }

    /// whether this class names `base` as its one base
    fn has_only_base(&self, db: &'db dyn Db, base: &ClassSite<'_>) -> bool {
        let env = ProgramEnvironment::from_file(self.file);
        let bases = class_bases(db, &env, self.ty);
        bases.len() == 1 && self.bases(db).first().is_some_and(|only| only.same(base))
    }

    fn is_ancestor_of(&self, db: &'db dyn Db, other: &ClassSite<'db>) -> bool {
        other
            .ancestors(db)
            .iter()
            .any(|ancestor| ancestor.same(self))
    }

    /// the project's classes that name this class as a base
    fn subclasses(&self, db: &'db dyn Db) -> Vec<ClassSite<'db>> {
        let env = ProgramEnvironment::from_file(self.file);
        let modules: Vec<_> =
            ty_module_resolver::all_modules(db, self.file.resolver_environment(db))
                .into_iter()
                .filter(|module| {
                    module
                        .search_path(db)
                        .is_some_and(ty_module_resolver::SearchPath::is_first_party)
                })
                .collect();
        type_hierarchy_subtypes(db, &env, self.ty, &modules)
            .into_iter()
            .filter_map(|class| {
                let file = ProgramFile::new(db, class.file.file(db), self.file.program(db));
                let parsed = parsed_module(db, file.python_file(db)).load(db);
                let def = all_classes(&parsed)
                    .into_iter()
                    .find(|def| def.name.range() == class.selection_range)?;
                let ty = def.inferred_type(&SemanticModel::new(db, file))?;
                Self::new(db, file, parsed, class.selection_range, ty)
            })
            .collect()
    }

    /// the project's classes below this one, at any depth
    fn descendants(&self, db: &'db dyn Db) -> Vec<ClassSite<'db>> {
        let mut found: Vec<ClassSite<'db>> = Vec::new();
        let mut pending = self.subclasses(db);
        while let Some(next) = pending.pop() {
            if found.iter().any(|seen| seen.same(&next)) {
                continue;
            }
            pending.extend(next.subclasses(db));
            found.push(next);
        }
        found
    }
}

/// every class definition of a module, at any depth
fn all_classes(parsed: &ParsedModuleRef) -> Vec<&ast::StmtClassDef> {
    #[derive(Default)]
    struct Classes<'a> {
        found: Vec<&'a ast::StmtClassDef>,
    }
    impl<'a> SourceOrderVisitor<'a> for Classes<'a> {
        fn visit_stmt(&mut self, statement: &'a Stmt) {
            if let Stmt::ClassDef(class) = statement {
                self.found.push(class);
            }
            source_order::walk_stmt(self, statement);
        }
        fn visit_expr(&mut self, _expression: &'a Expr) {}
    }
    let mut classes = Classes::default();
    classes.visit_body(parsed.suite());
    classes.found
}

/// the member a class body statement binds
fn member_name(statement: &Stmt) -> Option<&Name> {
    match statement {
        Stmt::FunctionDef(function) => Some(&function.name.id),
        Stmt::Assign(assign) => match assign.targets.as_slice() {
            [Expr::Name(name)] => Some(&name.id),
            _ => None,
        },
        Stmt::AnnAssign(assign) => match assign.target.as_ref() {
            Expr::Name(name) => Some(&name.id),
            _ => None,
        },
        _ => None,
    }
}

/// whether a class body statement binds an attribute rather than a method
fn is_attribute(statement: &Stmt) -> bool {
    !matches!(statement, Stmt::FunctionDef(_))
}

/// see [`ClassMember::detail`]. each parameter is written as the source writes it, but on one
/// line, with the `/` and `*` that separate the kinds
fn member_detail(source: &str, statement: &Stmt) -> String {
    let Stmt::FunctionDef(function) = statement else {
        return member_name(statement).map_or_else(String::new, ToString::to_string);
    };
    let written = |range: TextRange| {
        source[range]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let variadic = |star: &str, parameter: &ast::Parameter| {
        let text = written(parameter.range());
        if text.starts_with('*') {
            text
        } else {
            format!("{star}{text}")
        }
    };
    let parameters = &function.parameters;
    let mut parts: Vec<String> = Vec::new();
    parts.extend(parameters.posonlyargs.iter().map(|p| written(p.range())));
    if !parameters.posonlyargs.is_empty() {
        parts.push("/".to_string());
    }
    parts.extend(parameters.args.iter().map(|p| written(p.range())));
    match &parameters.vararg {
        Some(vararg) => parts.push(variadic("*", vararg)),
        None if !parameters.kwonlyargs.is_empty() => parts.push("*".to_string()),
        None => {}
    }
    parts.extend(parameters.kwonlyargs.iter().map(|p| written(p.range())));
    if let Some(kwarg) = &parameters.kwarg {
        parts.push(variadic("**", kwarg));
    }
    format!("{}({})", function.name, parts.join(", "))
}

/// what kind of member the class body statement `statement` of `model`'s file binds: a method
/// is a property where its decorators bind its name to one, and is passed what its decorators
/// say as the checker resolves them. `None` where the checker has no function for a `def`
fn member_kind(model: &SemanticModel<'_>, statement: &Stmt) -> Option<MemberKind> {
    let Stmt::FunctionDef(function) = statement else {
        return Some(MemberKind::Attribute);
    };
    if decorated_function(model.db(), function.definition(model))?.is_property() {
        return Some(MemberKind::Property);
    }
    Some(match resolved_method_kind(model, function)? {
        MethodKind::Instance => MemberKind::Method,
        MethodKind::Class => MemberKind::ClassMethod,
        MethodKind::Static => MemberKind::StaticMethod,
    })
}

fn unknown_kind(name: &str) -> String {
    format!("the checker cannot tell what kind of method `{name}` is")
}

fn member_name_range(statement: &Stmt) -> Option<TextRange> {
    match statement {
        Stmt::FunctionDef(function) => Some(function.name.range()),
        Stmt::Assign(assign) => assign.targets.first().map(Ranged::range),
        Stmt::AnnAssign(assign) => Some(assign.target.range()),
        _ => None,
    }
}

fn binds(statement: &Stmt, name: &str) -> bool {
    member_name(statement).is_some_and(|bound| bound.as_str() == name)
}

/// the indices of the statements of `class`'s body that bind `name`
fn member_statements(class: &ast::StmtClassDef, name: &str) -> Vec<usize> {
    class
        .body
        .iter()
        .enumerate()
        .filter(|(_, statement)| binds(statement, name))
        .map(|(index, _)| index)
        .collect()
}

fn refuse_mangled(name: &str) -> Result<(), String> {
    if name.starts_with("__") && !name.ends_with("__") {
        return Err(format!(
            "`{name}` is name-mangled: its name depends on the class it is written in"
        ));
    }
    Ok(())
}

/// whether a method of `class`, a class of `model`'s file, assigns `name` on its receiver:
/// `self.name = ...`
fn assigns_on_receiver(
    model: &SemanticModel<'_>,
    class: &ast::StmtClassDef,
    name: &str,
) -> Result<bool, String> {
    for statement in &class.body {
        if let Stmt::FunctionDef(function) = statement
            && let Some(receiver) = receiver_name(model, function)?
            && assigned_on_receiver(statement, receiver.as_str(), name)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn assigned_on_receiver(statement: &Stmt, receiver: &str, name: &str) -> bool {
    struct Stores<'a> {
        receiver: &'a str,
        name: &'a str,
        found: bool,
    }
    impl<'a> SourceOrderVisitor<'a> for Stores<'_> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Attribute(attribute) = expr
                && attribute.ctx.is_store()
                && attribute.attr.as_str() == self.name
                && matches!(&*attribute.value, Expr::Name(value) if value.id.as_str() == self.receiver)
            {
                self.found = true;
            }
            walk_expr(self, expr);
        }
    }
    let mut stores = Stores {
        receiver,
        name,
        found: false,
    };
    stores.visit_stmt(statement);
    stores.found
}

/// the name of the receiver parameter of `function`, a method of `model`'s file, `self` or
/// `cls`; `None` for a static method
fn receiver_name(
    model: &SemanticModel<'_>,
    function: &ast::StmtFunctionDef,
) -> Result<Option<Name>, String> {
    let kind = resolved_method_kind(model, function)
        .ok_or_else(|| unknown_kind(function.name.as_str()))?;
    if kind == MethodKind::Static {
        return Ok(None);
    }
    Ok(function
        .parameters
        .posonlyargs
        .iter()
        .chain(&function.parameters.args)
        .next()
        .map(|parameter| parameter.parameter.name.id.clone()))
}

/// the attributes a method of `model`'s file reads or writes on its receiver, `x` of `self.x`,
/// with where
fn receiver_attributes(
    model: &SemanticModel<'_>,
    statement: &Stmt,
) -> Result<Vec<(Name, TextRange)>, String> {
    struct Attributes<'a> {
        receiver: &'a str,
        found: Vec<(Name, TextRange)>,
    }
    impl<'a> SourceOrderVisitor<'a> for Attributes<'_> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Attribute(attribute) = expr
                && matches!(&*attribute.value, Expr::Name(value) if value.id.as_str() == self.receiver)
                && !self
                    .found
                    .iter()
                    .any(|(name, _)| name == attribute.attr.as_str())
            {
                self.found
                    .push((attribute.attr.id.clone(), attribute.range()));
            }
            walk_expr(self, expr);
        }
    }
    let Stmt::FunctionDef(function) = statement else {
        return Ok(Vec::new());
    };
    let Some(receiver) = receiver_name(model, function)? else {
        return Ok(Vec::new());
    };
    let mut attributes = Attributes {
        receiver: receiver.as_str(),
        found: Vec::new(),
    };
    attributes.visit_body(&function.body);
    Ok(attributes.found)
}

/// a name a member reads where its class body is evaluated: an attribute's annotation and value,
/// a method's decorators, defaults and annotations, a lambda's defaults and a comprehension's
/// first iterable
enum BodyRead {
    /// a name the class body binds, bound by the member statement `index` where one binds it
    Member { name: Name, index: Option<usize> },
    /// a name the class body does not bind, and the scope it resolves to from there; `None` for
    /// a builtin or a name nothing binds
    Outside {
        name: Name,
        resolves_to: Option<FileScopeId>,
    },
}

fn class_body_reads(
    db: &dyn Db,
    class: &ClassSite<'_>,
    def: &ast::StmtClassDef,
    statement: &Stmt,
) -> Vec<BodyRead> {
    #[derive(Default)]
    struct Loads<'a> {
        found: Vec<&'a ast::ExprName>,
    }
    impl<'a> SourceOrderVisitor<'a> for Loads<'a> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            match expr {
                Expr::Name(name) if name.ctx.is_load() => self.found.push(name),
                // what is evaluated later, in a scope of its own, is not read in the class body
                Expr::Lambda(lambda) => {
                    if let Some(parameters) = &lambda.parameters {
                        for default in parameters.iter().filter_map(ast::AnyParameterRef::default) {
                            self.visit_expr(default);
                        }
                    }
                }
                Expr::ListComp(ast::ExprListComp { generators, .. })
                | Expr::SetComp(ast::ExprSetComp { generators, .. })
                | Expr::Generator(ast::ExprGenerator { generators, .. }) => {
                    if let Some(first) = generators.first() {
                        self.visit_expr(&first.iter);
                    }
                }
                Expr::DictComp(ast::ExprDictComp { generators, .. }) => {
                    if let Some(first) = generators.first() {
                        self.visit_expr(&first.iter);
                    }
                }
                _ => walk_expr(self, expr),
            }
        }
    }
    let mut loads = Loads::default();
    match statement {
        Stmt::Assign(assign) => loads.visit_expr(&assign.value),
        Stmt::AnnAssign(assign) => {
            loads.visit_expr(&assign.annotation);
            if let Some(value) = &assign.value {
                loads.visit_expr(value);
            }
        }
        Stmt::FunctionDef(function) => {
            for decorator in &function.decorator_list {
                loads.visit_expr(&decorator.expression);
            }
            for parameter in &function.parameters {
                if let Some(default) = parameter.default() {
                    loads.visit_expr(default);
                }
                if let Some(annotation) = parameter.annotation() {
                    loads.visit_expr(annotation);
                }
            }
            if let Some(returns) = &function.returns {
                loads.visit_expr(returns);
            }
        }
        _ => {}
    }
    let index = semantic_index(db, class.file);
    let body_scope = index.try_node_scope(NodeWithScopeRef::Class(def));
    loads
        .found
        .into_iter()
        .filter_map(|load| {
            let scope = index.try_expression_scope_id(&ast::ExprRef::Name(load))?;
            let resolves_to = resolving_scope(index, scope, load.id.as_str());
            Some(if resolves_to.is_some() && resolves_to == body_scope {
                BodyRead::Member {
                    name: load.id.clone(),
                    index: member_statements(def, load.id.as_str()).first().copied(),
                }
            } else {
                BodyRead::Outside {
                    name: load.id.clone(),
                    resolves_to,
                }
            })
        })
        .collect()
}

/// whether `scope` binds `name` after the statement of `first` begins and before the statement of
/// `second`, where a class body run between them would read another value
fn bound_between(
    db: &dyn Db,
    class: &ClassSite<'_>,
    scope: FileScopeId,
    name: &str,
    first: &ast::StmtClassDef,
    second: &ast::StmtClassDef,
) -> bool {
    let index = semantic_index(db, class.file);
    let Some(symbol) = index.place_table(scope).symbol_id(name) else {
        return false;
    };
    let in_stub = class.file_id.is_stub(db);
    let between = TextRange::new(first.start(), second.start().max(first.start()));
    bindings_of_symbol(db, index, &class.parsed, scope, symbol, in_stub)
        .into_iter()
        .any(|definition| between.contains_range(definition.focus_range(db, &class.parsed).range()))
}

/// whether a member of `model`'s file reads the builtin `super`, under whatever name it is bound
/// to, or basedpython's `super` keyword: either names the class the member is written in, the
/// one by its `__class__` cell, the other by the class it is passed
fn uses_super(model: &SemanticModel<'_>, statement: &Stmt) -> bool {
    struct Super<'m, 'db> {
        model: &'m SemanticModel<'db>,
        found: bool,
    }
    impl<'a> SourceOrderVisitor<'a> for Super<'_, '_> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Name(name) = expr
                && name.ctx.is_load()
                && name
                    .inferred_type(self.model)
                    .is_some_and(|ty| is_super(self.model.db(), ty))
            {
                self.found = true;
            }
            walk_expr(self, expr);
        }
    }
    let mut uses = Super {
        model,
        found: false,
    };
    uses.visit_stmt(statement);
    uses.found
}

/// a name `statement`, a member of the class `def`, reads from around the class that names
/// something else around `target`'s class `target_def`, where the member would read it: a scope
/// around one binds it and the same scope is not around the other, or the class body of
/// `target_def` binds what the class body of `def` reads from around it. a name both read from
/// their module's top level, or that no scope binds, is for [`checked_dependencies`] to compare
/// between modules
fn read_rebound_around(
    db: &dyn Db,
    class: &ClassSite<'_>,
    def: &ast::StmtClassDef,
    statement: &Stmt,
    target: &ClassSite<'_>,
    target_def: &ast::StmtClassDef,
) -> Option<Name> {
    let index = semantic_index(db, class.file);
    let target_index = semantic_index(db, target.file);
    let body = index.try_node_scope(NodeWithScopeRef::Class(def))?;
    let target_body = target_index.try_node_scope(NodeWithScopeRef::Class(target_def))?;
    let around: FxHashSet<FileScopeId> = index
        .ancestor_scopes(body)
        .map(|(scope, _)| scope)
        .filter(|scope| *scope != body)
        .collect();
    let in_module = |scope: Option<FileScopeId>| scope.is_none_or(FileScopeId::is_global);
    loads(statement).into_iter().find_map(|load| {
        let scope = index.try_expression_scope_id(&ast::ExprRef::Name(load))?;
        let name = load.id.as_str();
        let seen: Vec<FileScopeId> = index
            .visible_ancestor_scopes(scope)
            .map(|(scope, _)| scope)
            .collect();
        let resolves_to = resolving_scope_among(index, seen.iter().copied(), name);
        // what the member binds itself moves with it, and what the class body binds is a sibling
        if resolves_to.is_some_and(|scope| !around.contains(&scope)) {
            return None;
        }
        // what a method reads skips its class body, and what the class body evaluates does not
        let sees_body = seen.contains(&body);
        let target_resolves_to = resolving_scope_among(
            target_index,
            target_index
                .visible_ancestor_scopes(target_body)
                .map(|(scope, _)| scope)
                .skip(usize::from(!sees_body)),
            name,
        );
        let rebound = if class.file_id == target.file_id {
            resolves_to != target_resolves_to
        } else {
            !in_module(resolves_to) || !in_module(target_resolves_to)
        };
        rebound.then(|| load.id.clone())
    })
}

/// whether a member of `model`'s file reads the `__class__` cell python gives a method, which is
/// the class the method is written in, wherever it is called from
fn member_reads_class_cell(model: &SemanticModel<'_>, statement: &Stmt) -> bool {
    struct ClassCell<'m, 'db> {
        model: &'m SemanticModel<'db>,
        found: bool,
    }
    impl<'a> SourceOrderVisitor<'a> for ClassCell<'_, '_> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Name(name) = expr
                && name.ctx.is_load()
                && reads_class_cell(self.model, name)
            {
                self.found = true;
            }
            walk_expr(self, expr);
        }
    }
    let mut reads = ClassCell {
        model,
        found: false,
    };
    reads.visit_stmt(statement);
    reads.found
}

fn class_cell_refusal(member: &str, to: &str, from: &str) -> String {
    format!("`{member}` reads `__class__`, which would be {to} rather than {from}")
}

/// the `override` a method of `class` is marked with: basedpython's modifier, or a decorator
/// that is `typing.override`
fn override_marker<'a>(
    db: &dyn Db,
    class: &ClassSite<'_>,
    statement: &'a Stmt,
) -> Option<&'a ast::Decorator> {
    let Stmt::FunctionDef(function) = statement else {
        return None;
    };
    let model = SemanticModel::new(db, class.file);
    function
        .decorator_list
        .iter()
        .find(|decorator| match &decorator.expression {
            // the modifier is parsed into a decorator named for it, which no source spells, so
            // it is told apart from a real name by its context
            Expr::Name(name) if name.ctx == ExprContext::Invalid => name.id.as_str() == "override",
            expression => expression
                .inferred_type(&model)
                .is_some_and(|ty| is_override_function(db, ty)),
        })
}

/// the range that takes `decorator` out: an `@override` line with its line break, or the
/// `override` modifier keyword with the space after it
fn override_range(source: &str, decorator: &ast::Decorator) -> TextRange {
    if source[decorator.range()].starts_with('@') {
        TextRange::new(
            source.line_start(decorator.start()),
            source.full_line_end(decorator.end()),
        )
    } else {
        let spaces = source[decorator.end().to_usize()..]
            .chars()
            .take_while(|character| *character == ' ')
            .count();
        TextRange::new(
            decorator.start(),
            decorator.end() + TextSize::try_from(spaces).unwrap_or_default(),
        )
    }
}

/// the text of the body statement `index`, as [`cut_range`] cuts it, without the `override`
/// `strip_override` when there is one
fn member_text(
    source: &str,
    body: &[Stmt],
    index: usize,
    strip_override: Option<&ast::Decorator>,
) -> String {
    let cut = cut_range(source, body, index);
    if let Some(decorator) = strip_override {
        let local = override_range(source, decorator) - cut.start;
        let mut text = cut.text;
        text.replace_range(local.to_std_range(), "");
        return text;
    }
    cut.text
}

/// the indentation the statements of `class`'s body are written with
fn body_indentation(source: &str, class: &ast::StmtClassDef) -> String {
    class.body.first().map_or_else(
        || "    ".to_string(),
        |statement| {
            let start = source.line_start(statement.start());
            source[TextRange::new(start, statement.start())].to_string()
        },
    )
}

/// `text` with each line's leading `from` replaced by `to`
fn reindent(text: &str, from: &str, to: &str) -> String {
    if from == to {
        return text.to_string();
    }
    text.split_inclusive('\n')
        .map(|line| match line.strip_prefix(from) {
            Some(rest) => format!("{to}{rest}"),
            None => line.to_string(),
        })
        .collect()
}

/// whether a body statement stands in for no body: `pass` or `...`
fn is_placeholder(statement: &Stmt) -> bool {
    match statement {
        Stmt::Pass(_) => true,
        Stmt::Expr(expression) => matches!(*expression.value, Expr::EllipsisLiteral(_)),
        _ => false,
    }
}

fn is_docstring(statement: &Stmt) -> bool {
    matches!(statement, Stmt::Expr(expression) if expression.value.is_string_literal_expr())
}

/// the edits taking the statements `removed` out of `class`'s body, leaving `pass` when nothing
/// but a docstring would be left
fn remove_members(
    file: File,
    source: &str,
    class: &ast::StmtClassDef,
    removed: &[usize],
) -> Vec<FileEdit> {
    let left = class
        .body
        .iter()
        .enumerate()
        .filter(|(index, statement)| !removed.contains(index) && !is_docstring(statement))
        .count();
    let mut edits = Vec::new();
    let mut ranges: Vec<TextRange> = removed
        .iter()
        .map(|&index| cut_range(source, &class.body, index).removed)
        .collect();
    ranges.sort_by_key(Ranged::start);
    // two adjacent cuts that would overlap at a blank line are one
    let mut merged: Vec<TextRange> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if last.end() >= range.start() => *last = last.cover(range),
            _ => merged.push(range),
        }
    }
    // cuts that end the body take the blank lines above them, as the last one alone does
    let body_end = class.body.last().map_or(class.end(), Ranged::end);
    for range in &mut merged {
        if range.end() >= body_end {
            let mut start = range.start();
            while start > class.start() {
                let previous = source.line_start(start - TextSize::from(1));
                if !source[TextRange::new(previous, start)].trim().is_empty() {
                    break;
                }
                start = previous;
            }
            *range = TextRange::new(start, range.end());
        }
    }
    let indentation = body_indentation(source, class);
    for (position, range) in merged.into_iter().enumerate() {
        let replacement = if left == 0 && position == 0 {
            format!("{indentation}pass\n")
        } else {
            String::new()
        };
        // the last member's cut takes the blank lines above it; a `pass` needs only its line
        let range = if replacement.is_empty() {
            range
        } else {
            let first = removed.iter().min().copied().unwrap_or_default();
            TextRange::new(
                source.line_start(class.body[first].start()),
                source
                    .full_line_end(range.end() - TextSize::from(1))
                    .min(range.end()),
            )
        };
        edits.push(FileEdit {
            file,
            edit: if replacement.is_empty() {
                Edit::range_deletion(range)
            } else {
                Edit::range_replacement(replacement, range)
            },
        });
    }
    edits
}

/// the edits adding `members` to `class`'s body: an attribute after its last attribute, before
/// its first method; a method after its last statement, a blank line away; in place of a body
/// that is only `pass` or `...`
fn insert_members(
    file: File,
    source: &str,
    class: &ast::StmtClassDef,
    members: &[(MemberKind, String)],
    line_ending: &str,
) -> Vec<FileEdit> {
    // a pull of bases alone moves no member, and the superclass's body stays as it is: its
    // placeholder too, which replaced with nothing would leave the class without a body
    if members.is_empty() {
        return Vec::new();
    }
    let attributes: Vec<&str> = members
        .iter()
        .filter(|(kind, _)| *kind == MemberKind::Attribute)
        .map(|(_, text)| text.as_str())
        .collect();
    let methods: Vec<&str> = members
        .iter()
        .filter(|(kind, _)| *kind != MemberKind::Attribute)
        .map(|(_, text)| text.as_str())
        .collect();
    let placeholder = class
        .body
        .iter()
        .find(|statement| is_placeholder(statement))
        .filter(|_| {
            class
                .body
                .iter()
                .all(|statement| is_placeholder(statement) || is_docstring(statement))
        });
    if let Some(placeholder) = placeholder {
        let mut text = attributes.concat();
        if !attributes.is_empty() && !methods.is_empty() {
            text.push_str(line_ending);
        }
        text.push_str(&methods.join(line_ending));
        return vec![FileEdit {
            file,
            edit: Edit::range_replacement(
                text,
                TextRange::new(
                    source.line_start(placeholder.start()),
                    source.full_line_end(placeholder.end()),
                ),
            ),
        }];
    }

    let mut edits = Vec::new();
    if !attributes.is_empty() {
        let first_method = class
            .body
            .iter()
            .find(|statement| matches!(statement, Stmt::FunctionDef(_) | Stmt::ClassDef(_)));
        let (offset, text) = match first_method {
            Some(method) => {
                let mut text = attributes.concat();
                let previous_is_attribute = class
                    .body
                    .iter()
                    .take_while(|statement| !std::ptr::eq(*statement, method))
                    .any(|statement| !is_docstring(statement));
                if !previous_is_attribute {
                    text.push_str(line_ending);
                }
                (source.line_start(method.start()), text)
            }
            None => {
                let last = class.body.last().map_or(class.end(), Ranged::end);
                (source.full_line_end(last), attributes.concat())
            }
        };
        edits.push(FileEdit {
            file,
            edit: Edit::insertion(text, offset),
        });
    }
    if !methods.is_empty() {
        let last = class.body.last().map_or(class.end(), Ranged::end);
        let offset = source.full_line_end(last);
        let mut text = String::new();
        if !source[..offset.to_usize()].ends_with(['\n', '\r']) {
            text.push_str(line_ending);
        }
        for method in methods {
            text.push_str(line_ending);
            text.push_str(method);
        }
        edits.push(FileEdit {
            file,
            edit: Edit::insertion(text, offset),
        });
    }
    edits
}

/// the edits importing `requests` into `class`'s module, each under the name the moved code
/// reads it by
fn add_imports(
    db: &dyn Db,
    class: &ClassSite<'_>,
    requests: &[OwnedRequest],
) -> Result<Vec<FileEdit>, String> {
    let importer = import_order::importer(db, class.file, &class.parsed);
    let members = MembersInScope::empty(class.name_range.start());
    let mut actions: Vec<ImportAction> = Vec::new();
    for request in requests {
        let action = importer.import(request.request(), &members);
        if action.symbol_text() != request.binds {
            return Err(format!(
                "the moved code reads `{}`, which {} cannot import under that name",
                request.binds,
                class.module(db).unwrap_or_default()
            ));
        }
        actions.push(action);
    }
    Ok(importer
        .combined_edits(&actions)
        .into_iter()
        .map(|edit| FileEdit {
            file: class.file.file(db),
            edit,
        })
        .collect())
}

/// the edits marking `statement`, a method of `class`, as an override: basedpython's `override`
/// modifier, or python's `@override` with its import
fn add_override(db: &dyn Db, class: &ClassSite<'_>, statement: &Stmt) -> Vec<FileEdit> {
    let Stmt::FunctionDef(function) = statement else {
        return Vec::new();
    };
    let file = class.file.file(db);
    let source = source_text(db, file);
    let source = source.as_str();
    let header = source.line_start(function.name.start());
    let indentation = &source[TextRange::new(
        header,
        header
            + TextSize::try_from(
                source[header.to_usize()..].len() - source[header.to_usize()..].trim_start().len(),
            )
            .unwrap_or_default(),
    )];
    if file.source_type(db).is_basedpython() {
        let at = header + TextSize::of(indentation);
        return vec![FileEdit {
            file,
            edit: Edit::insertion("override ".to_string(), at),
        }];
    }
    let module = if class.file.python_version(db) >= ruff_python_ast::PythonVersion::PY312 {
        "typing"
    } else {
        "typing_extensions"
    };
    let stylist = Stylist::from_tokens(class.parsed.tokens(), source);
    let importer = import_order::importer(db, class.file, &class.parsed);
    let action = importer.import(
        ImportRequest::import_from(module, "override"),
        &MembersInScope::empty(statement.start()),
    );
    let first_line = source.line_start(statement.start());
    let mut edits = vec![FileEdit {
        file,
        edit: Edit::insertion(
            format!(
                "{indentation}@{}{}",
                action.symbol_text(),
                stylist.line_ending().as_str()
            ),
            first_line,
        ),
    }];
    edits.extend(
        importer
            .combined_edits([&action])
            .into_iter()
            .map(|edit| FileEdit { file, edit }),
    );
    edits
}

/// the edit taking the `override` off `statement`, a method of `class`
fn remove_override(db: &dyn Db, class: &ClassSite<'_>, statement: &Stmt) -> Vec<FileEdit> {
    let Some(decorator) = override_marker(db, class, statement) else {
        return Vec::new();
    };
    let source = source_text(db, class.file_id);
    vec![FileEdit {
        file: class.file_id,
        edit: Edit::range_deletion(override_range(source.as_str(), decorator)),
    }]
}

/// the type of the receiver of the attribute whose name is at `range` in `file`: `x` of `x.name`
fn attribute_receiver_type<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    range: TextRange,
) -> Option<Type<'db>> {
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let covering = ruff_python_ast::find_node::covering_node(parsed.syntax().into(), range);
    let attribute = covering.ancestors().find_map(|node| match node {
        ast::AnyNodeRef::ExprAttribute(attribute) if attribute.attr.range() == range => {
            Some(attribute)
        }
        _ => None,
    })?;
    attribute.value.inferred_type(&SemanticModel::new(db, file))
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use super::{extract_superclass, movable_members, pull_members_up, push_members_down};
    use crate::refactor::move_symbol::tests::rendered;
    use crate::tests::{CursorTest, CursorTestBuilder};

    const SHAPES: &str = r#"class Base:
    def keep(self):
        return 1


class Child(Base):
    LIMIT = 3

    def __init__(self, x):
        self.x = x

    def area(self):
        return self.x * self.LIMIT

    def plain(self):
        return 2

    def helper(self):
        return self.plain() + 1


class Other(Base):
    pass


class GrandChild(Child):
    def area(self):
        return 0
"#;

    fn project(shapes: &str) -> CursorTest {
        CursorTestBuilder::default()
            .source("cls/__init__.py", "")
            .source("cls/shapes.py", shapes)
            .source(
                "cls/user.py",
                "from cls.shapes import Base, Child\n\nprint(Child(1).area(), Base().keep())\n",
            )
            .build()
    }

    impl CursorTest {
        fn pull_up(&self, names: &[&str], target: &str) -> String {
            self.pull_up_with_bases(names, &[], target)
        }

        fn pull_up_with_bases(&self, names: &[&str], bases: &[&str], target: &str) -> String {
            let names: Vec<String> = names.iter().map(ToString::to_string).collect();
            let bases: Vec<String> = bases.iter().map(ToString::to_string).collect();
            let moved = salsa::attach(&self.db, || {
                pull_members_up(
                    &self.db,
                    self.program_file(self.cursor.file),
                    self.cursor.offset,
                    &names,
                    &bases,
                    target,
                )
            });
            rendered(&self.db, moved)
        }

        fn push_down(&self, names: &[&str]) -> String {
            let names: Vec<String> = names.iter().map(ToString::to_string).collect();
            let moved = salsa::attach(&self.db, || {
                push_members_down(
                    &self.db,
                    self.program_file(self.cursor.file),
                    self.cursor.offset,
                    &names,
                )
            });
            rendered(&self.db, moved)
        }

        fn movable(&self) -> String {
            let movable = salsa::attach(&self.db, || {
                movable_members(
                    &self.db,
                    self.program_file(self.cursor.file),
                    self.cursor.offset,
                )
            });
            let Some(movable) = movable else {
                return "nothing".to_string();
            };
            format!(
                "{}\nmembers: {}\nsuperclasses: {}\nsubclasses: {}",
                movable.class.qualified_name.unwrap_or_default(),
                movable
                    .members
                    .iter()
                    .map(|member| format!("{} ({:?})", member.detail, member.kind))
                    .collect::<Vec<_>>()
                    .join(", "),
                movable
                    .superclasses
                    .iter()
                    .filter_map(|class| class.qualified_name.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
                movable
                    .subclasses
                    .iter()
                    .filter_map(|class| class.qualified_name.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        }
    }

    #[test]
    fn what_can_move_where() {
        let test = project(&SHAPES.replace("class Child(Base)", "class <CURSOR>Child(Base)"));
        assert_snapshot!(test.movable(), @"
        cls.shapes.Child
        members: LIMIT (Attribute), __init__(self, x) (Method), area(self) (Method), plain(self) (Method), helper(self) (Method), Base (Base)
        superclasses: cls.shapes.Base
        subclasses: cls.shapes.GrandChild
        ");
    }

    /// a method reads as its parameters, on one line whatever lines they are written on
    #[test]
    fn a_member_reads_as_its_name_and_parameters() {
        let test = CursorTestBuilder::default()
            .source(
                "main.py",
                r#"class <CURSOR>Store:
    SIZE: int = 3

    def fetch(self, key: str, /, *args,
              default=None, **options):
        return key

    def only(self, *, strict: bool = True):
        return strict

    @property
    def size(self):
        return self.SIZE
"#,
            )
            .build();
        assert_snapshot!(test.movable(), @"
        main.Store
        members: SIZE (Attribute), fetch(self, key: str, /, *args, default=None, **options) (Method), only(self, *, strict: bool = True) (Method), size(self) (Property)
        superclasses: 
        subclasses:
        ");
    }

    fn mixins(source: &str) -> CursorTest {
        CursorTestBuilder::default()
            .source("main.py", source)
            .build()
    }

    /// a base the class writes is pulled up into the superclass's bases, after its own, where
    /// every class below the superclass finds what it found before
    #[test]
    fn a_base_is_pulled_up_into_the_superclass() {
        let test = mixins(
            r#"class Mixin:
    def mix(self):
        return 1


class Base:
    def keep(self):
        return 1


class <CURSOR>Child(Base, Mixin, metaclass=type):
    def own(self):
        return self.mix()
"#,
        );
        assert_snapshot!(test.movable(), @"
        main.Child
        members: own(self) (Method), Base (Base), Mixin (Base)
        superclasses: main.Base, main.Mixin
        subclasses:
        ");
        assert_snapshot!(test.pull_up_with_bases(&["own"], &["Mixin"], "main.Base"), @"
        --- /main.py
        class Mixin:
            def mix(self):
                return 1


        class Base(Mixin):
            def keep(self):
                return 1

            def own(self):
                return self.mix()


        class Child(Base, metaclass=type):
            pass
        ");
    }

    /// bases pulled up on their own move no member, and leave a superclass whose body is only
    /// `pass` as it is
    #[test]
    fn a_base_alone_is_pulled_up_into_a_superclass_with_an_empty_body() {
        let test = mixins(
            r#"class Mixin:
    def mix(self):
        return 1


class Base:
    pass


class <CURSOR>Child(Base, Mixin):
    def own(self):
        return self.mix()
"#,
        );
        assert_snapshot!(test.pull_up_with_bases(&[], &["Mixin"], "main.Base"), @"
        --- /main.py
        class Mixin:
            def mix(self):
                return 1


        class Base(Mixin):
            pass


        class Child(Base):
            def own(self):
                return self.mix()
        ");
    }

    /// whatever a class below the superclass finds, it has to find in the same class afterwards
    #[test]
    fn a_base_that_changes_what_a_class_finds_is_refused() {
        let test = mixins(
            r#"class Mixin:
    def keep(self):
        return 2


class Base:
    def keep(self):
        return 1


class <CURSOR>Child(Mixin, Base):
    pass
"#,
        );
        assert_snapshot!(test.pull_up_with_bases(&[], &["Mixin"], "main.Base"), @"refused: Child would find `keep` in another class than it does");

        let test = mixins(
            r#"class Mixin:
    def shared(self):
        return 2


class Other:
    def shared(self):
        return 3


class Base:
    pass


class <CURSOR>Child(Base, Mixin):
    pass


class Sibling(Base, Other):
    pass
"#,
        );
        assert_snapshot!(test.pull_up_with_bases(&[], &["Mixin"], "main.Base"), @"refused: Sibling would find `shared` in another class than it does");
    }

    /// the superclass names the base where it is defined, so the name has to be bound there, and
    /// the class cannot lose the base it reaches the superclass through
    #[test]
    fn a_base_the_superclass_cannot_take_is_refused() {
        let test = mixins(
            r#"class Base:
    pass


class Mixin:
    pass


class <CURSOR>Child(Base, Mixin):
    pass
"#,
        );
        assert_snapshot!(test.pull_up_with_bases(&[], &["Mixin"], "main.Base"), @"refused: `Mixin` is not bound above Base, where it would be written");

        let test = mixins(
            r#"class Base:
    pass


class Middle(Base):
    pass


class <CURSOR>Child(Middle):
    pass
"#,
        );
        assert_snapshot!(test.pull_up_with_bases(&[], &["Middle"], "main.Base"), @"refused: Child inherits Base through `Middle`, so it cannot lose it");
    }

    #[test]
    fn a_method_that_needs_nothing_else_is_pulled_up() {
        let test = project(&SHAPES.replace("class Child(Base)", "class <CURSOR>Child(Base)"));
        assert_snapshot!(test.pull_up(&["plain"], "cls.shapes.Base"), @"
        --- /cls/shapes.py
        class Base:
            def keep(self):
                return 1

            def plain(self):
                return 2


        class Child(Base):
            LIMIT = 3

            def __init__(self, x):
                self.x = x

            def area(self):
                return self.x * self.LIMIT

            def helper(self):
                return self.plain() + 1


        class Other(Base):
            pass


        class GrandChild(Child):
            def area(self):
                return 0
        ");
    }

    /// `area` reads `self.x`, which only a `Child` has; `Other` would inherit an `area` that
    /// fails
    #[test]
    fn a_method_that_reads_what_the_superclass_lacks_is_refused() {
        let test = project(&SHAPES.replace("class Child(Base)", "class <CURSOR>Child(Base)"));
        assert_snapshot!(test.pull_up(&["area"], "cls.shapes.Base"), @"refused: `area` uses `self.x` on line 13, which Base does not have");
        assert_snapshot!(test.pull_up(&["helper"], "cls.shapes.Base"), @"refused: `helper` uses `self.plain` on line 19, which Base does not have; pull it up too");
    }

    /// pulled up together, `helper` finds the `plain` it calls
    #[test]
    fn members_pulled_up_together_satisfy_each_other() {
        let test = project(&SHAPES.replace("class Child(Base)", "class <CURSOR>Child(Base)"));
        assert_snapshot!(test.pull_up(&["helper", "plain", "LIMIT"], "cls.shapes.Base"), @"
        --- /cls/shapes.py
        class Base:
            LIMIT = 3

            def keep(self):
                return 1

            def plain(self):
                return 2

            def helper(self):
                return self.plain() + 1


        class Child(Base):
            def __init__(self, x):
                self.x = x

            def area(self):
                return self.x * self.LIMIT


        class Other(Base):
            pass


        class GrandChild(Child):
            def area(self):
                return 0
        ");
    }

    /// `Base().keep()` would no longer find `keep`
    #[test]
    fn a_member_read_off_the_class_itself_is_not_pushed_down() {
        let test = project(&SHAPES.replace("class Base:", "class <CURSOR>Base:"));
        assert_snapshot!(test.push_down(&["keep"]), @"refused: cls/user.py:3 uses `keep` on what may be a Base rather than a subclass");
    }

    /// `helper` calls `self.plain()` on what may be a `Child` rather than a `GrandChild`
    #[test]
    fn a_member_the_class_s_own_methods_read_is_not_pushed_down() {
        let test = project(&SHAPES.replace("class Child(Base)", "class <CURSOR>Child(Base)"));
        assert_snapshot!(test.push_down(&["plain"]), @"refused: cls/shapes.py:19 uses `plain` on what may be a Child rather than a subclass");
    }

    #[test]
    fn a_member_nothing_reads_on_the_class_is_pushed_down() {
        let test = CursorTestBuilder::default()
            .source(
                "main.py",
                r#"class <CURSOR>Base:
    LIMIT = 3

    def keep(self):
        return self.LIMIT


class Child(Base):
    pass


class Other(Base):
    def keep(self):
        return 2


def use(child: Child, other: Other):
    return child.keep() + other.keep()
"#,
            )
            .build();
        assert_snapshot!(test.push_down(&["keep"]), @"
        --- /main.py
        class Base:
            LIMIT = 3


        class Child(Base):
            def keep(self):
                return self.LIMIT


        class Other(Base):
            def keep(self):
                return 2


        def use(child: Child, other: Other):
            return child.keep() + other.keep()
        ");
    }

    /// pulled up where nothing above has the name, a method overrides nothing, and what has
    /// the name below the superclass then overrides it: in basedpython it gains the modifier
    #[test]
    fn override_follows_a_pull_up() {
        let test = CursorTestBuilder::default()
            .source(
                "main.by",
                r#"class Base:
    pass


class <CURSOR>Child(Base):
    def describe(self) -> str:
        return "child"


class Other(Base):
    def describe(self) -> str:
        return "other"


class Leaf(Child):
    override def describe(self) -> str:
        return "leaf"
"#,
            )
            .build();
        assert_snapshot!(test.pull_up(&["describe"], "main.Base"), @r#"
        --- /main.by
        class Base:
            def describe(self) -> str:
                return "child"


        class Child(Base):
            pass


        class Other(Base):
            override def describe(self) -> str:
                return "other"


        class Leaf(Child):
            override def describe(self) -> str:
                return "leaf"
        "#);
    }

    /// in python it gains `@override`, imported
    #[test]
    fn override_follows_a_pull_up_in_python() {
        let test = CursorTestBuilder::default()
            .source(
                "main.py",
                r#"class Base:
    pass


class <CURSOR>Child(Base):
    def describe(self) -> str:
        return "child"


class Other(Base):
    def describe(self) -> str:
        return "other"
"#,
            )
            .build();
        assert_snapshot!(test.pull_up(&["describe"], "main.Base"), @r#"
        --- /main.py
        from typing import override


        class Base:
            def describe(self) -> str:
                return "child"


        class Child(Base):
            pass


        class Other(Base):
            @override
            def describe(self) -> str:
                return "other"
        "#);
    }

    /// `Leaf` would inherit `Mid`'s `describe` instead of the one it pulls up
    #[test]
    fn a_class_between_with_the_name_is_refused() {
        let test = CursorTestBuilder::default()
            .source(
                "main.py",
                r#"class Base:
    pass


class Mid(Base):
    def describe(self) -> str:
        return "mid"


class <CURSOR>Leaf(Mid):
    def describe(self) -> str:
        return "leaf"
"#,
            )
            .build();
        assert_snapshot!(test.pull_up(&["describe"], "main.Base"), @"refused: Leaf would inherit Mid's `describe` rather than its own");
    }
    impl CursorTest {
        fn extract(&self, names: &[&str], name: &str) -> String {
            let names: Vec<String> = names.iter().map(ToString::to_string).collect();
            let moved = salsa::attach(&self.db, || {
                extract_superclass(
                    &self.db,
                    self.program_file(self.cursor.file),
                    self.cursor.offset,
                    &names,
                    name,
                )
            });
            rendered(&self.db, moved)
        }
    }

    /// the new class takes the class's bases and sits right after it in its MRO, so what the
    /// moved members find is what they found
    #[test]
    fn extracting_a_superclass_inserts_it_between_the_class_and_its_bases() {
        let test = project(&SHAPES.replace("class Child(Base)", "class <CURSOR>Child(Base)"));
        assert_snapshot!(test.extract(&["plain", "LIMIT"], "Shape"), @"
        --- /cls/shapes.py
        class Base:
            def keep(self):
                return 1


        class Shape(Base):
            LIMIT = 3

            def plain(self):
                return 2


        class Child(Shape):
            def __init__(self, x):
                self.x = x

            def area(self):
                return self.x * self.LIMIT

            def helper(self):
                return self.plain() + 1


        class Other(Base):
            pass


        class GrandChild(Child):
            def area(self):
                return 0
        ");
    }

    /// the new class's name has to be one a class statement can be written with, and one the
    /// module does not bind already, to a class or to anything else
    #[test]
    fn extracting_under_a_name_a_class_cannot_have_is_refused() {
        let test = project(&SHAPES.replace("class Child(Base)", "class <CURSOR>Child(Base)"));
        assert_snapshot!(test.extract(&["plain"], "1Shape"), @"refused: `1Shape` is not a name a class can have");
        assert_snapshot!(test.extract(&["plain"], "My Shape"), @"refused: `My Shape` is not a name a class can have");
        assert_snapshot!(test.extract(&["plain"], ""), @"refused: `` is not a name a class can have");
        assert_snapshot!(test.extract(&["plain"], "class"), @"refused: `class` is not a name a class can have");
        assert_snapshot!(test.extract(&["plain"], "Base"), @"refused: cls.shapes has a `Base` already");
    }

    #[test]
    fn extracting_what_reads_an_attribute_left_behind_is_refused() {
        let test = project(&SHAPES.replace("class Child(Base)", "class <CURSOR>Child(Base)"));
        assert_snapshot!(test.extract(&["area"], "Shape"), @"refused: `area` uses `self.x` on line 13, which Shape would not have");
        assert_snapshot!(test.extract(&["plain"], "Base"), @"refused: cls.shapes has a `Base` already");
    }

    #[test]
    fn a_class_with_a_special_base_is_refused() {
        let test = CursorTestBuilder::default()
            .source(
                "main.py",
                "from typing import NamedTuple\n\n\nclass <CURSOR>Point(NamedTuple):\n    x: int\n    y: int\n",
            )
            .build();
        assert_snapshot!(test.extract(&["x"], "Base"), @"refused: Point has a base whose subclasses python treats specially, which Base would take in its place");
    }

    /// a default, a decorator and a comprehension's first iterable are evaluated in the class
    /// body, so what they read there has to move with the member
    #[test]
    fn a_member_whose_class_body_reads_stay_behind_is_refused() {
        let test = mixins(
            r#"class Base:
    pass


class <CURSOR>Child(Base):
    LIMIT = 3
    RAW = ["a", "b"]

    def tag(function):
        return function

    def clamp(self, x, limit=LIMIT):
        return min(x, limit)

    @tag
    def run(self):
        return 1

    NAMES = [name.upper() for name in RAW]
"#,
        );
        assert_snapshot!(test.pull_up(&["clamp"], "main.Base"), @"refused: `clamp` reads Child's `LIMIT`, which it would no longer read in Base's body; pull it up too");
        assert_snapshot!(test.pull_up(&["run"], "main.Base"), @"refused: `run` reads Child's `tag`, which it would no longer read in Base's body; pull it up too");
        assert_snapshot!(test.pull_up(&["NAMES"], "main.Base"), @"refused: `NAMES` reads Child's `RAW`, which it would no longer read in Base's body; pull it up too");
        assert_snapshot!(test.extract(&["clamp"], "Shape"), @"refused: `clamp` reads Child's `LIMIT`, which it would no longer read in Shape's body; extract it too");
    }

    /// `Base`'s own `LIMIT` is not what `Child`'s body reads: a class body does not see another
    /// class's members, and `SIZE` would read `Base.LIMIT` once it is written in `Base`
    #[test]
    fn a_member_that_reads_a_sibling_the_superclass_also_has_is_refused() {
        let test = mixins(
            r#"class Base:
    LIMIT = 1


class <CURSOR>Child(Base):
    LIMIT = 3
    SIZE = LIMIT * 2
"#,
        );
        assert_snapshot!(test.pull_up(&["SIZE"], "main.Base"), @"refused: `SIZE` reads Child's `LIMIT`, which it would no longer read in Base's body; pull it up too");
    }

    /// `LIMIT` is bound after `Base`, so `Base`'s body would read it before it is bound
    #[test]
    fn a_member_that_reads_a_name_bound_between_the_classes_is_refused() {
        let test = mixins(
            r#"class Base:
    pass


LIMIT = 3


class <CURSOR>Child(Base):
    SIZE = LIMIT * 2
"#,
        );
        assert_snapshot!(test.pull_up(&["SIZE"], "main.Base"), @"refused: `SIZE` reads `LIMIT`, which is bound after Base is defined");
    }

    /// `Other` finds `describe` in `Mixin`, after `Shape` in its method resolution order, and
    /// would find the one pulled into `Shape` instead
    #[test]
    fn a_pull_up_another_subclass_would_find_before_its_mixin_is_refused() {
        let test = mixins(
            r#"class Shape:
    pass


class Mixin:
    def describe(self) -> str:
        return "mixin"


class <CURSOR>Square(Shape):
    def describe(self) -> str:
        return "square"


class Other(Shape, Mixin):
    pass
"#,
        );
        assert_snapshot!(test.pull_up(&["describe"], "main.Shape"), @"refused: Other would find `describe` in Shape rather than in Mixin");
    }

    /// `Square` finds `Mixin`'s `describe` before `Shape`'s, so it would lose its own
    #[test]
    fn a_pull_up_past_a_mixin_before_the_superclass_is_refused() {
        let test = mixins(
            r#"class Shape:
    pass


class Mixin:
    def describe(self) -> str:
        return "mixin"


class <CURSOR>Square(Mixin, Shape):
    def describe(self) -> str:
        return "square"
"#,
        );
        assert_snapshot!(test.pull_up(&["describe"], "main.Shape"), @"refused: Square would inherit Mixin's `describe` rather than its own");
    }

    /// `Both` finds `Square`'s `describe` before `Other`'s, and would find `Other`'s once
    /// `Square` loses it to `Shape`, which comes after `Other`
    #[test]
    fn a_pull_up_a_class_below_would_find_elsewhere_is_refused() {
        let test = mixins(
            r#"class Shape:
    pass


class <CURSOR>Square(Shape):
    def describe(self) -> str:
        return "square"


class Other(Shape):
    def describe(self) -> str:
        return "other"


class Both(Square, Other):
    pass
"#,
        );
        assert_snapshot!(test.pull_up(&["describe"], "main.Shape"), @"refused: Both would find `describe` in Other rather than in Square");
    }

    /// a decorator the program itself calls `override` is no `typing.override`, and stays
    #[test]
    fn a_decorator_named_override_that_is_not_typing_s_stays() {
        let test = mixins(
            r#"def override(function):
    return function


class Base:
    pass


class <CURSOR>Child(Base):
    @override
    def describe(self) -> str:
        return "child"
"#,
        );
        assert_snapshot!(test.pull_up(&["describe"], "main.Base"), @r#"
        --- /main.py
        def override(function):
            return function


        class Base:
            @override
            def describe(self) -> str:
                return "child"


        class Child(Base):
            pass
        "#);
    }

    /// a member and a base written with one name are told apart by what the pull is asked to
    /// move: here the base, while the class keeps its member
    #[test]
    fn a_base_is_pulled_up_apart_from_a_member_of_its_name() {
        let test = mixins(
            r#"class Mixin:
    pass


class Base:
    pass


class <CURSOR>Child(Base, Mixin):
    Mixin = 1
"#,
        );
        assert_snapshot!(test.pull_up_with_bases(&[], &["Mixin"], "main.Base"), @"
        --- /main.py
        class Mixin:
            pass


        class Base(Mixin):
            pass


        class Child(Base):
            Mixin = 1
        ");
    }

    /// `Root` is `object`, which every class inherits already, so it is no base to pull up
    #[test]
    fn object_is_no_movable_base_whatever_it_is_called() {
        let test = mixins(
            r#"Root = object


class Base:
    pass


class <CURSOR>Child(Base, Root):
    pass
"#,
        );
        assert_snapshot!(test.movable(), @"
        main.Child
        members: Base (Base)
        superclasses: main.Base
        subclasses:
        ");
    }

    /// a class keyword is passed to `__init_subclass__` for the class it is written on alone,
    /// so it cannot move to the new class
    #[test]
    fn extracting_from_a_class_with_a_keyword_is_refused() {
        let test = mixins(
            r#"class Base:
    def __init_subclass__(cls, flag: bool = False, **kwargs):
        super().__init_subclass__(**kwargs)


class <CURSOR>Child(Base, flag=True):
    x = 1
"#,
        );
        assert_snapshot!(test.extract(&["x"], "Shape"), @"refused: Child passes `flag=True` when it is made, which Shape would be passed instead");
    }

    /// a metaclass makes the new class too, and the class still gets it from the new class
    #[test]
    fn extracting_from_a_class_with_a_metaclass_moves_it() {
        let test = mixins(
            r#"from abc import ABCMeta


class <CURSOR>Child(metaclass=ABCMeta):
    x = 1
"#,
        );
        assert_snapshot!(test.extract(&["x"], "Shape"), @"
        --- /main.py
        from abc import ABCMeta


        class Shape(metaclass=ABCMeta):
            x = 1


        class Child(Shape):
            pass
        ");
    }

    #[test]
    fn extracting_an_attribute_of_a_decorated_class_is_refused() {
        let test = mixins(
            r#"from dataclasses import dataclass


@dataclass
class <CURSOR>Point:
    x: int
"#,
        );
        assert_snapshot!(test.extract(&["x"], "Base"), @"refused: `x` is an attribute of a decorated class, which its decorator may read as a field");
    }

    fn two_modules(base: &str, child: &str) -> CursorTest {
        CursorTestBuilder::default()
            .source("pkg/__init__.py", "")
            .source("pkg/base.py", base)
            .source("pkg/child.py", child)
            .build()
    }

    /// pulled into another module, a member reads what it read where it was written, or is
    /// refused as a function moved there is
    #[test]
    fn a_member_pulled_into_another_module_is_checked_as_a_moved_function_is() {
        let base = "class Base:\n    pass\n";
        let test = two_modules(
            base,
            "from pkg.base import Base\n\n\nclass <CURSOR>Child(Base):\n    def origin(self):\n        return __name__\n",
        );
        assert_snapshot!(test.pull_up(&["origin"], "pkg.base.Base"), @"refused: `origin` reads `__name__`, which is pkg.child's own and would be pkg.base's once moved");
        let test = two_modules(
            base,
            "from pkg.base import Base\n\nCOUNT = 0\n\n\nclass <CURSOR>Child(Base):\n    def bump(self):\n        global COUNT\n        COUNT += 1\n",
        );
        assert_snapshot!(test.pull_up(&["bump"], "pkg.base.Base"), @"refused: `bump` assigns pkg.child's `COUNT` through `global`, which would name pkg.base's once moved");
        let test = two_modules(
            "len = 3\n\n\nclass Base:\n    pass\n",
            "from pkg.base import Base\n\n\nclass <CURSOR>Child(Base):\n    def size(self, x):\n        return len(x)\n",
        );
        assert_snapshot!(test.pull_up(&["size"], "pkg.base.Base"), @"refused: `size` uses the builtin `len`, which pkg.base binds to something else");
        let test = two_modules(
            base,
            "import os\nfrom pkg.base import Base\n\nimport os.path as os\n\n\nclass <CURSOR>Child(Base):\n    def sep(self):\n        return os.sep\n",
        );
        assert_snapshot!(test.pull_up(&["sep"], "pkg.base.Base"), @"refused: `sep` uses `os`, which pkg.child binds more than once, and an import of it would keep only the value it had when imported");
    }

    /// pushed into another module, a member reads what it read where it was written: a name
    /// private to its module is not imported into the other
    #[test]
    fn a_member_pushed_into_another_module_is_checked_as_a_moved_function_is() {
        let test = two_modules(
            "_RATE = 2\n\n\nclass <CURSOR>Base:\n    def rate(self):\n        return _RATE\n",
            "from pkg.base import Base\n\n\nclass Child(Base):\n    pass\n",
        );
        assert_snapshot!(test.push_down(&["rate"]), @"refused: `rate` uses `_RATE`, which is private to pkg.base; make it public, or move it too");
    }

    /// what a decorator makes of a method is what the checker resolves it to, under whatever
    /// name it is bound to
    #[test]
    fn a_member_s_kind_is_what_its_decorators_resolve_to() {
        let test = mixins(
            r#"prop = property
static = staticmethod


class Base:
    pass


class <CURSOR>Child(Base):
    @prop
    def size(self):
        return 1

    @static
    def make(other):
        return other.missing
"#,
        );
        assert_snapshot!(test.movable(), @"
        main.Child
        members: size(self) (Property), make(other) (StaticMethod), Base (Base)
        superclasses: main.Base
        subclasses:
        ");
        // a static method is passed no receiver, so `other.missing` is no attribute it needs
        assert_snapshot!(test.pull_up(&["make"], "main.Base"), @"
        --- /main.py
        prop = property
        static = staticmethod


        class Base:
            @static
            def make(other):
                return other.missing


        class Child(Base):
            @prop
            def size(self):
                return 1
        ");
    }

    /// `super` read under another name still names the class the method is written in, and a
    /// parameter called `super` does not
    #[test]
    fn a_use_of_super_is_the_builtin_s_under_whatever_name() {
        let test = mixins(
            r#"sup = super


class Root:
    def describe(self):
        return "root"


class Base(Root):
    pass


class Mixin:
    pass


class <CURSOR>Child(Base, Mixin):
    def describe(self):
        return sup(Child, self).describe()

    def tag(self, super):
        return super.tag
"#,
        );
        assert_snapshot!(test.pull_up(&["describe"], "main.Base"), @"refused: `describe` calls `super()`, which would start from Base rather than from Child");
        assert_snapshot!(test.pull_up(&["tag"], "main.Base"), @r#"
        --- /main.py
        sup = super


        class Root:
            def describe(self):
                return "root"


        class Base(Root):
            def tag(self, super):
                return super.tag


        class Mixin:
            pass


        class Child(Base, Mixin):
            def describe(self):
                return sup(Child, self).describe()
        "#);
    }

    /// a name a pushed member reads from around its class has to be the same name around the
    /// subclass: a function the subclass is defined in, or the subclass's body for what the class
    /// body evaluates, may bind it to something else
    #[test]
    fn a_push_down_into_a_scope_that_binds_a_name_it_reads_is_refused() {
        let test = mixins(
            "def helper():\n    return 1\n\n\nclass <CURSOR>Base:\n    def run(self):\n        return helper()\n\n\ndef make():\n    def helper():\n        return 2\n\n    class Child(Base):\n        pass\n\n    return Child\n",
        );
        assert_snapshot!(test.push_down(&["run"]), @"refused: `run` reads `helper`, which is another name where Child is defined");
        let test = mixins(
            "def helper():\n    return 1\n\n\nclass <CURSOR>Base:\n    def run(self, value=helper()):\n        return value\n\n\nclass Child(Base):\n    helper = None\n",
        );
        assert_snapshot!(test.push_down(&["run"]), @"refused: `run` reads `helper`, which is another name where Child is defined");
        let test = mixins(
            "def make():\n    def helper():\n        return 2\n\n    class <CURSOR>Base:\n        def run(self):\n            return helper()\n\n    class Child(Base):\n        pass\n\n    return Child\n",
        );
        assert_snapshot!(test.push_down(&["run"]), @"
        --- /main.py
        def make():
            def helper():
                return 2

            class Base:
                pass

            class Child(Base):
                def run(self):
                    return helper()

            return Child
        ");
    }

    /// a name a pulled member reads from around its class has to be the same name around the
    /// superclass, as it does for what the class body evaluates
    #[test]
    fn a_pull_up_out_of_a_scope_that_binds_a_name_it_reads_is_refused() {
        let test = mixins(
            "class Base:\n    pass\n\n\ndef make():\n    def helper():\n        return 2\n\n    class <CURSOR>Child(Base):\n        def run(self):\n            return helper()\n\n    return Child\n",
        );
        assert_snapshot!(test.pull_up(&["run"], "main.Base"), @"refused: `run` reads `helper`, which is another name where Base is defined");
    }

    /// `__class__` in a method is the class the method is written in, so it names another class
    /// wherever the method moves to, and a `__class__` the method binds itself does not
    #[test]
    fn a_method_reading_its_class_cell_stays_in_its_class() {
        let test = mixins(
            "class Base:\n    def make(self):\n        return __class__()\n\n\nclass <CURSOR>Child(Base):\n    def copy(self):\n        return __class__()\n\n    def local(self):\n        __class__ = int\n        return __class__()\n",
        );
        assert_snapshot!(test.pull_up(&["copy"], "main.Base"), @"refused: `copy` reads `__class__`, which would be Base rather than Child");
        assert_snapshot!(test.extract(&["copy"], "Middle"), @"refused: `copy` reads `__class__`, which would be Middle rather than Child");
        assert_snapshot!(test.pull_up(&["local"], "main.Base"), @"
        --- /main.py
        class Base:
            def make(self):
                return __class__()

            def local(self):
                __class__ = int
                return __class__()


        class Child(Base):
            def copy(self):
                return __class__()
        ");
        let test = mixins(
            "class <CURSOR>Base:\n    def make(self):\n        return __class__()\n\n\nclass Child(Base):\n    pass\n",
        );
        assert_snapshot!(test.push_down(&["make"]), @"refused: `make` reads `__class__`, which would be the subclass it lands in rather than Base");
    }

    /// an enum is a base python treats specially, whatever it holds, and a class that holds what
    /// an enum does is not one
    #[test]
    fn an_enum_base_is_told_by_its_class() {
        let test = mixins(
            "from enum import Enum\n\n\nclass Flavor(Enum):\n    pass\n\n\nclass <CURSOR>Child(Flavor):\n    def describe(self):\n        return 1\n",
        );
        assert_snapshot!(test.extract(&["describe"], "Base"), @"refused: Child has a base whose subclasses python treats specially, which Base would take in its place");
        let test = mixins(
            "class Registry:\n    _member_map_ = {}\n\n\nclass <CURSOR>Child(Registry):\n    def describe(self):\n        return 1\n",
        );
        assert_snapshot!(test.extract(&["describe"], "Base"), @"
        --- /main.py
        class Registry:
            _member_map_ = {}


        class Base(Registry):
            def describe(self):
                return 1


        class Child(Base):
            pass
        ");
    }
}
