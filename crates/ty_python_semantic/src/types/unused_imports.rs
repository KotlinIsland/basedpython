//! basedpython: `unused-import`, the checker's half of pyflakes' unused-import rule for
//! `.by` files
//!
//! whether an import in a `.by` file is used is a question about types as well as names.
//! `import colours` may be written only for the `extension Enum:` block `colours` declares,
//! and then nothing reads the name `colours` — the import is used because a member access
//! resolved through an extension it makes applicable. the linter answers per file and has no
//! types, so the rule lives here, where the file's inference has recorded every extension
//! and conformance it relied on, and buff's `F401` leaves `.by` files alone
//!
//! everything else is `F401`'s rule, read in the checker's model of the file:
//!
//! - a read of a binding counts for every later binding of the name in the scope. an import
//!   nothing reads before another import rebinds the name is reported too, which is the
//!   redefinition `F811` reports, so that one round of fixes removes all of them
//! - a `global` or `nonlocal` name binds in another scope, and is never reported
//! - `import a as a`, `from a import b as b` and `from a export b` re-export on purpose
//! - `from __future__ import …` is a compiler directive rather than a binding
//! - a module-scope name listed in `__all__` is exported, which is a use
//! - in a package's `__init__.by` an unused import may be an intended re-export, so it is
//!   reported without a fix, and a first-party one says how to make the re-export explicit
//! - an import inside a `try` that handles `ImportError` probes for the module, so it is
//!   reported without a fix, and says how to probe without importing

use ruff_db::files::File;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_python_ast::identifier::Identifier;
use ruff_python_ast::name::Name;
use ruff_python_ast::statement_visitor::{StatementVisitor, walk_stmt};
use ruff_python_ast::{self as ast};
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::FxHashSet;
use ty_module_resolver::{ImportingFile, ModuleName, resolve_module};
use ty_python_core::definition::{Definition, DefinitionCategory, DefinitionKind};
use ty_python_core::place::ScopedPlaceId;
use ty_python_core::scope::{FileScopeId, ScopeKind};
use ty_python_core::symbol::ScopedSymbolId;
use ty_python_core::{ProgramFile, ScopedDefinitionId, SemanticIndex, semantic_index};

use crate::dunder_all::dunder_all_names;
use crate::lint::LintId;
use crate::place::builtins_symbol;
use crate::reachability::is_reachable;
use crate::types::context::InferContext;
use crate::types::definition_resolution::user_visible_definitions;
use crate::types::diagnostic::{TypeCheckDiagnostics, UNUSED_IMPORT};
use crate::types::extensions::{extension_granting_imports, extensions_in_module};
use crate::types::{ProgramEnvironment, Type};
use crate::{Db, FxIndexMap};

/// report every unused import in `file`. `inferred` holds the diagnostics of the whole file's
/// inference, whose record of the extensions it relied on decides the imports no name is
/// read through
pub(crate) fn report_unused_imports(
    db: &dyn Db,
    file: ProgramFile<'_>,
    inferred: &TypeCheckDiagnostics,
    diagnostics: &mut TypeCheckDiagnostics,
) {
    let source_file = file.file(db);
    if !source_file.source_type(db).is_basedpython() {
        return;
    }
    if db
        .rule_selection(source_file)
        .get(LintId::of(&UNUSED_IMPORT))
        .is_none()
    {
        return;
    }
    let env = ProgramEnvironment::from_file(file);
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    // a file that does not parse may have lost a read to the parser's recovery, and `F401`
    // does not check one either
    if parsed.has_syntax_errors() {
        return;
    }

    let index = semantic_index(db, file);
    let uses = Uses::new(db, file, index, &parsed, inferred);
    let context = ImportContext::new(db, file, &env, &parsed);

    for scope in index.scope_ids() {
        let file_scope = scope.file_scope_id(db);
        let kind = index.scope(file_scope).kind();
        if !matches!(kind, ScopeKind::Module | ScopeKind::Function) {
            continue;
        }
        let unused = unused_in_scope(db, index, file_scope, &parsed, &uses, source_file);
        if unused.is_empty() {
            continue;
        }
        let infer_context = InferContext::new(db, &env, scope, source_file, file, &parsed);
        for import in &unused {
            import.report(&infer_context, &context, kind == ScopeKind::Module);
        }
        diagnostics.extend(&infer_context.finish());
    }
}

/// every read of a binding the file makes, from both the semantic index and inference
struct Uses<'db> {
    /// the definitions a name expression, a nested scope or a loop reads
    definitions: FxHashSet<Definition<'db>>,
    /// the imports whose module supplied an extension or a conformance inference relied on
    granting: FxHashSet<Definition<'db>>,
    /// names inference read as every binding their scope makes, rather than at a position
    /// the semantic index records: a receiver callable read as a method, and a name in an
    /// annotation, which is evaluated lazily
    scope_reads: FxHashSet<(FileScopeId, ScopedSymbolId)>,
    /// module-scope names read as a whole rather than at a position: an `__all__` entry,
    /// and the type an `extension` header names
    module_names: FxHashSet<Name>,
}

impl<'db> Uses<'db> {
    fn new(
        db: &'db dyn Db,
        file: ProgramFile<'db>,
        index: &'db SemanticIndex<'db>,
        parsed: &ParsedModuleRef,
        inferred: &TypeCheckDiagnostics,
    ) -> Self {
        let source_file = file.file(db);

        let read = index.scope_ids().flat_map(|scope| {
            index
                .use_def_map(scope.file_scope_id(db))
                .definitions_with_usage()
                .filter_map(|(_, definition, is_used)| is_used.then_some(definition))
        });
        // a read of a synthetic definition reads the definitions it stands for
        let mut definitions: FxHashSet<Definition<'db>> =
            user_visible_definitions(db, read).into_iter().collect();
        // a read of a loop's header reads every binding live at the top of the loop, which
        // is how a use earlier in the body reaches a binding later in it
        for scope in index.scope_ids() {
            let use_def = index.use_def_map(scope.file_scope_id(db));
            for (_, definition, is_used) in use_def.definitions_with_usage() {
                if !is_used {
                    continue;
                }
                let DefinitionKind::LoopHeader(header) = definition.kind(db) else {
                    continue;
                };
                let loop_header = use_def.loop_header(header.loop_header_id());
                for live in loop_header.bindings_for_place(header.place()) {
                    if is_reachable(db, use_def, live.reachability_constraint())
                        && let Some(binding) = use_def.definition(live.binding()).definition()
                    {
                        definitions.insert(binding);
                    }
                }
            }
        }

        let granting = extension_granting_imports(db, source_file)
            .iter()
            .filter(|grant| inferred.is_extension_module_used(grant.module))
            .map(|grant| grant.import)
            .collect();

        let mut module_names: FxHashSet<Name> = dunder_all_names(db, file)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        let mut literal_all = DunderAllLiterals::default();
        literal_all.visit_body(parsed.suite());
        module_names.extend(literal_all.names);
        // an extension resolves the type it names in its module's globals, as a whole —
        // see `extended_class`
        module_names.extend(
            extensions_in_module(db, source_file)
                .iter()
                .map(|extension| extension.name(db).clone()),
        );

        Self {
            definitions,
            granting,
            scope_reads: inferred.scope_reads().collect(),
            module_names,
        }
    }

    fn reads(&self, definition: Definition<'db>) -> bool {
        self.definitions.contains(&definition) || self.granting.contains(&definition)
    }
}

/// an import binding that nothing reads
struct UnusedImport {
    /// the statement the binding is one alias of
    statement: TextRange,
    /// the name the import makes available, as F401 spells it: `a.b`, `a.b.c` for
    /// `from a.b import c`, `.c` for `from . import c`
    qualified_name: String,
    /// the module the import names, for deciding whether it is first party
    module: Option<ModuleName>,
    /// the bound name, the range the diagnostic is reported on
    binding: TextRange,
}

fn unused_in_scope<'db>(
    db: &'db dyn Db,
    index: &'db SemanticIndex<'db>,
    scope: FileScopeId,
    parsed: &ParsedModuleRef,
    uses: &Uses<'db>,
    file: File,
) -> Vec<UnusedImport> {
    let is_stub = file.is_stub(db);
    let place_table = index.place_table(scope);
    let use_def = index.use_def_map(scope);

    // what happens to each name over the scope, in the order the scope does it: a binding,
    // or a `del` (or the end of an `except … as` clause) that unbinds it
    let mut history: FxIndexMap<
        ScopedSymbolId,
        Vec<(ScopedDefinitionId, Option<Definition<'db>>)>,
    > = FxIndexMap::default();
    for (id, definition, _) in use_def.definitions_with_usage() {
        let ScopedPlaceId::Symbol(symbol) = definition.place(db) else {
            continue;
        };
        let kind = definition.kind(db);
        // a definition the source does not spell — a loop header, the submodule binding
        // `from .x import y` makes in a package — stands for something written elsewhere
        if !kind.is_user_visible() || matches!(kind, DefinitionKind::ImportFromSubmodule(_)) {
            continue;
        }
        // a bare annotation declares a type and binds nothing
        if matches!(
            kind.category(is_stub, parsed),
            DefinitionCategory::Declaration
        ) {
            continue;
        }
        history
            .entry(symbol)
            .or_default()
            .push((id, Some(definition)));
    }
    for (id, place) in use_def.deletions() {
        if let ScopedPlaceId::Symbol(symbol) = place
            && let Some(events) = history.get_mut(&symbol)
        {
            events.push((id, None));
        }
    }

    let mut unused = Vec::new();
    for (symbol_id, mut events) in history {
        let symbol = place_table.symbol(symbol_id);
        if symbol.is_global() || symbol.is_nonlocal() {
            continue;
        }
        events.sort_unstable_by_key(|(id, _)| *id);
        // the bindings since the name was last unbound: the one it is left holding, and the
        // ones it shadows. a read of any of them counts for the last, as it does for `F401`
        let since_unbound = match events.iter().rposition(|(_, event)| event.is_none()) {
            Some(deletion) => &events[deletion + 1..],
            None => &events[..],
        };
        // a name left unbound at the end of the scope holds no import
        let Some(&(_, Some(live))) = since_unbound.last() else {
            continue;
        };
        let Some(import) = reportable_import(db, live, parsed) else {
            continue;
        };
        if since_unbound
            .iter()
            .filter_map(|(_, definition)| *definition)
            .any(|definition| uses.reads(definition))
        {
            continue;
        }
        if uses.scope_reads.contains(&(scope, symbol_id)) {
            continue;
        }
        if scope.is_global() && uses.module_names.contains(symbol.name()) {
            continue;
        }
        unused.push(import);
    }
    unused.sort_unstable_by_key(|import| import.binding.start());
    unused
}

/// `definition` as an import F401 reports when nothing reads it: not a `__future__`
/// directive, and not an explicit re-export
fn reportable_import<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    parsed: &ParsedModuleRef,
) -> Option<UnusedImport> {
    match definition.kind(db) {
        DefinitionKind::Import(import) => {
            let statement = import.import(parsed);
            let alias = import.alias(parsed);
            if alias
                .asname
                .as_ref()
                .is_some_and(|asname| asname.id == alias.name.id)
            {
                return None;
            }
            Some(UnusedImport {
                statement: statement.range(),
                qualified_name: alias.name.to_string(),
                module: (!alias.is_resource)
                    .then(|| ModuleName::new(&alias.name))
                    .flatten(),
                binding: alias.identifier(),
            })
        }
        DefinitionKind::ImportFrom(import) => {
            let statement = import.import(parsed);
            let alias = import.alias(parsed);
            if statement.module.as_deref() == Some("__future__")
                && statement.level == 0
                && !statement.is_lazy
            {
                return None;
            }
            if statement.is_export
                || alias
                    .asname
                    .as_ref()
                    .is_some_and(|asname| asname.id == alias.name.id)
            {
                return None;
            }
            let mut qualified_name = ".".repeat(statement.level as usize);
            if let Some(module) = &statement.module {
                qualified_name.push_str(module);
                qualified_name.push('.');
            }
            qualified_name.push_str(&alias.name);
            Some(UnusedImport {
                statement: statement.range(),
                qualified_name,
                module: statement
                    .module
                    .as_deref()
                    .and_then(ModuleName::new)
                    .filter(|_| statement.level == 0),
                binding: alias.identifier(),
            })
        }
        _ => None,
    }
}

/// what the diagnostics of one file share: whether it is a package's `__init__`, and which
/// import statements probe for their module
struct ImportContext<'db> {
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    in_init: bool,
    /// the start of every import statement written in the body of a `try` that handles
    /// `ImportError` or `ModuleNotFoundError`
    probing: FxHashSet<TextSize>,
}

impl<'db> ImportContext<'db> {
    fn new(
        db: &'db dyn Db,
        file: ProgramFile<'db>,
        env: &ProgramEnvironment<'db>,
        parsed: &ParsedModuleRef,
    ) -> Self {
        let source_file = file.file(db);
        // a package's `__init__.by` is the module importing the package runs; its stub
        // re-exports only what it spells as one, as any stub does
        let in_init = !source_file.is_stub(db)
            && source_file
                .path(db)
                .as_system_path()
                .and_then(|path| path.file_stem())
                .is_some_and(|stem| stem == "__init__");
        let import_errors: Vec<Type<'db>> = ["ImportError", "ModuleNotFoundError"]
            .into_iter()
            .filter_map(|name| {
                builtins_symbol(db, env, name)
                    .place
                    .ignore_possibly_undefined()
            })
            .collect();
        let model = crate::SemanticModel::new(db, file);
        let mut probes = ImportProbes {
            model: &model,
            import_errors: &import_errors,
            depth: 0,
            probing: FxHashSet::default(),
        };
        probes.visit_body(parsed.suite());
        Self {
            db,
            file,
            in_init,
            probing: probes.probing,
        }
    }

    /// whether `module` is the project's own code
    fn is_first_party(&self, import: &UnusedImport) -> bool {
        let db = self.db;
        let Some(module) = &import.module else {
            // a relative import names the package it is written in
            return import.qualified_name.starts_with('.');
        };
        let file = self.file.file(db);
        resolve_module(
            db,
            ImportingFile::File(file, self.file.resolver_environment(db)),
            module,
        )
        .and_then(|module| module.search_path(db))
        .is_some_and(ty_module_resolver::SearchPath::is_first_party)
    }
}

impl UnusedImport {
    fn report(&self, context: &InferContext<'_, '_>, imports: &ImportContext<'_>, at_module: bool) {
        let Some(builder) = context.report_lint(&UNUSED_IMPORT, self.binding) else {
            return;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "`{}` imported but unused",
            self.qualified_name
        ));
        if let Some(annotation) = diagnostic.primary_annotation_mut() {
            annotation.push_tag(ruff_db::diagnostic::DiagnosticTag::Unnecessary);
        }
        if imports.probing.contains(&self.statement.start()) {
            diagnostic.help(
                "To test whether a module is available, use `importlib.util.find_spec` \
                 instead of importing it",
            );
        } else if imports.in_init && at_module && imports.is_first_party(self) {
            diagnostic.help(
                "If this is a re-export, write it as `from … export …`, as a redundant alias, \
                 or list it in `__all__`",
            );
        }
    }
}

/// finds the import statements written in the body of a `try` that handles `ImportError` or
/// `ModuleNotFoundError` — an import made to find out whether the module is there
struct ImportProbes<'a, 'db> {
    model: &'a crate::SemanticModel<'db>,
    import_errors: &'a [Type<'db>],
    /// how many enclosing `try` bodies handle an import error
    depth: usize,
    probing: FxHashSet<TextSize>,
}

impl ImportProbes<'_, '_> {
    fn handles_import_error(&self, handlers: &[ast::ExceptHandler]) -> bool {
        handlers.iter().any(|handler| {
            let ast::ExceptHandler::ExceptHandler(handler) = handler;
            let Some(caught) = handler.type_.as_deref() else {
                return false;
            };
            let caught: &[ast::Expr] = match caught {
                ast::Expr::Tuple(tuple) => &tuple.elts,
                single => std::slice::from_ref(single),
            };
            caught.iter().any(|element| {
                crate::HasType::inferred_type(element, self.model)
                    .is_some_and(|ty| self.import_errors.contains(&ty))
            })
        })
    }
}

impl<'ast> StatementVisitor<'ast> for ImportProbes<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        match stmt {
            ast::Stmt::Import(_) | ast::Stmt::ImportFrom(_) => {
                if self.depth > 0 {
                    self.probing.insert(stmt.start());
                }
            }
            ast::Stmt::Try(try_stmt) => {
                let probes = self.handles_import_error(&try_stmt.handlers);
                self.depth += usize::from(probes);
                self.visit_body(&try_stmt.body);
                self.depth -= usize::from(probes);
                for handler in &try_stmt.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    self.visit_body(&handler.body);
                }
                self.visit_body(&try_stmt.orelse);
                self.visit_body(&try_stmt.finalbody);
            }
            _ => walk_stmt(self, stmt),
        }
    }
}

/// the string literals listed in the module's `__all__`, wherever its definition is too
/// dynamic for the checker's own reading of it to give a set of names
#[derive(Default)]
struct DunderAllLiterals {
    names: Vec<Name>,
}

impl DunderAllLiterals {
    fn extend_from(&mut self, value: &ast::Expr) {
        match value {
            ast::Expr::List(ast::ExprList { elts, .. })
            | ast::Expr::Tuple(ast::ExprTuple { elts, .. }) => {
                for element in elts {
                    if let ast::Expr::StringLiteral(literal) = element {
                        self.names.push(Name::new(literal.value.to_str()));
                    }
                }
            }
            ast::Expr::BinOp(ast::ExprBinOp { left, right, .. }) => {
                self.extend_from(left);
                self.extend_from(right);
            }
            _ => {}
        }
    }
}

fn is_dunder_all(target: &ast::Expr) -> bool {
    matches!(target, ast::Expr::Name(name) if name.id == "__all__")
}

impl<'ast> StatementVisitor<'ast> for DunderAllLiterals {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        match stmt {
            ast::Stmt::Assign(assign) if assign.targets.iter().any(is_dunder_all) => {
                self.extend_from(&assign.value);
            }
            ast::Stmt::AnnAssign(assign) if is_dunder_all(&assign.target) => {
                if let Some(value) = &assign.value {
                    self.extend_from(value);
                }
            }
            ast::Stmt::AugAssign(assign) if is_dunder_all(&assign.target) => {
                self.extend_from(&assign.value);
            }
            ast::Stmt::Expr(ast::StmtExpr { value, .. }) => {
                if let ast::Expr::Call(call) = value.as_ref()
                    && let ast::Expr::Attribute(attribute) = call.func.as_ref()
                    && is_dunder_all(&attribute.value)
                    && let [argument] = &*call.arguments.args
                {
                    match attribute.attr.as_str() {
                        "extend" => self.extend_from(argument),
                        "append" => {
                            if let ast::Expr::StringLiteral(literal) = argument {
                                self.names.push(Name::new(literal.value.to_str()));
                            }
                        }
                        _ => {}
                    }
                }
            }
            // a nested scope's `__all__` is not the module's
            ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_) => {}
            _ => walk_stmt(self, stmt),
        }
    }
}
