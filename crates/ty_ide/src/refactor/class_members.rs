//! Pulling a class's members up into one of its superclasses, and pushing them down into its
//! subclasses.
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
//! A member is every statement of a class body that binds its name: a method (with its property
//! accessors or overloads, which share the name), or a class attribute. It moves with its
//! decorators and the comments directly above it, reindented for the body it lands in.
//!
//! Either move is refused, with the reason, where the program would no longer mean what it
//! meant:
//!
//! - pulled up, a member that reads an attribute of its receiver the superclass does not have,
//!   which the superclass's other subclasses would then miss; that uses `super()` from a class
//!   whose base is not the superclass, since `super()` would then start from elsewhere; that
//!   another subclass of the superclass inherits from further up already, and would inherit the
//!   pulled one instead; or that the superclass binds already
//! - pushed down, a member something reads off the class itself or an instance that may be the
//!   class rather than a subclass — another of its methods, through `self`, included — or of a
//!   class without subclasses in the project
//! - either way, a name-mangled `__member`, whose name depends on the class it is written in,
//!   and, between modules, a member that reads a name of its module that the other would have
//!   to import from a module that imports it
//!
//! `override` follows the move: a method pulled up overrides nothing where it lands unless the
//! superclass inherits the name, so it loses the modifier there, and the methods of the same
//! name below the superclass then override it and gain the modifier; a method pushed down keeps
//! it only where something above still has the name.

use ruff_db::files::File;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::source_text;
use ruff_diagnostics::Edit;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor, walk_expr};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_python_codegen::Stylist;
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::FxHashSet;
use ty_module_resolver::file_to_module;
use ty_project::Db;
use ty_python_core::ProgramFile;
use ty_python_semantic::importer::{ImportAction, ImportRequest, MembersInScope};
use ty_python_semantic::types::Type;
use ty_python_semantic::{
    HasType, ProgramEnvironment, SemanticModel, class_bases, instance_member_names,
    is_within_classes, type_hierarchy_prepare, type_hierarchy_subtypes,
};

use super::move_symbol::{
    Dependency, OwnedRequest, SymbolMove, cut_range, dependencies, imports_module, line_number,
};
use crate::code_action::FileEdit;
use crate::goto::find_goto_target;
use crate::import_order;
use crate::method_kind::{MethodKind, method_kind};
use crate::references::{ReferencesMode, references};

/// What kind of member of a class a member is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Method,
    ClassMethod,
    StaticMethod,
    Property,
    Attribute,
}

/// A member [`pull_members_up`] or [`push_members_down`] can move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassMember {
    pub name: String,
    pub kind: MemberKind,
    /// The range of its name in its first statement.
    pub name_range: TextRange,
}

/// A class of the project, where its name is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HierarchyClass {
    pub file: File,
    pub name: String,
    /// Its module's dotted name and its own, the way [`pull_members_up`] is told which
    /// superclass to pull into.
    pub qualified_name: Option<String>,
    pub name_range: TextRange,
}

/// The members of the class at a position, and the classes of the project above and below it
/// they could move to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovableMembers {
    pub class: HierarchyClass,
    pub members: Vec<ClassMember>,
    /// The project's classes the class inherits from, nearest first.
    pub superclasses: Vec<HierarchyClass>,
    /// The project's classes that name the class as a base.
    pub subclasses: Vec<HierarchyClass>,
}

/// The class around `offset`, its members, and where they could go.
pub fn movable_members(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
) -> Option<MovableMembers> {
    let class = ClassSite::at(db, file, offset)?;
    let def = class.def()?;
    let mut members: Vec<ClassMember> = Vec::new();
    for statement in &def.body {
        let Some((name, kind)) = member_of(statement) else {
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
        });
    }
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

/// Moves the members `names` of the class around `offset` into its superclass `target`, named by
/// its qualified name as [`movable_members`] lists it. `Err` says why they cannot be moved.
pub fn pull_members_up(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
    names: &[String],
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
    let super_members = instance_member_names(db, &env, superclass.ty);
    let pulled: FxHashSet<&str> = names.iter().map(String::as_str).collect();
    let source = source_text(db, class.file.file(db));
    let source = source.as_str();

    let mut moved = SymbolMove::default();
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
            || assigns_on_receiver(super_def, name)
        {
            return Err(format!("{} has a `{name}` already", superclass.name));
        }
        let inherited_above = super_members.contains(&Name::new(name));
        // a class between the two that has the name would be what the class inherits instead
        if let Some(between) = ancestors
            .iter()
            .take_while(|ancestor| !ancestor.same(superclass))
            .find(|ancestor| {
                superclass.is_ancestor_of(db, ancestor)
                    && ancestor
                        .def()
                        .is_some_and(|def| !member_statements(def, name).is_empty())
            })
        {
            return Err(format!(
                "{} would inherit {}'s `{name}` rather than its own",
                class.name, between.name
            ));
        }
        for &index in &statements {
            let statement = &def.body[index];
            // what it reads off its receiver has to be there in the superclass
            for (attribute, range) in receiver_attributes(statement) {
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
            for (sibling, _) in class_scope_reads(def, statement) {
                if !pulled.contains(sibling.as_str()) && !super_members.contains(&sibling) {
                    return Err(format!(
                        "`{name}` reads {}'s `{sibling}`, which {} does not have; pull it up too",
                        class.name, superclass.name
                    ));
                }
            }
            if uses_super(statement) && !class.has_only_base(db, superclass) {
                return Err(format!(
                    "`{name}` calls `super()`, which would start from {} rather than from {}",
                    superclass.name, class.name
                ));
            }
            if superclass.file_id != class.file_id {
                let parsed = &class.parsed;
                for dependency in dependencies(db, class.file, parsed, statement, name) {
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
                member_kind(statement),
                member_text(
                    source,
                    &def.body,
                    index,
                    !inherited_above && has_override(statement),
                ),
            ));
            removed.push(index);
        }
        if inherited_above {
            // another subclass of the superclass that inherits it from further up would
            // inherit this one instead
            for below in superclass.descendants(db) {
                if below.same(&class) || class.is_ancestor_of(db, &below) {
                    continue;
                }
                if !below.inherits_own(db, name, superclass) {
                    return Err(format!(
                        "{} inherits `{name}` from above {}, and would inherit {}'s instead",
                        below.name, superclass.name, class.name
                    ));
                }
            }
        } else {
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
                && !has_override(&below_def.body[first])
            {
                moved
                    .edits
                    .extend(add_override(db, &below, &below_def.body[first]));
            }
        }
    }
    Ok(moved)
}

/// Moves the members `names` of the class around `offset` into each of the project's classes
/// that name it as a base, as a copy into each that does not define the member itself. `Err`
/// says why they cannot be moved.
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
            if uses_super(statement) {
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
                member_kind(statement),
                member_text(
                    source,
                    &def.body,
                    index,
                    !inherited_above && has_override(statement),
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
            let Some((name, _)) = member_of(statement) else {
                continue;
            };
            let own = member_statements(sub_def, name.as_str());
            if let Some(&first) = own.first() {
                // it keeps its own, which overrides nothing once the class no longer has it
                let own_statement = &sub_def.body[first];
                if !inherited.contains(name) && has_override(own_statement) {
                    moved
                        .edits
                        .extend(remove_override(db, subclass, own_statement));
                }
                continue;
            }
            copies.push((*kind, reindent(text, &from_indent, &to_indent)));
            if subclass.file_id != class.file_id {
                for dependency in dependencies(db, class.file, &class.parsed, statement, name) {
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

/// A class of the project, parsed.
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
    /// The innermost class whose definition `offset` is in.
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

    /// The class whose name is at `name_range` in `file`, of the type `ty`.
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

    /// The class described by `class`, a class of the type hierarchy, of the type `ty`.
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

    /// The project's classes this class names as its bases.
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

    /// The project's classes this class inherits from, nearest first.
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

    /// Whether this class names `base` as its one base.
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

    /// The project's classes that name this class as a base.
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

    /// The project's classes below this one, at any depth.
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

    /// Whether this class, or a class between it and `top`, defines `name` itself, so that it
    /// does not inherit `name` from `top` or above.
    fn inherits_own(&self, db: &'db dyn Db, name: &str, top: &ClassSite<'db>) -> bool {
        if self
            .def()
            .is_some_and(|def| !member_statements(def, name).is_empty())
        {
            return true;
        }
        self.ancestors(db)
            .iter()
            .take_while(|ancestor| !ancestor.same(top))
            .any(|ancestor| {
                top.is_ancestor_of(db, ancestor)
                    && ancestor
                        .def()
                        .is_some_and(|def| !member_statements(def, name).is_empty())
            })
    }
}

/// Every class definition of a module, at any depth.
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

/// The member a class body statement binds, and what kind of member it is.
fn member_of(statement: &Stmt) -> Option<(&Name, MemberKind)> {
    match statement {
        Stmt::FunctionDef(function) => Some((&function.name.id, member_kind(statement))),
        Stmt::Assign(assign) => match assign.targets.as_slice() {
            [Expr::Name(name)] => Some((&name.id, MemberKind::Attribute)),
            _ => None,
        },
        Stmt::AnnAssign(assign) => match assign.target.as_ref() {
            Expr::Name(name) => Some((&name.id, MemberKind::Attribute)),
            _ => None,
        },
        _ => None,
    }
}

fn member_kind(statement: &Stmt) -> MemberKind {
    let Stmt::FunctionDef(function) = statement else {
        return MemberKind::Attribute;
    };
    let is_property = function.decorator_list.iter().any(|decorator| {
        matches!(&decorator.expression, Expr::Name(name) if name.id.as_str() == "property")
            || matches!(&decorator.expression, Expr::Attribute(attribute)
                if matches!(attribute.attr.as_str(), "setter" | "getter" | "deleter"))
    });
    if is_property {
        return MemberKind::Property;
    }
    match method_kind(function) {
        MethodKind::Instance => MemberKind::Method,
        MethodKind::Class => MemberKind::ClassMethod,
        MethodKind::Static => MemberKind::StaticMethod,
    }
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
    member_of(statement).is_some_and(|(bound, _)| bound.as_str() == name)
}

/// The indices of the statements of `class`'s body that bind `name`.
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

/// Whether a method of `class` assigns `name` on its receiver: `self.name = ...`.
fn assigns_on_receiver(class: &ast::StmtClassDef, name: &str) -> bool {
    class.body.iter().any(|statement| {
        receiver_attributes(statement)
            .iter()
            .any(|(attribute, _)| attribute.as_str() == name)
            && matches!(statement, Stmt::FunctionDef(_))
            && assigned_on_receiver(statement, name)
    })
}

fn assigned_on_receiver(statement: &Stmt, name: &str) -> bool {
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
    let Some(receiver) = receiver_name(statement) else {
        return false;
    };
    let mut stores = Stores {
        receiver: receiver.as_str(),
        name,
        found: false,
    };
    stores.visit_stmt(statement);
    stores.found
}

/// The name of a method's receiver parameter, `self` or `cls`; `None` for a static method or
/// what is no method.
fn receiver_name(statement: &Stmt) -> Option<Name> {
    let Stmt::FunctionDef(function) = statement else {
        return None;
    };
    if method_kind(function) == MethodKind::Static {
        return None;
    }
    function
        .parameters
        .posonlyargs
        .iter()
        .chain(&function.parameters.args)
        .next()
        .map(|parameter| parameter.parameter.name.id.clone())
}

/// The attributes a method reads or writes on its receiver, `x` of `self.x`, with where.
fn receiver_attributes(statement: &Stmt) -> Vec<(Name, TextRange)> {
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
    let Some(receiver) = receiver_name(statement) else {
        return Vec::new();
    };
    let mut attributes = Attributes {
        receiver: receiver.as_str(),
        found: Vec::new(),
    };
    if let Stmt::FunctionDef(function) = statement {
        attributes.visit_body(&function.body);
    }
    attributes.found
}

/// The names an attribute's value reads from the class body it is in: other members bound
/// before it, like `B` in `B = A + 1`.
fn class_scope_reads(class: &ast::StmtClassDef, statement: &Stmt) -> Vec<(Name, TextRange)> {
    struct Loads {
        found: Vec<(Name, TextRange)>,
    }
    impl<'a> SourceOrderVisitor<'a> for Loads {
        fn visit_expr(&mut self, expr: &'a Expr) {
            match expr {
                Expr::Name(name) if name.ctx.is_load() => {
                    self.found.push((name.id.clone(), name.range()));
                }
                // a lambda or comprehension does not see the class body
                Expr::Lambda(_)
                | Expr::ListComp(_)
                | Expr::SetComp(_)
                | Expr::DictComp(_)
                | Expr::Generator(_) => {}
                _ => walk_expr(self, expr),
            }
        }
    }
    let value = match statement {
        Stmt::Assign(assign) => &*assign.value,
        Stmt::AnnAssign(assign) => match &assign.value {
            Some(value) => value,
            None => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    let mut loads = Loads { found: Vec::new() };
    loads.visit_expr(value);
    loads
        .found
        .into_iter()
        .filter(|(name, _)| {
            class
                .body
                .iter()
                .any(|other| !std::ptr::eq(other, statement) && binds(other, name))
        })
        .collect()
}

/// Whether a method calls `super()`, or reads basedpython's `super`.
fn uses_super(statement: &Stmt) -> bool {
    struct Super {
        found: bool,
    }
    impl<'a> SourceOrderVisitor<'a> for Super {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if matches!(expr, Expr::Name(name) if name.id.as_str() == "super") {
                self.found = true;
            }
            walk_expr(self, expr);
        }
    }
    let mut uses = Super { found: false };
    uses.visit_stmt(statement);
    uses.found
}

/// The `override` a method is marked with: basedpython's modifier, or `@override`.
fn override_marker(statement: &Stmt) -> Option<&ast::Decorator> {
    let Stmt::FunctionDef(function) = statement else {
        return None;
    };
    function.decorator_list.iter().find(|decorator| {
        matches!(&decorator.expression, Expr::Name(name) if name.id.as_str() == "override")
            || matches!(&decorator.expression, Expr::Attribute(attribute) if attribute.attr.as_str() == "override")
    })
}

fn has_override(statement: &Stmt) -> bool {
    override_marker(statement).is_some()
}

/// The range that takes `decorator` out: an `@override` line with its line break, or the
/// `override` modifier keyword with the space after it.
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

/// The text of the body statement `index`, as [`cut_range`] cuts it, without its `override` when
/// `strip_override`.
fn member_text(source: &str, body: &[Stmt], index: usize, strip_override: bool) -> String {
    let statement = &body[index];
    let cut = cut_range(source, body, index);
    if strip_override && let Some(decorator) = override_marker(statement) {
        let local = override_range(source, decorator) - cut.start;
        let mut text = cut.text;
        text.replace_range(local.to_std_range(), "");
        return text;
    }
    cut.text
}

/// The indentation the statements of `class`'s body are written with.
fn body_indentation(source: &str, class: &ast::StmtClassDef) -> String {
    class.body.first().map_or_else(
        || "    ".to_string(),
        |statement| {
            let start = source.line_start(statement.start());
            source[TextRange::new(start, statement.start())].to_string()
        },
    )
}

/// `text` with each line's leading `from` replaced by `to`.
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

/// Whether a body statement stands in for no body: `pass` or `...`.
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

/// The edits taking the statements `removed` out of `class`'s body, leaving `pass` when nothing
/// but a docstring would be left.
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

/// The edits adding `members` to `class`'s body: an attribute after its last attribute, before
/// its first method; a method after its last statement, a blank line away; in place of a body
/// that is only `pass` or `...`.
fn insert_members(
    file: File,
    source: &str,
    class: &ast::StmtClassDef,
    members: &[(MemberKind, String)],
    line_ending: &str,
) -> Vec<FileEdit> {
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

/// The edits importing `requests` into `class`'s module, each under the name the moved code
/// reads it by.
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

/// The edits marking `statement`, a method of `class`, as an override: basedpython's `override`
/// modifier, or python's `@override` with its import.
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

/// The edit taking the `override` off `statement`, a method of `class`.
fn remove_override(db: &dyn Db, class: &ClassSite<'_>, statement: &Stmt) -> Vec<FileEdit> {
    let Some(decorator) = override_marker(statement) else {
        return Vec::new();
    };
    let source = source_text(db, class.file_id);
    vec![FileEdit {
        file: class.file_id,
        edit: Edit::range_deletion(override_range(source.as_str(), decorator)),
    }]
}

/// The type of the receiver of the attribute whose name is at `range` in `file`: `x` of `x.name`.
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

    use super::{movable_members, pull_members_up, push_members_down};
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
            let names: Vec<String> = names.iter().map(ToString::to_string).collect();
            let moved = salsa::attach(&self.db, || {
                pull_members_up(
                    &self.db,
                    self.program_file(self.cursor.file),
                    self.cursor.offset,
                    &names,
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
                    .map(|member| format!("{} ({:?})", member.name, member.kind))
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
        members: LIMIT (Attribute), __init__ (Method), area (Method), plain (Method), helper (Method)
        superclasses: cls.shapes.Base
        subclasses: cls.shapes.GrandChild
        ");
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
    /// fails.
    #[test]
    fn a_method_that_reads_what_the_superclass_lacks_is_refused() {
        let test = project(&SHAPES.replace("class Child(Base)", "class <CURSOR>Child(Base)"));
        assert_snapshot!(test.pull_up(&["area"], "cls.shapes.Base"), @"refused: `area` uses `self.x` on line 13, which Base does not have");
        assert_snapshot!(test.pull_up(&["helper"], "cls.shapes.Base"), @"refused: `helper` uses `self.plain` on line 19, which Base does not have; pull it up too");
    }

    /// Pulled up together, `helper` finds the `plain` it calls.
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

    /// `Base().keep()` would no longer find `keep`.
    #[test]
    fn a_member_read_off_the_class_itself_is_not_pushed_down() {
        let test = project(&SHAPES.replace("class Base:", "class <CURSOR>Base:"));
        assert_snapshot!(test.push_down(&["keep"]), @"refused: cls/user.py:3 uses `keep` on what may be a Base rather than a subclass");
    }

    /// `helper` calls `self.plain()` on what may be a `Child` rather than a `GrandChild`.
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

    /// Pulled up where nothing above has the name, a method overrides nothing, and what has
    /// the name below the superclass then overrides it: in basedpython it gains the modifier.
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

    /// In python it gains `@override`, imported.
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

    /// `Leaf` would inherit `Mid`'s `describe` instead of the one it pulls up.
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
}
