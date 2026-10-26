//! Moving a module's top-level function, class or variable to another module.
//!
//! ```py
//! # shapes/util.py               # shapes/geometry.py
//! import math                    import math
//!
//! RATE = 2                       from shapes.util import RATE
//!
//! def area(r):          ──►      def area(r):
//!     return math.pi * r * RATE      return math.pi * r * RATE
//! ```
//!
//! The definition is cut from its module, with its decorators and the comments directly above
//! it, and appended to the target. What it reads from its old module comes with it: an import it
//! uses is written in the target too (and dropped from the old module when nothing else there
//! uses it and no other module imports it from there), and another of the old module's names is
//! imported from there. Every reference to it in the project follows it:
//! `from shapes.util import area` imports it from `shapes.geometry`, `shapes.util.area` reads
//! `shapes.geometry.area`, a module that reached it through a star import of the old module
//! imports it by name, and the old module imports it back when it still uses it.
//!
//! The move is refused, with the reason, when the result would not run as the original did:
//! when the definition reads a name private to its module, which nothing outside the module is
//! meant to import; when it reads a name its module binds more than once, which an import would
//! see only as it was when imported; when it assigns a name of its module through `global`, which
//! would name the target's once moved; when it reads an attribute of its module object, `__name__`
//! or `__file__`, which would be the target's; when it reads a builtin the target binds to
//! something else; when the two modules would end up importing each other, which python cannot
//! load; when the target already binds the name; when the name is bound more than once in its
//! module; and when the modules are not the same kind of file.

use ruff_db::files::File;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::source_text;
use ruff_diagnostics::Edit;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr};
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, Stmt};
use ruff_python_codegen::Stylist;
use ruff_source_file::{LineRanges, UniversalNewlines};
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::{FxHashMap, FxHashSet};
use ty_module_resolver::{ImportingFile, ModuleName, file_to_module};
use ty_project::Db;
use ty_python_core::ProgramFile;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::scope::FileScopeId;
use ty_python_core::{SemanticIndex, semantic_index};
use ty_python_semantic::importer::{ImportAction, ImportRequest, MembersInScope};
use ty_python_semantic::types::Type;
use ty_python_semantic::{HasType, SemanticModel};

use super::flow::{bindings_of_symbol, resolving_scope};
use crate::code_action::FileEdit;
use crate::goto::find_goto_target;
use crate::import_order;
use crate::references::{ReferencesMode, references};

/// What a top-level definition that can be moved is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovableKind {
    Function,
    Class,
    Variable,
}

/// A top-level definition [`move_symbol`] can move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovableSymbol {
    /// The file it is defined in, which need not be the one asked about.
    pub file: File,
    pub name: String,
    pub kind: MovableKind,
    /// The range of its name where it is defined.
    pub name_range: TextRange,
    /// The qualified name of the module it is defined in.
    pub module: String,
}

/// What [`move_symbol`] does to the project.
#[derive(Debug, Default)]
pub struct SymbolMove {
    pub edits: Vec<FileEdit>,
    /// References the move leaves as they are, which may no longer reach the definition once it
    /// has moved, each described with where it is.
    pub warnings: Vec<String>,
}

/// the top-level definition named at `offset` — at its definition or at a reference to it — or
/// else the one `offset` is anywhere in, when it is one [`move_symbol`] can move: a function, a
/// class, or a variable a single name is assigned to, at the top level of a module of the project
pub fn movable_symbol(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
) -> Option<MovableSymbol> {
    let (file, offset) = declaration_site(db, file, offset)?;
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let declaration = Declaration::at(parsed.suite(), offset)?;
    let module = file_to_module(db, file.resolver_file(db))?;
    Some(MovableSymbol {
        file: file.file(db),
        name: declaration.name.to_string(),
        kind: declaration.kind,
        name_range: declaration.name_range,
        module: module.name(db).to_string(),
    })
}

/// Moves the top-level definition named at `offset` to the end of the module `target`, and
/// updates the project to find it there. `Err` says why it cannot be moved.
pub fn move_symbol(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
    target: File,
) -> Result<SymbolMove, String> {
    let (source, offset) = declaration_site(db, file, offset)
        .ok_or_else(|| "there is no top-level function, class or variable here".to_string())?;
    let parsed = parsed_module(db, source.python_file(db)).load(db);
    let declaration = Declaration::at(parsed.suite(), offset)
        .ok_or_else(|| "there is no top-level function, class or variable here".to_string())?;
    let name = declaration.name;
    let source_file = source.file(db);
    let target_file = ProgramFile::new(db, target, source.program(db));

    let source_module = file_to_module(db, source.resolver_file(db))
        .ok_or_else(|| format!("`{name}` is not in a module anything can import"))?
        .name(db)
        .clone();
    if target == source_file {
        return Err(format!("`{name}` is in {source_module} already"));
    }
    let target_module = file_to_module(db, target_file.resolver_file(db))
        .ok_or_else(|| "the target is not a module anything can import".to_string())?
        .name(db)
        .clone();
    let source_type = source_file.source_type(db);
    let target_type = target.source_type(db);
    if !target_type.is_py_file() {
        return Err(format!("{target_module} is not a module of source code"));
    }
    if source_type.is_basedpython() != target_type.is_basedpython() {
        return Err(if source_type.is_basedpython() {
            format!(
                "{target_module} is a python module, and `{name}` is basedpython, which python does \
                 not read"
            )
        } else {
            format!(
                "{target_module} is a basedpython module, which reads `{name}`'s python \
                 differently"
            )
        });
    }

    let index = semantic_index(db, source);
    let globals = index.place_table(FileScopeId::global());
    let symbol = globals
        .symbol_id(name)
        .ok_or_else(|| format!("`{name}` is not a name of {source_module}"))?;
    let bindings = bindings_of_symbol(
        db,
        index,
        &parsed,
        FileScopeId::global(),
        symbol,
        source_type.is_stub(),
    );
    if bindings.len() != 1 {
        return Err(format!(
            "`{name}` is bound more than once in {source_module}, so moving one of them would \
             change which the module ends with"
        ));
    }

    let target_parsed = parsed_module(db, target_file.python_file(db)).load(db);
    let target_index = semantic_index(db, target_file);
    let target_globals = target_index.place_table(FileScopeId::global());
    let target_binds = |name: &str| {
        target_globals
            .symbol_id(name)
            .is_some_and(|symbol| target_globals.symbol(symbol).is_bound())
    };
    if target_binds(name) {
        return Err(format!("{target_module} has a `{name}` already"));
    }

    // a `global` in the definition names the module it is in, which the move changes
    if let Some(written) = global_written_by(db, index, declaration.statement.range()) {
        return Err(format!(
            "`{name}` assigns {source_module}'s `{written}` through `global`, which would name \
             {target_module}'s once moved"
        ));
    }

    // What the definition reads from its module comes with it.
    let dependencies = dependencies(db, source, &parsed, declaration.statement, name);
    let mut target_imports: Vec<OwnedRequest> = Vec::new();
    let mut from_source: Vec<String> = Vec::new();
    for builtin in builtins_read(db, source, declaration.statement) {
        if target_binds(&builtin) {
            return Err(format!(
                "`{name}` uses the builtin `{builtin}`, which {target_module} binds to something \
                 else"
            ));
        }
    }
    for dependency in &dependencies {
        let read = match dependency {
            Dependency::Imported { request, .. } => &request.binds,
            Dependency::Sibling(sibling) => sibling,
        };
        if globals.symbol_id(read).is_some_and(|symbol| {
            bindings_of_symbol(
                db,
                index,
                &parsed,
                FileScopeId::global(),
                symbol,
                source_type.is_stub(),
            )
            .len()
                > 1
        }) {
            return Err(format!(
                "`{name}` uses `{read}`, which {source_module} binds more than once, and an import \
                 of it would keep only the value it had when imported"
            ));
        }
        match dependency {
            Dependency::Imported { request, .. } => {
                if target_binds(&request.binds)
                    && !target_imports_same(db, target_file, &target_parsed, request)
                {
                    return Err(format!(
                        "`{name}` uses `{}`, which {target_module} binds to something else",
                        request.binds
                    ));
                }
                target_imports.push(request.clone());
            }
            Dependency::Sibling(sibling) => {
                if is_private(sibling) {
                    return Err(format!(
                        "`{name}` uses `{sibling}`, which is private to {source_module}; make it \
                         public, or move it too"
                    ));
                }
                if target_binds(sibling) {
                    return Err(format!(
                        "`{name}` uses {source_module}'s `{sibling}`, which {target_module} binds \
                         to something else"
                    ));
                }
                from_source.push(sibling.clone());
            }
        }
    }

    // Where the definition is referred to, other than by itself.
    let statement_range = declaration.statement.range();
    let model = SemanticModel::new(db, source);
    let goto_target = find_goto_target(&model, &parsed, declaration.name_range.start())
        .ok_or_else(|| format!("`{name}` cannot be searched for"))?;
    let mut by_file: FxHashMap<File, Vec<TextRange>> = FxHashMap::default();
    for reference in
        references(db, source, &goto_target, ReferencesMode::RenameMultiFile).unwrap_or_default()
    {
        if reference.file() == source_file && statement_range.contains_range(reference.range()) {
            continue;
        }
        by_file
            .entry(reference.file())
            .or_default()
            .push(reference.range());
    }
    let source_uses: Vec<TextRange> = by_file.remove(&source_file).unwrap_or_default();

    // Two modules that import each other at the top cannot both be loaded: the second one would
    // read a name from the first before the first has bound it.
    let source_needs_target =
        !source_uses.is_empty() || imports_module(db, source, &parsed, &target_module);
    let target_needs_source =
        !from_source.is_empty() || imports_module(db, target_file, &target_parsed, &source_module);
    if source_needs_target && target_needs_source {
        let reason = if from_source.is_empty() {
            format!("{target_module} imports {source_module}")
        } else {
            format!(
                "`{name}` uses {}",
                from_source
                    .iter()
                    .map(|sibling| format!("`{sibling}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let because = if source_uses.is_empty() {
            format!("{source_module} imports {target_module}")
        } else {
            format!("{source_module} still uses `{name}`")
        };
        return Err(format!(
            "{source_module} and {target_module} would import each other: {reason}, and \
             {because}"
        ));
    }

    let mut moved = SymbolMove::default();

    // The old module: the definition cut, the imports only it used dropped, and the definition
    // imported back where the module still uses it.
    let source_text = source_text(db, source_file);
    let source_text = source_text.as_str();
    let cut = cut_range(source_text, parsed.suite(), declaration.index);
    moved.edits.push(FileEdit {
        file: source_file,
        edit: Edit::range_deletion(cut.removed),
    });
    for dependency in &dependencies {
        if let Dependency::Imported {
            binding_statement: Some(binding_statement),
            alias_index,
            request,
        } = dependency
            && !used_elsewhere(&parsed, &request.binds, statement_range)
            && !imported_from(
                db,
                source,
                &parsed,
                &model,
                *binding_statement,
                *alias_index,
                &source_module,
            )
            && let Some(edit) = remove_alias(
                source_text,
                parsed.suite(),
                *binding_statement,
                *alias_index,
            )
        {
            moved.edits.push(FileEdit {
                file: source_file,
                edit,
            });
        }
    }
    if let Some(edit) = remove_from_all(parsed.suite(), name) {
        moved.edits.push(FileEdit {
            file: source_file,
            edit,
        });
    }
    if let Some(first_use) = source_uses.iter().map(Ranged::start).min() {
        let importer = import_order::importer(db, source, &parsed);
        let action = importer.import(
            ImportRequest::import_from(target_module.as_str(), name),
            &MembersInScope::empty(first_use),
        );
        moved.edits.extend(
            importer
                .combined_edits([&action])
                .into_iter()
                .map(|edit| FileEdit {
                    file: source_file,
                    edit,
                }),
        );
    }

    // The target: what the definition imports, then the definition.
    let target_text = ruff_db::source::source_text(db, target);
    let target_text = target_text.as_str();
    let target_end = TextSize::of(target_text);
    let stylist = Stylist::from_tokens(target_parsed.tokens(), target_text);
    let importer = import_order::importer(db, target_file, &target_parsed);
    let members = MembersInScope::empty(target_end);
    let mut actions: Vec<ImportAction> = Vec::new();
    for request in &target_imports {
        let action = importer.import(request.request(), &members);
        if action.symbol_text() != request.binds {
            return Err(format!(
                "`{name}` uses `{}`, which {target_module} cannot import under that name",
                request.binds
            ));
        }
        actions.push(action);
    }
    for sibling in &from_source {
        actions.push(importer.import(
            ImportRequest::import_from(source_module.as_str(), sibling),
            &members,
        ));
    }
    let import_edits = importer.combined_edits(&actions);
    let adds_imports = !import_edits.is_empty();
    moved.edits.extend(
        import_edits
            .into_iter()
            .map(|edit| FileEdit { file: target, edit }),
    );
    moved.edits.push(FileEdit {
        file: target,
        edit: Edit::insertion(
            appended_text(
                target_text,
                target_parsed.suite(),
                &cut.text,
                declaration.kind,
                adds_imports,
                stylist.line_ending().as_str(),
            ),
            target_end,
        ),
    });
    if remove_from_all(parsed.suite(), name).is_some()
        && let Some(edit) = add_to_all(target_text, target_parsed.suite(), name)
    {
        moved.edits.push(FileEdit { file: target, edit });
    }

    // Every other module that refers to it.
    let mut files: Vec<File> = by_file.keys().copied().collect();
    files.sort_by_key(|file| file.path(db).as_str().to_string());
    for file in files {
        let ranges = &by_file[&file];
        rewrite_references(
            db,
            ProgramFile::new(db, file, source.program(db)),
            ranges,
            name,
            &source_module,
            &target_module,
            &mut moved,
        );
    }
    moved.edits = disjoint(db, moved.edits)?;
    Ok(moved)
}

/// `edits` without two that overlap, which an editor would refuse. each removal of a statement
/// takes the blank lines around it, so two removed next to each other both claim the lines
/// between them: overlapping removals are made one. an insertion inside a removal goes where the
/// removal starts, which keeps it in the same place among what is left. any other overlap is
/// refused
fn disjoint(db: &dyn Db, edits: Vec<FileEdit>) -> Result<Vec<FileEdit>, String> {
    let mut by_file: FxHashMap<File, Vec<Edit>> = FxHashMap::default();
    let mut files: Vec<File> = Vec::new();
    for edit in edits {
        if !by_file.contains_key(&edit.file) {
            files.push(edit.file);
        }
        by_file.entry(edit.file).or_default().push(edit.edit);
    }
    let mut disjoint = Vec::new();
    for file in files {
        let edits = by_file.remove(&file).unwrap_or_default();
        let (removals, others): (Vec<Edit>, Vec<Edit>) = edits
            .into_iter()
            .partition(|edit| edit.content().is_none_or(str::is_empty) && !edit.range().is_empty());
        let mut ranges: Vec<TextRange> = removals.iter().map(Ranged::range).collect();
        ranges.sort_by_key(|range| (range.start(), range.end()));
        let mut merged: Vec<TextRange> = Vec::new();
        for range in ranges {
            match merged.last_mut() {
                Some(last) if range.start() < last.end() => *last = last.cover(range),
                _ => merged.push(range),
            }
        }
        let mut edits: Vec<Edit> = merged.into_iter().map(Edit::range_deletion).collect();
        for edit in others {
            let inside = edits.iter().find_map(|removal| {
                (edit.range().is_empty()
                    && removal.start() < edit.start()
                    && edit.start() < removal.end())
                .then(|| removal.start())
            });
            edits.push(match inside {
                Some(start) => {
                    Edit::insertion(edit.content().unwrap_or_default().to_string(), start)
                }
                None => edit,
            });
        }
        let mut ranges: Vec<TextRange> = edits.iter().map(Ranged::range).collect();
        ranges.sort_by_key(|range| (range.start(), range.end()));
        if ranges
            .windows(2)
            .any(|pair| pair[0].end() > pair[1].start())
        {
            return Err(format!(
                "the move would make edits to {} that overlap",
                file.path(db)
            ));
        }
        disjoint.extend(edits.into_iter().map(|edit| FileEdit { file, edit }));
    }
    Ok(disjoint)
}

/// The file and offset of the top-level declaration named at `offset`: `offset` itself when it is
/// in a declaration's name, or else where the name at `offset` is defined, or else the name of the
/// top-level declaration `offset` is anywhere in, as a move from inside a definition's body moves
/// the definition
fn declaration_site<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    offset: TextSize,
) -> Option<(ProgramFile<'db>, TextSize)> {
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    if Declaration::at(parsed.suite(), offset).is_some() {
        return Some((file, offset));
    }
    if let Some(site) = referenced_declaration(db, file, offset) {
        return Some(site);
    }
    let declaration = Declaration::enclosing(parsed.suite(), offset)?;
    Some((file, declaration.name_range.start()))
}

/// the file and offset of the top-level declaration of the project the name at `offset` refers to
fn referenced_declaration<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    offset: TextSize,
) -> Option<(ProgramFile<'db>, TextSize)> {
    let targets = crate::goto_definition(db, file, offset)?;
    let mut targets = targets.value.into_iter();
    let target = targets.next()?;
    if targets.next().is_some() {
        return None;
    }
    let target_file = ProgramFile::new(db, target.file(), file.program(db));
    if !db.project().files(db).contains(target.file()) {
        return None;
    }
    let target_parsed = parsed_module(db, target_file.python_file(db)).load(db);
    let declaration = Declaration::at(target_parsed.suite(), target.focus_range().start())?;
    Some((target_file, declaration.name_range.start()))
}

/// A top-level statement that defines one name.
struct Declaration<'a> {
    index: usize,
    statement: &'a Stmt,
    name: &'a str,
    name_range: TextRange,
    kind: MovableKind,
}

impl<'a> Declaration<'a> {
    /// the top-level declaration `offset` is anywhere in: its decorators, its signature, its body
    /// or its value
    fn enclosing(suite: &'a [Stmt], offset: TextSize) -> Option<Self> {
        let statement = suite
            .iter()
            .find(|statement| statement.range().contains_inclusive(offset))?;
        let name = match statement {
            Stmt::FunctionDef(function) => function.name.start(),
            Stmt::ClassDef(class) => class.name.start(),
            Stmt::Assign(assign) => assign.targets.first()?.start(),
            Stmt::AnnAssign(assign) => assign.target.start(),
            _ => return None,
        };
        Self::at(suite, name)
    }

    /// The top-level declaration whose name `offset` is in.
    fn at(suite: &'a [Stmt], offset: TextSize) -> Option<Self> {
        suite.iter().enumerate().find_map(|(index, statement)| {
            let (name, kind) = match statement {
                Stmt::FunctionDef(function) => (&function.name, MovableKind::Function),
                Stmt::ClassDef(class) => (&class.name, MovableKind::Class),
                Stmt::Assign(assign) => match assign.targets.as_slice() {
                    [Expr::Name(target)] => {
                        return target.range().contains_inclusive(offset).then_some(Self {
                            index,
                            statement,
                            name: target.id.as_str(),
                            name_range: target.range(),
                            kind: MovableKind::Variable,
                        });
                    }
                    _ => return None,
                },
                Stmt::AnnAssign(assign) if assign.value.is_some() => match assign.target.as_ref() {
                    Expr::Name(target) => {
                        return target.range().contains_inclusive(offset).then_some(Self {
                            index,
                            statement,
                            name: target.id.as_str(),
                            name_range: target.range(),
                            kind: MovableKind::Variable,
                        });
                    }
                    _ => return None,
                },
                _ => return None,
            };
            name.range().contains_inclusive(offset).then_some(Self {
                index,
                statement,
                name: name.as_str(),
                name_range: name.range(),
                kind,
            })
        })
    }
}

/// An import request whose strings it owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OwnedRequest {
    pub(super) module: String,
    pub(super) member: Option<String>,
    pub(super) asname: Option<String>,
    /// The name the import binds.
    pub(super) binds: String,
}

impl OwnedRequest {
    pub(super) fn request(&self) -> ImportRequest<'_> {
        match (&self.member, &self.asname) {
            (Some(member), Some(asname)) => {
                ImportRequest::import_from_as(&self.module, member, asname)
            }
            (Some(member), None) => ImportRequest::import_from(&self.module, member),
            (None, Some(asname)) => ImportRequest::module_as(&self.module, asname),
            (None, None) => ImportRequest::module(&self.module),
        }
    }
}

/// A name a definition reads from the top level of its module.
#[derive(Debug)]
pub(super) enum Dependency {
    /// A name one of the module's imports binds, as the import that binds it.
    Imported {
        /// The start of the statement that imports it, or `None` for a star import, which binds
        /// more than this name and so is never dropped.
        binding_statement: Option<TextSize>,
        alias_index: usize,
        request: OwnedRequest,
    },
    /// Another of the module's own top-level names.
    Sibling(String),
}

/// The names `statement` reads from the top level of `file`, other than `name`, which it defines.
pub(super) fn dependencies(
    db: &dyn Db,
    file: ProgramFile<'_>,
    parsed: &ParsedModuleRef,
    statement: &Stmt,
    name: &str,
) -> Vec<Dependency> {
    let index = semantic_index(db, file);
    let globals = index.place_table(FileScopeId::global());
    let importing_file = ImportingFile::File(file.file(db), file.resolver_environment(db));
    let mut seen = FxHashSet::default();
    let mut dependencies = Vec::new();
    for load in loads(statement) {
        let id = load.id.as_str();
        if id == name || !seen.insert(id) {
            continue;
        }
        let Some(scope) = index.try_expression_scope_id(&ast::ExprRef::Name(load)) else {
            continue;
        };
        if resolving_scope(index, scope, id) != Some(FileScopeId::global()) {
            continue;
        }
        let Some(symbol) = globals.symbol_id(id) else {
            continue;
        };
        let bindings = bindings_of_symbol(
            db,
            index,
            parsed,
            FileScopeId::global(),
            symbol,
            file.file(db).is_stub(db),
        );
        let imported = bindings
            .iter()
            .find_map(|definition| match definition.kind(db) {
                DefinitionKind::Import(import) => {
                    let statement = import.import(parsed);
                    let alias = import.alias(parsed);
                    Some(Dependency::Imported {
                        binding_statement: Some(statement.start()),
                        alias_index: import.alias_index(),
                        request: OwnedRequest {
                            module: alias.name.to_string(),
                            member: None,
                            asname: alias.asname.as_ref().map(ToString::to_string),
                            binds: id.to_string(),
                        },
                    })
                }
                DefinitionKind::ImportFrom(import) => {
                    let statement = import.import(parsed);
                    let alias = import.alias(parsed);
                    let module =
                        ModuleName::from_import_statement(db, importing_file, statement).ok()?;
                    Some(Dependency::Imported {
                        binding_statement: Some(statement.start()),
                        alias_index: import.alias_index(),
                        request: OwnedRequest {
                            module: module.to_string(),
                            member: Some(alias.name.to_string()),
                            asname: alias.asname.as_ref().map(ToString::to_string),
                            binds: id.to_string(),
                        },
                    })
                }
                DefinitionKind::StarImport(import) => {
                    let statement = import.import(parsed);
                    let module =
                        ModuleName::from_import_statement(db, importing_file, statement).ok()?;
                    Some(Dependency::Imported {
                        binding_statement: None,
                        alias_index: 0,
                        request: OwnedRequest {
                            module: module.to_string(),
                            member: Some(id.to_string()),
                            asname: None,
                            binds: id.to_string(),
                        },
                    })
                }
                _ => None,
            });
        dependencies.push(imported.unwrap_or_else(|| Dependency::Sibling(id.to_string())));
    }
    dependencies
}

/// the names `statement` reads
fn loads(statement: &Stmt) -> Vec<&ast::ExprName> {
    struct Loads<'a> {
        names: Vec<&'a ast::ExprName>,
    }
    impl<'a> SourceOrderVisitor<'a> for Loads<'a> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Name(name) = expr
                && name.ctx.is_load()
            {
                self.names.push(name);
            }
            walk_expr(self, expr);
        }
    }
    let mut loads = Loads { names: Vec::new() };
    loads.visit_stmt(statement);
    loads.names
}

/// the names `statement` reads that no scope of `file` binds, the builtins among them
fn builtins_read(db: &dyn Db, file: ProgramFile<'_>, statement: &Stmt) -> Vec<String> {
    let index = semantic_index(db, file);
    let mut read: Vec<String> = Vec::new();
    for load in loads(statement) {
        let id = load.id.as_str();
        if let Some(scope) = index.try_expression_scope_id(&ast::ExprRef::Name(load))
            && resolving_scope(index, scope, id).is_none()
            && !read.iter().any(|seen| seen == id)
        {
            read.push(id.to_string());
        }
    }
    read
}

/// a name of the module's own that a `global` in a scope inside `range` assigns
fn global_written_by(db: &dyn Db, index: &SemanticIndex<'_>, range: TextRange) -> Option<String> {
    index
        .use_def_map(FileScopeId::global())
        .definitions_with_usage()
        .find_map(|(_, definition, _)| match definition.kind(db) {
            DefinitionKind::NestedBindings(nested) => nested
                .nested_declarations
                .iter()
                .any(|declaration| {
                    declaration.is_global()
                        && declaration.is_bound
                        && range.contains_range(declaration.range)
                })
                .then(|| nested.name.to_string()),
            _ => None,
        })
}

/// whether a module of the project other than `file` reads the name the import starting at
/// `statement` binds, alias `alias_index`, from `file`'s module: imports it from there, reads it
/// as an attribute of the module, or reads it through a star import of the module
fn imported_from(
    db: &dyn Db,
    file: ProgramFile<'_>,
    parsed: &ParsedModuleRef,
    model: &SemanticModel<'_>,
    statement: TextSize,
    alias_index: usize,
    module: &ModuleName,
) -> bool {
    let Some(alias) = parsed
        .suite()
        .iter()
        .find(|stmt| stmt.start() == statement)
        .and_then(|stmt| match stmt {
            Stmt::Import(import) => import.names.get(alias_index),
            Stmt::ImportFrom(from) => from.names.get(alias_index),
            _ => None,
        })
    else {
        // what cannot be looked up is kept
        return true;
    };
    let binding = alias.asname.as_ref().unwrap_or(&alias.name);
    let Some(goto) = find_goto_target(model, parsed, binding.start()) else {
        return true;
    };
    let file_id = file.file(db);
    let mut reads_by_file: FxHashMap<File, Vec<TextRange>> = FxHashMap::default();
    for reference in
        references(db, file, &goto, ReferencesMode::RenameMultiFile).unwrap_or_default()
    {
        if reference.file() != file_id {
            reads_by_file
                .entry(reference.file())
                .or_default()
                .push(reference.range());
        }
    }
    reads_by_file.into_iter().any(|(other, ranges)| {
        let other = ProgramFile::new(db, other, file.program(db));
        let parsed = parsed_module(db, other.python_file(db)).load(db);
        let model = SemanticModel::new(db, other);
        let importing_file = ImportingFile::File(other.file(db), other.resolver_environment(db));
        let from_module = |from: &ast::StmtImportFrom| {
            ModuleName::from_import_statement(db, importing_file, from)
                .is_ok_and(|imported| &imported == module)
        };
        let star_imports_module = parsed.suite().iter().any(|stmt| {
            matches!(stmt, Stmt::ImportFrom(from)
                if from.names.iter().any(|alias| alias.name.as_str() == "*") && from_module(from))
        });
        ranges.iter().any(|range| {
            let covering = covering_node(parsed.syntax().into(), *range);
            if let Some(from) = covering.ancestors().find_map(|node| match node {
                AnyNodeRef::StmtImportFrom(from) => Some(from),
                _ => None,
            }) {
                return from_module(from);
            }
            match covering
                .ancestors()
                .find(|node| !matches!(node, AnyNodeRef::Identifier(_)))
            {
                Some(AnyNodeRef::ExprAttribute(attribute)) => matches!(
                    attribute.value.inferred_type(&model),
                    Some(Type::ModuleLiteral(literal)) if literal.module(db).name(db) == module
                ),
                Some(AnyNodeRef::ExprName(_)) => star_imports_module,
                _ => false,
            }
        })
    })
}

/// Whether `name` is private to its module by python's convention: it starts with an underscore
/// and is not a dunder.
pub(super) fn is_private(name: &str) -> bool {
    name.starts_with('_') && !(name.starts_with("__") && name.ends_with("__"))
}

/// Whether `file` already imports what `request` asks for, under the name it asks for.
fn target_imports_same(
    db: &dyn Db,
    file: ProgramFile<'_>,
    parsed: &ParsedModuleRef,
    request: &OwnedRequest,
) -> bool {
    let importing_file = ImportingFile::File(file.file(db), file.resolver_environment(db));
    parsed.suite().iter().any(|stmt| match stmt {
        Stmt::Import(import) => {
            request.member.is_none()
                && import.names.iter().any(|alias| {
                    alias.name.as_str() == request.module
                        && alias.asname.as_ref().map(ToString::to_string) == request.asname
                })
        }
        Stmt::ImportFrom(from) => request.member.as_ref().is_some_and(|member| {
            ModuleName::from_import_statement(db, importing_file, from)
                .is_ok_and(|module| module.as_str() == request.module)
                && from.names.iter().any(|alias| {
                    alias.name.as_str() == member
                        && alias.asname.as_ref().map(ToString::to_string) == request.asname
                })
        }),
        _ => false,
    })
}

/// Whether a top-level import of `file` imports `module`, or something from it.
pub(super) fn imports_module(
    db: &dyn Db,
    file: ProgramFile<'_>,
    parsed: &ParsedModuleRef,
    module: &ModuleName,
) -> bool {
    let importing_file = ImportingFile::File(file.file(db), file.resolver_environment(db));
    parsed.suite().iter().any(|stmt| match stmt {
        Stmt::Import(import) => import
            .names
            .iter()
            .any(|alias| alias.name.as_str() == module.as_str()),
        Stmt::ImportFrom(from) => ModuleName::from_import_statement(db, importing_file, from)
            .is_ok_and(|imported| {
                &imported == module
                    || from.names.iter().any(|alias| {
                        format!("{imported}.{}", alias.name.as_str()) == module.as_str()
                    })
            }),
        _ => false,
    })
}

/// Whether `name` is read anywhere in the module outside `except`, or listed in its `__all__`.
fn used_elsewhere(parsed: &ParsedModuleRef, name: &str, except: TextRange) -> bool {
    struct Uses<'a> {
        name: &'a str,
        except: TextRange,
        found: bool,
    }
    impl<'a> SourceOrderVisitor<'a> for Uses<'_> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            match expr {
                Expr::Name(name)
                    if name.id.as_str() == self.name
                        && !self.except.contains_range(name.range()) =>
                {
                    self.found = true;
                }
                Expr::StringLiteral(string)
                    if string.value.to_str() == self.name
                        && !self.except.contains_range(string.range()) =>
                {
                    // a name listed in `__all__`, or otherwise spelled out, is kept
                    self.found = true;
                }
                _ => walk_expr(self, expr),
            }
        }
    }
    let mut uses = Uses {
        name,
        except,
        found: false,
    };
    uses.visit_body(parsed.suite());
    uses.found
}

/// The text a cut takes, and the range it removes from the module.
pub(super) struct Cut {
    /// The definition with its decorators and the comments directly above it, ending in a line
    /// break.
    pub(super) text: String,
    /// where `text` starts in the module
    pub(super) start: TextSize,
    /// Its lines and the blank lines that separated it from the statement after it, or, when it is
    /// the last statement, from the one before.
    pub(super) removed: TextRange,
}

pub(super) fn cut_range(source: &str, suite: &[Stmt], index: usize) -> Cut {
    let statement = &suite[index];
    let floor = index
        .checked_sub(1)
        .map_or(TextSize::default(), |previous| {
            source.full_line_end(suite[previous].end())
        });
    let mut start = source.line_start(statement.start());
    // the comments directly above it go with it
    while start > floor {
        let previous = source.line_start(start - TextSize::from(1));
        if !source[TextRange::new(previous, start)]
            .trim_start()
            .starts_with('#')
        {
            break;
        }
        start = previous;
    }
    let end = source.full_line_end(statement.end());
    let mut text = source[TextRange::new(start, end)].to_string();
    if !text.ends_with(['\n', '\r']) {
        text.push('\n');
    }

    let removed = if let Some(next) = suite.get(index + 1) {
        // up to the next statement, or the comments above it
        let mut next_start = source.line_start(next.start());
        while next_start > end {
            let previous = source.line_start(next_start - TextSize::from(1));
            if source[TextRange::new(previous, next_start)]
                .trim()
                .is_empty()
            {
                break;
            }
            next_start = previous;
        }
        let mut removed_end = end;
        while removed_end < next_start
            && source[TextRange::new(removed_end, source.full_line_end(removed_end))]
                .trim()
                .is_empty()
        {
            removed_end = source.full_line_end(removed_end);
        }
        TextRange::new(start, removed_end)
    } else {
        // the last statement of its suite takes the blank lines above it, and at the end of the
        // file the blank lines after it
        let mut removed_start = start;
        while removed_start > floor {
            let previous = source.line_start(removed_start - TextSize::from(1));
            if !source[TextRange::new(previous, removed_start)]
                .trim()
                .is_empty()
            {
                break;
            }
            removed_start = previous;
        }
        let removed_end = if source[end.to_usize()..].trim().is_empty() {
            TextSize::of(source)
        } else {
            end
        };
        TextRange::new(removed_start, removed_end)
    };
    Cut {
        text,
        start,
        removed,
    }
}

/// The text appended to the end of the target: the definition, set apart from what is above it
/// as PEP 8 sets apart a top-level definition.
fn appended_text(
    target: &str,
    suite: &[Stmt],
    definition: &str,
    kind: MovableKind,
    adds_imports: bool,
    line_ending: &str,
) -> String {
    let mut text = String::new();
    if !target.is_empty() && !target.ends_with(['\n', '\r']) {
        text.push_str(line_ending);
    }
    let trailing_blank_lines = target
        .lines()
        .rev()
        .take_while(|line| line.trim().is_empty())
        .count();
    let follows_a_variable = matches!(
        suite.last(),
        Some(Stmt::Assign(_) | Stmt::AnnAssign(_) | Stmt::AugAssign(_))
    );
    let blank_lines = if (suite.is_empty() && !adds_imports)
        || (kind == MovableKind::Variable && follows_a_variable)
    {
        0
    } else {
        2
    };
    for _ in trailing_blank_lines.min(blank_lines)..blank_lines {
        text.push_str(line_ending);
    }
    // the definition is written in the line ending of the module it was cut from
    for line in definition.universal_newlines() {
        text.push_str(line.as_str());
        if line.line_ending().is_some() {
            text.push_str(line_ending);
        }
    }
    text
}

/// The edit removing alias `alias_index` from the import statement starting at `statement`, or the
/// whole statement when it is its only alias.
fn remove_alias(
    source: &str,
    suite: &[Stmt],
    statement: TextSize,
    alias_index: usize,
) -> Option<Edit> {
    let (position, stmt) = suite
        .iter()
        .enumerate()
        .find(|(_, stmt)| stmt.start() == statement)?;
    let names = match stmt {
        Stmt::Import(import) => &import.names,
        Stmt::ImportFrom(from) => &from.names,
        _ => return None,
    };
    if names.len() == 1 {
        return Some(Edit::range_deletion(
            cut_range(source, suite, position).removed,
        ));
    }
    let alias = names.get(alias_index)?;
    let range = if let Some(next) = names.get(alias_index + 1) {
        TextRange::new(alias.start(), next.start())
    } else {
        TextRange::new(names[alias_index - 1].end(), alias.end())
    };
    Some(Edit::range_deletion(range))
}

/// The `__all__ = [...]` (or tuple) assignment at the top level of a module.
fn all_list(suite: &[Stmt]) -> Option<&[Expr]> {
    suite.iter().find_map(|stmt| {
        let (target, value) = match stmt {
            Stmt::Assign(assign) => (assign.targets.first()?, assign.value.as_ref()),
            Stmt::AnnAssign(assign) => (assign.target.as_ref(), assign.value.as_deref()?),
            _ => return None,
        };
        if !matches!(target, Expr::Name(name) if name.id.as_str() == "__all__") {
            return None;
        }
        match value {
            Expr::List(list) => Some(list.elts.as_slice()),
            Expr::Tuple(tuple) => Some(tuple.elts.as_slice()),
            _ => None,
        }
    })
}

/// The edit taking `name` out of the module's `__all__`, when it lists it.
fn remove_from_all(suite: &[Stmt], name: &str) -> Option<Edit> {
    let elements = all_list(suite)?;
    let position = elements.iter().position(
        |element| matches!(element, Expr::StringLiteral(string) if string.value.to_str() == name),
    )?;
    let element = &elements[position];
    let range = if let Some(next) = elements.get(position + 1) {
        TextRange::new(element.start(), next.start())
    } else if position > 0 {
        TextRange::new(elements[position - 1].end(), element.end())
    } else {
        element.range()
    };
    Some(Edit::range_deletion(range))
}

/// The edit adding `name` to the end of the module's `__all__`, when it has one.
fn add_to_all(source: &str, suite: &[Stmt], name: &str) -> Option<Edit> {
    let elements = all_list(suite)?;
    let quote = elements
        .first()
        .and_then(|element| source[element.range()].chars().next())
        .filter(|quote| matches!(quote, '"' | '\''))
        .unwrap_or('"');
    elements
        .last()
        .map(|last| Edit::insertion(format!(", {quote}{name}{quote}"), last.end()))
}

/// Points the references `ranges` in `file` at `name`'s new module.
fn rewrite_references(
    db: &dyn Db,
    file: ProgramFile<'_>,
    ranges: &[TextRange],
    name: &str,
    source_module: &ModuleName,
    target_module: &ModuleName,
    moved: &mut SymbolMove,
) {
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let text = source_text(db, file.file(db));
    let text = text.as_str();
    let stylist = Stylist::from_tokens(parsed.tokens(), text);
    let importer = import_order::importer(db, file, &parsed);
    let model = SemanticModel::new(db, file);
    let importing_file = ImportingFile::File(file.file(db), file.resolver_environment(db));
    let file_id = file.file(db);
    let path = file.file(db).path(db).as_str().to_string();

    let mut actions: Vec<ImportAction> = Vec::new();
    let mut imported_by_statement = false;
    let mut names = Vec::new();
    for range in ranges {
        let covering = covering_node(parsed.syntax().into(), *range);
        // the name of an attribute is an identifier of the attribute expression
        let node = match covering.node() {
            AnyNodeRef::Identifier(_) => match covering.parent() {
                Some(attribute @ AnyNodeRef::ExprAttribute(_)) => attribute,
                _ => covering.node(),
            },
            node => node,
        };
        match node {
            // `from module import name`, as a statement of its own or among others
            AnyNodeRef::Alias(_) | AnyNodeRef::Identifier(_) => {
                let Some(from) = covering.ancestors().find_map(|node| match node {
                    AnyNodeRef::StmtImportFrom(from) => Some(from),
                    _ => None,
                }) else {
                    continue;
                };
                imported_by_statement = true;
                let Ok(module) = ModuleName::from_import_statement(db, importing_file, from) else {
                    continue;
                };
                if &module != source_module {
                    // imported through a module that re-exports it, which keeps doing so
                    continue;
                }
                let Some((alias_index, alias)) = from
                    .names
                    .iter()
                    .enumerate()
                    .find(|(_, alias)| alias.name.range() == *range)
                else {
                    continue;
                };
                if from.names.len() == 1 {
                    if let Some(module_range) = from_module_range(&parsed, from) {
                        moved.edits.push(FileEdit {
                            file: file_id,
                            edit: Edit::range_replacement(target_module.to_string(), module_range),
                        });
                    }
                } else {
                    let range = if let Some(next) = from.names.get(alias_index + 1) {
                        TextRange::new(alias.start(), next.start())
                    } else {
                        TextRange::new(from.names[alias_index - 1].end(), alias.end())
                    };
                    moved.edits.push(FileEdit {
                        file: file_id,
                        edit: Edit::range_deletion(range),
                    });
                    let request = match &alias.asname {
                        Some(asname) => ImportRequest::import_from_as(
                            target_module.as_str(),
                            name,
                            asname.as_str(),
                        ),
                        None => ImportRequest::import_from(target_module.as_str(), name),
                    };
                    if parsed.suite().iter().any(|stmt| stmt.range() == from.range) {
                        actions.push(importer.import(request, &MembersInScope::empty(from.end())));
                    } else {
                        // an import inside a function: the new one goes on the line after it
                        // when it is a line of its own, and after it on its line otherwise,
                        // which keeps it in the same block and before anything after it
                        let statement = match &alias.asname {
                            Some(asname) => {
                                format!("from {target_module} import {name} as {asname}")
                            }
                            None => format!("from {target_module} import {name}"),
                        };
                        let tokens = parsed.tokens();
                        let starts_line = tokens.before(from.start()).last().is_none_or(|token| {
                            matches!(
                                token.kind(),
                                TokenKind::Newline
                                    | TokenKind::NonLogicalNewline
                                    | TokenKind::Indent
                                    | TokenKind::Dedent
                            )
                        });
                        let ends_line = tokens.after(from.end()).first().is_none_or(|token| {
                            matches!(
                                token.kind(),
                                TokenKind::Newline | TokenKind::Comment | TokenKind::EndOfFile
                            )
                        });
                        let edit = if starts_line && ends_line {
                            let indentation =
                                &text[TextRange::new(text.line_start(from.start()), from.start())];
                            Edit::insertion(
                                format!(
                                    "{indentation}{statement}{}",
                                    stylist.line_ending().as_str()
                                ),
                                text.full_line_end(from.end()),
                            )
                        } else {
                            Edit::insertion(format!("; {statement}"), from.end())
                        };
                        moved.edits.push(FileEdit {
                            file: file_id,
                            edit,
                        });
                    }
                }
            }
            // `module.name`, through an import of the module
            AnyNodeRef::ExprAttribute(attribute)
                if attribute.attr.range() == *range
                    && matches!(
                        attribute.value.inferred_type(&model),
                        Some(Type::ModuleLiteral(literal)) if literal.module(db).name(db) == source_module
                    ) =>
            {
                let action = importer.import(
                    ImportRequest::import(target_module.as_str(), name),
                    &MembersInScope::empty(attribute.start()),
                );
                moved.edits.push(FileEdit {
                    file: file_id,
                    edit: Edit::range_replacement(
                        action.symbol_text().to_string(),
                        attribute.range(),
                    ),
                });
                actions.push(action);
            }
            AnyNodeRef::ExprName(_) => names.push(*range),
            _ => {
                let line = line_number(text, range.start());
                moved.warnings.push(format!(
                    "{path}:{line} refers to `{name}` in a way the move leaves as it is"
                ));
            }
        }
    }

    // a name the file reads without importing it by name reaches it through a star import
    if !imported_by_statement && let Some(first) = names.iter().map(Ranged::start).min() {
        let star = parsed.suite().iter().any(|stmt| {
            matches!(stmt, Stmt::ImportFrom(from)
                if from.names.iter().any(|alias| alias.name.as_str() == "*")
                    && ModuleName::from_import_statement(db, importing_file, from)
                        .is_ok_and(|module| &module == source_module))
        });
        if star {
            actions.push(importer.import(
                ImportRequest::import_from(target_module.as_str(), name),
                &MembersInScope::empty(first),
            ));
        } else {
            let line = line_number(text, first);
            moved.warnings.push(format!(
                "{path}:{line} reaches `{name}` through a module that will no longer provide it"
            ));
        }
    }

    moved.edits.extend(
        importer
            .combined_edits(&actions)
            .into_iter()
            .map(|edit| FileEdit {
                file: file_id,
                edit,
            }),
    );
}

/// the range of the module a `from` import names, its leading dots included: from the token
/// after `from` to the end of the module's name, or of its dots when it has none
fn from_module_range(parsed: &ParsedModuleRef, from: &ast::StmtImportFrom) -> Option<TextRange> {
    let mut tokens = parsed.tokens().in_range(from.range()).iter();
    let keyword = tokens.next()?;
    if keyword.kind() != TokenKind::From {
        return None;
    }
    let first = tokens.next()?;
    let end = match &from.module {
        Some(module) => module.end(),
        None => std::iter::once(first)
            .chain(tokens)
            .take_while(|token| matches!(token.kind(), TokenKind::Dot | TokenKind::Ellipsis))
            .last()?
            .end(),
    };
    Some(TextRange::new(first.start(), end))
}

/// The one-based line `offset` is on.
pub(super) fn line_number(source: &str, offset: TextSize) -> usize {
    source[..offset.to_usize()].matches('\n').count() + 1
}

#[cfg(test)]
pub(super) mod tests {
    use std::fmt::Write as _;

    use insta::assert_snapshot;
    use ruff_db::files::system_path_to_file;
    use ruff_db::source::source_text;
    use ruff_text_size::Ranged;

    use super::move_symbol;
    use crate::tests::{CursorTest, CursorTestBuilder};

    const SRC: &str = r#"import math
from collections import OrderedDict

__all__ = ["RATE", "area", "helper"]

RATE = 2
_SECRET = 3


def helper(x):
    return x * RATE


@staticmethod
def area(r):
    """Area of a circle."""
    return math.pi * r * r * RATE


def uses_private(x):
    return x + _SECRET


def free(x):
    return OrderedDict(x=x)
"#;

    const DST: &str = r#""""Destination."""

import os

X = os.sep
"#;

    /// A project of the modules above and modules using `area`, with the cursor where `src`
    /// gives it, in `mv/src.py`.
    fn project(src: &str) -> CursorTest {
        CursorTestBuilder::default()
            .source("mv/__init__.py", "")
            .source("mv/src.py", src)
            .source("mv/dst.py", DST)
            .source(
                "mv/user1.py",
                "from mv.src import area, helper\n\nprint(area(1), helper(2))\n",
            )
            .source("mv/user2.py", "import mv.src\n\nprint(mv.src.area(1))\n")
            .source("mv/user3.py", "from mv import src\n\nprint(src.area(1))\n")
            .source("mv/user4.py", "from mv.src import *\n\nprint(area(1))\n")
            .source(
                "mv/user5.py",
                "from mv.src import area as a\n\nprint(a(1))\n",
            )
            .build()
    }

    /// every file `moved` changes, as it is afterwards, and its warnings; or why it refused
    pub(in crate::refactor) fn rendered(
        db: &ty_project::TestDb,
        moved: Result<super::SymbolMove, String>,
    ) -> String {
        let moved = match moved {
            Ok(moved) => moved,
            Err(reason) => return format!("refused: {reason}"),
        };
        let mut files: Vec<_> = moved.edits.iter().map(|edit| edit.file).collect();
        files.sort_by_key(|file| file.path(db).as_str().to_string());
        files.dedup();
        let mut out = String::new();
        for file in files {
            let mut text = source_text(db, file).as_str().to_string();
            let mut edits: Vec<_> = moved
                .edits
                .iter()
                .filter(|edit| edit.file == file)
                .enumerate()
                .collect();
            let mut ranges: Vec<_> = edits.iter().map(|(_, edit)| edit.edit.range()).collect();
            ranges.sort_by_key(|range| (range.start(), range.end()));
            for pair in ranges.windows(2) {
                assert!(
                    pair[0].end() <= pair[1].start(),
                    "edits to {} overlap, which an editor refuses: {:?} and {:?}",
                    file.path(db),
                    pair[0],
                    pair[1]
                );
            }
            // applied from the end, a removal before an insertion where it starts; of two
            // insertions at one place, the one listed first ends up first
            edits.sort_by_key(|(order, edit)| {
                (
                    std::cmp::Reverse(edit.edit.start()),
                    std::cmp::Reverse(edit.edit.end()),
                    std::cmp::Reverse(*order),
                )
            });
            for (_, edit) in edits {
                text.replace_range(
                    edit.edit.range().to_std_range(),
                    edit.edit.content().unwrap_or_default(),
                );
            }
            let _ = write!(out, "--- {}\n{text}", file.path(db));
        }
        for warning in moved.warnings {
            let _ = writeln!(out, "warning: {warning}");
        }
        out
    }

    impl CursorTest {
        /// Every module the move to `mv/dst.py` changes, as it is afterwards, or why it refuses.
        fn move_to_dst(&self) -> String {
            let target = system_path_to_file(&self.db, "mv/dst.py").expect("dst to exist");
            let moved = salsa::attach(&self.db, || {
                move_symbol(
                    &self.db,
                    self.program_file(self.cursor.file),
                    self.cursor.offset,
                    target,
                )
            });
            rendered(&self.db, moved)
        }
    }

    /// The definition goes to the end of the target with the import it uses and the name of
    /// its old module it reads; the old module drops the import nothing else uses and its
    /// `__all__` entry; and every way the project refers to it follows it.
    #[test]
    fn a_function_moves_with_what_it_reads() {
        let test = project(&SRC.replace("def area(r)", "def <CURSOR>area(r)"));
        assert_snapshot!(test.move_to_dst(), @r#"
        --- /mv/dst.py
        """Destination."""

        import math
        import os

        from mv.src import RATE

        X = os.sep


        @staticmethod
        def area(r):
            """Area of a circle."""
            return math.pi * r * r * RATE
        --- /mv/src.py
        from collections import OrderedDict

        __all__ = ["RATE", "helper"]

        RATE = 2
        _SECRET = 3


        def helper(x):
            return x * RATE


        def uses_private(x):
            return x + _SECRET


        def free(x):
            return OrderedDict(x=x)
        --- /mv/user1.py
        from mv.dst import area
        from mv.src import helper

        print(area(1), helper(2))
        --- /mv/user2.py
        import mv.dst
        import mv.src

        print(mv.dst.area(1))
        --- /mv/user3.py
        import mv.dst
        from mv import src

        print(mv.dst.area(1))
        --- /mv/user4.py
        from mv.dst import area
        from mv.src import *

        print(area(1))
        --- /mv/user5.py
        from mv.dst import area as a

        print(a(1))
        "#);
    }

    /// The old module still calls it, and it reads `RATE` from the old module, so the two
    /// modules would import each other, which python cannot load.
    #[test]
    fn a_move_that_makes_two_modules_import_each_other_is_refused() {
        let test = project(&format!(
            "{}\n\ndef user():\n    return area(2)\n",
            SRC.replace("def area(r)", "def <CURSOR>area(r)")
        ));
        assert_snapshot!(test.move_to_dst(), @"refused: mv.src and mv.dst would import each other: `area` uses `RATE`, and mv.src still uses `area`");
    }

    /// anywhere in a definition that is not a name of something else, a move moves the
    /// definition, as it does from its name
    #[test]
    fn a_move_from_inside_a_body_moves_the_definition() {
        let inside =
            project(&SRC.replace("    return OrderedDict", "    re<CURSOR>turn OrderedDict"));
        let at_name = project(&SRC.replace("def free(x)", "def <CURSOR>free(x)"));
        assert_eq!(inside.move_to_dst(), at_name.move_to_dst());
        assert!(inside.move_to_dst().contains("def free(x)"));
    }

    /// a definition moved out of a module written with `\r\n` line endings into one written with
    /// `\n` is written with `\n`, as the rest of the target is
    #[test]
    fn a_moved_definition_takes_the_targets_line_endings() {
        let test = project(
            &SRC.replace("def free(x)", "def <CURSOR>free(x)")
                .replace('\n', "\r\n"),
        );
        let moved = test.move_to_dst();
        let dst_path = system_path_to_file(&test.db, "mv/dst.py")
            .expect("dst to exist")
            .path(&test.db)
            .to_string();
        let dst = moved
            .split("--- ")
            .find(|file| file.starts_with(&dst_path))
            .expect("the move to change mv/dst.py");
        assert!(
            dst.contains("def free(x):\n    return OrderedDict(x=x)\n"),
            "{dst:?}"
        );
        assert!(!dst.contains('\r'), "{dst:?}");
    }

    #[test]
    fn a_definition_that_reads_a_private_name_is_refused() {
        let test = project(&SRC.replace("def uses_private", "def <CURSOR>uses_private"));
        assert_snapshot!(test.move_to_dst(), @"refused: `uses_private` uses `_SECRET`, which is private to mv.src; make it public, or move it too");
    }

    /// A variable the old module still reads is imported back from its new module.
    #[test]
    fn a_variable_the_old_module_reads_is_imported_back() {
        let test = project(&SRC.replace("RATE = 2", "<CURSOR>RATE = 2"));
        assert_snapshot!(test.move_to_dst(), @r#"
        --- /mv/dst.py
        """Destination."""

        import os

        X = os.sep
        RATE = 2
        --- /mv/src.py
        import math
        from collections import OrderedDict

        from mv.dst import RATE

        __all__ = ["area", "helper"]

        _SECRET = 3


        def helper(x):
            return x * RATE


        @staticmethod
        def area(r):
            """Area of a circle."""
            return math.pi * r * r * RATE


        def uses_private(x):
            return x + _SECRET


        def free(x):
            return OrderedDict(x=x)
        "#);
    }

    /// Asked at a reference, the definition the reference is to is moved.
    #[test]
    fn asked_at_a_reference() {
        let test = CursorTestBuilder::default()
            .source("mv/__init__.py", "")
            .source("mv/src.py", SRC)
            .source("mv/dst.py", DST)
            .source(
                "mv/user.py",
                "from mv.src import helper\n\nprint(<CURSOR>helper(2))\n",
            )
            .build();
        assert_snapshot!(test.move_to_dst(), @r#"
        --- /mv/dst.py
        """Destination."""

        import os

        from mv.src import RATE

        X = os.sep


        def helper(x):
            return x * RATE
        --- /mv/src.py
        import math
        from collections import OrderedDict

        __all__ = ["RATE", "area"]

        RATE = 2
        _SECRET = 3


        @staticmethod
        def area(r):
            """Area of a circle."""
            return math.pi * r * r * RATE


        def uses_private(x):
            return x + _SECRET


        def free(x):
            return OrderedDict(x=x)
        --- /mv/user.py
        from mv.dst import helper

        print(helper(2))
        "#);
    }

    #[test]
    fn a_name_the_target_binds_is_refused() {
        let test = CursorTestBuilder::default()
            .source("mv/__init__.py", "")
            .source("mv/src.py", "def <CURSOR>f():\n    pass\n")
            .source("mv/dst.py", "f = 1\n")
            .build();
        assert_snapshot!(test.move_to_dst(), @"refused: mv.dst has a `f` already");
    }

    /// a project of `mv/src.py`, `mv/dst.py` and the modules `others` names, by path and source
    fn modules(src: &str, dst: &str, others: &[(&str, &str)]) -> CursorTest {
        let mut builder = CursorTestBuilder::default();
        builder
            .source("mv/__init__.py", "")
            .source("mv/src.py", src)
            .source("mv/dst.py", dst);
        for (path, source) in others {
            builder.source(*path, source);
        }
        builder.build()
    }

    /// the import the definition takes with it and the definition itself are cut together, with
    /// the blank lines between them, which both cuts would otherwise claim
    #[test]
    fn an_import_and_the_definition_after_it_are_cut_together() {
        let test = modules(
            "import math\n\n\ndef <CURSOR>area(r): return math.pi*r*r\n",
            "",
            &[],
        );
        assert_snapshot!(test.move_to_dst(), @"
        --- /mv/dst.py
        import math


        def area(r): return math.pi*r*r
        --- /mv/src.py
        ");
    }

    /// another module imports the name from the old module, so the import stays
    #[test]
    fn an_import_another_module_imports_from_the_old_one_is_kept() {
        let test = modules(
            &SRC.replace("def free(x)", "def <CURSOR>free(x)"),
            DST,
            &[(
                "mv/user.py",
                "from mv.src import OrderedDict\n\nprint(OrderedDict())\n",
            )],
        );
        assert_snapshot!(test.move_to_dst(), @r#"
        --- /mv/dst.py
        """Destination."""

        import os
        from collections import OrderedDict

        X = os.sep


        def free(x):
            return OrderedDict(x=x)
        --- /mv/src.py
        import math
        from collections import OrderedDict

        __all__ = ["RATE", "area", "helper"]

        RATE = 2
        _SECRET = 3


        def helper(x):
            return x * RATE


        @staticmethod
        def area(r):
            """Area of a circle."""
            return math.pi * r * r * RATE


        def uses_private(x):
            return x + _SECRET
        "#);
    }

    /// a module that imports the name from where the old module does needs nothing from the old
    /// module, so the import goes
    #[test]
    fn an_import_another_module_makes_itself_is_dropped() {
        let test = modules(
            "from collections import OrderedDict\n\n\ndef <CURSOR>free(x):\n    return OrderedDict(x=x)\n",
            "",
            &[(
                "mv/user.py",
                "from collections import OrderedDict\n\nprint(OrderedDict())\n",
            )],
        );
        assert_snapshot!(test.move_to_dst(), @"
        --- /mv/dst.py
        from collections import OrderedDict


        def free(x):
            return OrderedDict(x=x)
        --- /mv/src.py
        ");
        let star = modules(
            "from collections import OrderedDict\n\n\ndef <CURSOR>free(x):\n    return OrderedDict(x=x)\n",
            "",
            &[(
                "mv/user.py",
                "from mv.src import *\n\nprint(OrderedDict())\n",
            )],
        );
        assert_snapshot!(star.move_to_dst(), @"
        --- /mv/dst.py
        from collections import OrderedDict


        def free(x):
            return OrderedDict(x=x)
        --- /mv/src.py
        from collections import OrderedDict
        ");
    }

    /// moved, `global COUNT` would name the target's `COUNT`, not the one the old module reads
    #[test]
    fn a_definition_that_assigns_a_global_of_its_module_is_refused() {
        let test = modules(
            "COUNT = 0\n\n\ndef <CURSOR>bump():\n    global COUNT\n    COUNT += 1\n",
            "",
            &[],
        );
        assert_snapshot!(test.move_to_dst(), @"refused: `bump` assigns mv.src's `COUNT` through `global`, which would name mv.dst's once moved");
    }

    /// an import of a name the old module rebinds would keep the value it had when imported
    #[test]
    fn a_definition_that_reads_a_name_its_module_rebinds_is_refused() {
        let test = modules(
            "COUNT = 0\n\n\ndef bump():\n    global COUNT\n    COUNT += 1\n\n\ndef <CURSOR>show():\n    return COUNT\n",
            "",
            &[],
        );
        assert_snapshot!(test.move_to_dst(), @"refused: `show` uses `COUNT`, which mv.src binds more than once, and an import of it would keep only the value it had when imported");
    }

    /// the target's own `len` would be read in place of the builtin
    #[test]
    fn a_builtin_the_target_binds_is_refused() {
        let test = modules(
            "def <CURSOR>size(x):\n    return len(x)\n",
            "len = 3\n",
            &[],
        );
        assert_snapshot!(test.move_to_dst(), @"refused: `size` uses the builtin `len`, which mv.dst binds to something else");
    }

    /// the module of an import is found by its tokens, so a comment that says `import` is not
    /// taken for the keyword
    #[test]
    fn an_import_with_a_comment_is_pointed_at_the_new_module() {
        let test = modules(
            "def <CURSOR>area(r):\n    return r * r\n",
            "",
            &[(
                "mv/user.py",
                "from mv.src import (  # import area\n    area,\n)\n\nprint(area(1))\n",
            )],
        );
        assert_snapshot!(test.move_to_dst(), @"
        --- /mv/dst.py
        def area(r):
            return r * r
        --- /mv/src.py
        --- /mv/user.py
        from mv.dst import (  # import area
            area,
        )

        print(area(1))
        ");
    }

    /// an import inside a function that does not start its line gets the new import after it,
    /// on the same line
    #[test]
    fn an_import_that_shares_its_line_is_followed_on_that_line() {
        let test = modules(
            "def <CURSOR>area(r):\n    return r * r\n\n\ndef helper():\n    return 1\n",
            "",
            &[(
                "mv/user.py",
                "def g():\n    x = 1; from mv.src import area, helper\n    return area(x) + helper()\n\n\ndef h():\n    from mv.src import area, helper\n    return area(helper())\n",
            )],
        );
        assert_snapshot!(test.move_to_dst(), @"
        --- /mv/dst.py
        def area(r):
            return r * r
        --- /mv/src.py
        def helper():
            return 1
        --- /mv/user.py
        def g():
            x = 1; from mv.src import helper; from mv.dst import area
            return area(x) + helper()


        def h():
            from mv.src import helper
            from mv.dst import area
            return area(helper())
        ");
    }
}
