/*!
An abstraction for adding new imports to a single Python source file.

This importer is based on a similar abstraction in `ruff_linter::importer`.
Both of them use the lower-level `ruff_python_importer::Insertion` primitive.
The main differences here are:

1. This works with ty's semantic model instead of ruff's.
2. This owns the task of visiting AST to extract imports for
   diagnostic fixes, completions, and inlay hints.
3. It doesn't have as many facilities as `ruff_linter`'s importer.
*/

use std::cell::OnceCell;
use std::ops::Range;

use rustc_hash::FxHashMap;

use ruff_db::PythonFile;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};

use crate::types::Type;
use crate::{Db, SemanticModel};
use ruff_db::source::{SourceText, source_text};
use ruff_diagnostics::Edit;
use ruff_python_ast as ast;
use ruff_python_ast::helpers::format_import_from;
use ruff_python_ast::name::Name;
use ruff_python_ast::token::Tokens;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, TraversalSignal, walk_stmt};
use ruff_python_codegen::Stylist;
use ruff_python_importer::{ImportOrdering, Insertion, SortedImport};
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_module_resolver::{ImportingFile, ModuleName};
use ty_python_core::ast_node_ref::AstNodeRef;
use ty_python_core::definition::{DefinitionKind, DefinitionState};
use ty_python_core::scope::FileScopeId;
use ty_python_core::{ProgramFile, semantic_index};

pub struct Importer<'a> {
    /// The ty Salsa database.
    db: &'a dyn Db,
    /// The file corresponding to the module that
    /// we want to insert an import statement into.
    file: ProgramFile<'a>,
    /// The parsed module ref.
    parsed: &'a ParsedModuleRef,
    /// The tokens representing the Python AST.
    tokens: &'a Tokens,
    /// The source code for `file`.
    source: SourceText,
    /// The cached source style for formatting inserted code.
    stylist: &'a Stylist<'static>,
    /// The list of visited, top-level runtime imports in the Python AST.
    imports: &'a [AstNodeRef<ast::Stmt>],
    /// the order an import sorter puts this file's imports in, for imports to be added where it
    /// would put them. `None` adds a statement at the top of the file and a member after an
    /// import's last
    sorting: Option<Box<dyn Sorting + 'a>>,
}

impl<'a> Importer<'a> {
    /// Create a new importer.
    ///
    /// The `file` given should correspond to the module that we want
    /// to insert an import statement into.
    ///
    /// Source style and top-level imports are cached per file, so callers
    /// can construct an importer without rescanning the source or AST.
    pub fn new(db: &'a dyn Db, file: ProgramFile<'a>, parsed: &'a ParsedModuleRef) -> Self {
        let imports = top_level_imports(db, file.python_file(db));

        Self {
            db,
            file,
            parsed,
            tokens: parsed.tokens(),
            source: source_text(db, file.file(db)),
            stylist: stylist(db, file.python_file(db)),
            imports,
            sorting: None,
        }
    }

    /// this importer, adding each import where `order` sorts it: a statement in its section of
    /// the file's import block and a member among the members of a `from` import
    ///
    /// `resolve` is called the first time an import is placed, as finding the order can be
    /// costly; when it returns `None` the imports are added as without an order
    #[must_use]
    pub fn sorted_by<O, F>(mut self, resolve: F) -> Self
    where
        O: ImportOrdering + 'a,
        F: Fn() -> Option<O> + 'a,
    {
        self.sorting = Some(Box::new(LazyOrder {
            resolve,
            order: OnceCell::new(),
            blocks: OnceCell::new(),
        }));
        self
    }

    /// The file's indentation unit, shared with inserted imports and diagnostic fixes.
    pub(crate) fn indentation(&self) -> &str {
        self.stylist.indentation().as_str()
    }

    /// Builds a set of members in scope at the given AST node and position.
    ///
    /// Callers should use this routine to build "in scope members" to be used
    /// with repeated calls to `Importer::import`. This does some work up-front
    /// to avoid doing it for every call to `Importer::import`.
    ///
    /// In general, `at` should be equivalent to `node.start()` (from the
    /// [`ruff_text_size::Ranged`] trait). However, in some cases, identifying
    /// a good AST node for where the cursor is can be difficult, where as
    /// knowing the precise position of the cursor is easy. The AST node in
    /// that circumstance may be a very poor approximation that may still
    /// result in good auto-import results.
    ///
    /// This API is designed with completions in mind. That is, we might have
    /// many possible candidates to add as an import while the position we want
    /// to insert them remains invariant.
    pub fn members_in_scope_at(
        &self,
        node: ast::AnyNodeRef<'_>,
        at: TextSize,
    ) -> MembersInScope<'a> {
        MembersInScope::new(self.db, self.file, self.parsed, node, at)
    }

    /// Builds a best-effort import action for this module, usually for an IDE completion.
    ///
    /// This method always returns an action, even when it cannot avoid every name conflict.
    /// For diagnostic fixes inside this crate, `Self::import_for_diagnostic` additionally checks
    /// bindings without querying inferred types and may decline the import.
    ///
    /// The given request is assumed to be valid. That is, the module
    /// is assumed to be importable and the member is assumed to be a
    /// valid thing to import from the given module.
    ///
    /// When possible (particularly when there is no existing import
    /// statement to satisfy the given request), the import style on
    /// the request is respected. When there is an existing import,
    /// then the existing style is always respected instead.
    ///
    /// `members` should be a map of symbols in scope at the position
    /// where the imported symbol should be available. This is used
    /// to craft import statements in a way that doesn't conflict with
    /// symbols in scope. If it's not feasible to provide this map, then
    /// providing an empty map is generally fine. But it does mean that
    /// the resulting import may shadow (or be shadowed by) some other
    /// symbol.
    ///
    /// The "import action" returned includes an edit for inserting
    /// the actual import (if necessary) along with the symbol text
    /// that should be used to refer to the imported symbol. While
    /// the symbol text may be expected to just be equivalent to the
    /// request's `member`, it can be different. For example, there
    /// might be an alias, or the corresponding module might already be
    /// imported in a qualified way.
    pub fn import(&self, request: ImportRequest<'_>, members: &MembersInScope) -> ImportAction {
        let importing_file = ImportingFile::File(
            self.file.file(self.db),
            self.file.resolver_environment(self.db),
        );
        let request = request.avoid_conflicts(self.db, importing_file, members);
        let mut symbol_text: Box<str> = request.symbol_text().into();
        let Some(response) = self.find(importing_file, &request, members.at) else {
            let placement = Placement::statement(&request, members.at);
            let import = self
                .placed_edits(std::slice::from_ref(&placement))
                .pop()
                .unwrap_or_else(|| {
                    self.statement_insertion(members.at)
                        .into_edit(&request.to_string())
                });
            if let Some(member) = request.member
                && matches!(request.style, ImportStyle::Import)
            {
                symbol_text = format!("{}.{}", request.module, member).into();
            }
            return ImportAction {
                import: Some(import),
                symbol_text,
                placement: Some(placement),
            };
        };

        // When we just have a request to import a module (and not
        // any members from that module), then the only way we can be
        // here is if we found a pre-existing import that definitively
        // satisfies the request. So we're done.
        let Some(member) = request.member else {
            return ImportAction {
                import: None,
                symbol_text,
                placement: None,
            };
        };
        match response.kind {
            ImportResponseKind::Unqualified { alias } => {
                let member = alias.asname.as_ref().unwrap_or(&alias.name).as_str();
                // As long as it's not a wildcard import, we use whatever name
                // the member is imported as when inserting the symbol.
                if member != "*" {
                    symbol_text = member.into();
                }
                ImportAction {
                    import: None,
                    symbol_text,
                    placement: None,
                }
            }
            ImportResponseKind::Qualified { alias } => {
                let module = alias.asname.as_ref().unwrap_or(&alias.name).as_str();
                ImportAction {
                    import: None,
                    symbol_text: format!("{module}.{member}").into(),
                    placement: None,
                }
            }
            ImportResponseKind::Partial(from) => {
                let module = from.module.as_ref().map(ast::Identifier::as_str);
                let alone = self
                    .sorting
                    .as_ref()
                    .is_some_and(|sorting| sorting.imports_member_alone(module, request.asname));
                // a sorter that won't list the member among others writes it in a `from` import
                // of its own, so that is the statement added
                let placement = if alone {
                    Placement::Statement {
                        module: module.map(str::to_string),
                        level: from.level,
                        member: Some(member.to_string()),
                        asname: request.asname.map(str::to_string),
                        at: members.at,
                    }
                } else {
                    Placement::Member {
                        import: from.start(),
                        member: member.to_string(),
                        asname: request.asname.map(str::to_string),
                    }
                };
                let import = self
                    .placed_edits(std::slice::from_ref(&placement))
                    .pop()
                    .unwrap_or_else(|| {
                        let imported = request.imported_member();
                        if let Some(insertion) =
                            Insertion::existing_import(response.import.stmt, self.tokens)
                        {
                            insertion.into_edit(&imported)
                        } else {
                            Insertion::end_of_statement(
                                response.import.stmt,
                                &self.source,
                                self.stylist,
                            )
                            .into_edit(&format!("from {} import {imported}", request.module))
                        }
                    });
                ImportAction {
                    import: Some(import),
                    symbol_text,
                    placement: Some(placement),
                }
            }
        }
    }

    /// Builds an import action for a diagnostic fix, returning `None` if its name cannot be used
    /// confidently at the proposed use site.
    ///
    /// [`Self::import`] uses a caller-supplied [`MembersInScope`] to choose an import style and
    /// always returns an action. This is useful for completions: a candidate can still be offered
    /// when resolving every possible conflict would be too restrictive. However, constructing
    /// that map with [`Self::members_in_scope_at`] queries inferred types. Doing so while emitting
    /// an inference diagnostic can re-enter the inference that is producing the diagnostic.
    ///
    /// This method instead checks bindings in the semantic index, without inferring their types.
    /// It accepts a new name only if visible scopes have no binding or declaration for it, and
    /// reuses an existing name only if it is a top-level runtime import that has not been
    /// reassigned or deleted. These checks are deliberately conservative: a later reassignment
    /// can prevent reuse even when the import would still be available at `at`.
    ///
    /// Both APIs assume that the requested module is importable and that it provides the requested
    /// member. The caller must check Python-version and dependency requirements; neither API
    /// establishes them. The examples below assume Python 3.11 or newer.
    ///
    /// # When the simpler API is sufficient
    ///
    /// An `undefined-reveal` diagnostic already establishes that `reveal_type` is unbound here:
    ///
    /// ```python
    /// def show(value: int) -> None:
    ///     reveal_type(value)
    /// ```
    ///
    /// That fix can call [`Self::import`] with
    /// `ImportRequest::import_from("typing", "reveal_type").force()` and an empty
    /// [`MembersInScope`]. Forcing a `from` import avoids introducing a module name that could be
    /// shadowed. Applying the returned import edit produces:
    ///
    /// ```python
    /// from typing import reveal_type
    ///
    /// def show(value: int) -> None:
    ///     reveal_type(value)
    /// ```
    ///
    /// A fix that introduces a new call cannot generally assume that its chosen function name is
    /// unbound. For example, `assert_never` could already name a function parameter. An empty
    /// [`MembersInScope`] would conceal that conflict from [`Self::import`].
    ///
    /// # When an existing import is shadowed
    ///
    /// For an unforced request for `typing.assert_never`, [`Self::import`] can reuse an existing
    /// `import typing as t` and return `t.assert_never`, even with a populated [`MembersInScope`].
    /// Its conflict avoidance chooses between the requested module and member names; it does not
    /// validate that an alias found in an existing import still refers to that import at the use
    /// site. A caller adding an exhaustiveness check could therefore produce this incorrect call:
    ///
    /// ```python
    /// import typing as t
    ///
    /// def handle(value: int | str, t: int) -> None:
    ///     if isinstance(value, int):
    ///         print(value)
    ///     elif isinstance(value, str):
    ///         print(value)
    ///     else:
    ///         t.assert_never(value)  # `t` is the integer parameter, not the module.
    /// ```
    ///
    /// This method rejects that alias and tries a `from` import instead. It returns an import edit
    /// and `assert_never` as the symbol text, allowing the caller to construct this fix:
    ///
    /// ```python
    /// from typing import assert_never
    /// import typing as t
    ///
    /// def handle(value: int | str, t: int) -> None:
    ///     if isinstance(value, int):
    ///         print(value)
    ///     elif isinstance(value, str):
    ///         print(value)
    ///     else:
    ///         assert_never(value)
    /// ```
    ///
    /// The caller must use [`ImportAction::symbol_text`] for the new reference and include any
    /// [`ImportAction::import`] edit. An unshadowed alias can be reused without an import edit;
    /// an occupied function name can instead require a qualified reference and a module import.
    ///
    /// # When neither import style is usable
    ///
    /// Both possible names can already have unrelated bindings:
    ///
    /// ```python
    /// def handle(value: int | str, typing: int, assert_never: int) -> None:
    ///     if isinstance(value, int):
    ///         print(value)
    ///     elif isinstance(value, str):
    ///         print(value)
    /// ```
    ///
    /// With these members in scope, [`Self::import`] still returns a best-effort action: add
    /// `import typing` at module level and use `typing.assert_never`. That module-level import
    /// would remain shadowed by the function parameter. This method returns `None`, so the caller
    /// can omit the import-dependent fix or offer a different fix. It does not invent a fresh alias.
    ///
    /// # Use site and shared implementation
    ///
    /// `scope` is the scope where the new reference will be evaluated. `at` is an offset in the
    /// original source at or before the planned reference: existing imports must precede it, and
    /// notebook import edits must respect its cell boundaries. The requested import style is
    /// tried first, followed by forced `from` and module imports. Even a forced request can be
    /// retried with the other style.
    ///
    /// All import lookup, formatting, and edit construction is shared with [`Self::import`]. This
    /// method calls it for each candidate style and then validates the returned name. The extra
    /// work here is deciding whether a diagnostic can use that action, not constructing a second
    /// kind of import edit.
    pub(crate) fn import_for_diagnostic(
        &self,
        request: ImportRequest<'_>,
        scope: FileScopeId,
        at: TextSize,
    ) -> Option<ImportAction> {
        let index = semantic_index(self.db, self.file);
        let importing_file = ImportingFile::File(
            self.file.file(self.db),
            self.file.resolver_environment(self.db),
        );
        for request in [
            request,
            ImportRequest {
                style: ImportStyle::ImportFrom,
                force_style: true,
                ..request
            },
            ImportRequest {
                style: ImportStyle::Import,
                force_style: true,
                ..request
            },
        ] {
            let action = self.import(request, &MembersInScope::empty(at));
            let root = action.symbol_text().split('.').next()?;
            let mut existing_import = false;
            let available = index.visible_ancestor_scopes(scope).all(|(scope, _)| {
                let places = index.place_table(scope);
                let Some(symbol_id) = places.symbol_id(root) else {
                    return true;
                };
                let symbol = places.symbol(symbol_id);
                if !symbol.is_bound() && !symbol.is_declared() {
                    return true;
                }
                if symbol.is_reassigned() {
                    return false;
                }
                // Ignore the implicit initial unbound state, but retain deletions so a deleted
                // import cannot be reused.
                let mut bindings = index
                    .use_def_map(scope)
                    .end_of_scope_symbol_bindings(symbol_id)
                    .map(|binding| binding.binding)
                    .filter(|binding| !matches!(binding, DefinitionState::Undefined));
                let Some(binding) = bindings.next().and_then(DefinitionState::definition) else {
                    return false;
                };
                if bindings.next().is_some() {
                    return false;
                }
                let import = match binding.kind(self.db) {
                    DefinitionKind::Import(kind) => AstImportKind::Import(kind.import(self.parsed)),
                    DefinitionKind::ImportFrom(kind) => {
                        AstImportKind::ImportFrom(kind.import(self.parsed))
                    }
                    _ => return false,
                };
                existing_import = import.start() < at
                    && self
                        .imports()
                        .any(|top_level| top_level.stmt.start() == import.start())
                    && match import.satisfies(self.db, importing_file, &request) {
                        Some(ImportResponseKind::Qualified { .. }) => true,
                        Some(ImportResponseKind::Unqualified { alias }) => {
                            Some(alias.name.as_str()) == request.member
                        }
                        _ => false,
                    };
                existing_import
            });
            if available && (existing_import || action.import().is_some()) {
                return Some(action);
            }
        }
        None
    }

    /// the edits that add everything `actions` import at once
    ///
    /// each action's own edit is computed as if it were the only import added, so two of them
    /// can land at one place in an order the file's import sorter wouldn't write, or both
    /// separate themselves from the imports around them. these are placed together instead:
    /// statements of one section are sorted among each other and separated as the sorter
    /// separates them, two members added to one `from` import are sorted, and two members added
    /// from a module the file doesn't import yet are listed by one new `from` import, with or
    /// without a sorter's order to follow
    pub fn combined_edits<'b>(
        &self,
        actions: impl IntoIterator<Item = &'b ImportAction>,
    ) -> Vec<Edit> {
        let mut placements = Vec::new();
        let mut edits = Vec::new();
        for action in actions {
            match (&action.placement, &action.import) {
                (Some(placement), Some(_)) => placements.push(placement.clone()),
                (None, Some(edit)) => edits.push(edit.clone()),
                (_, None) => {}
            }
        }
        edits.extend(self.placed_edits(&placements));
        edits
    }

    /// where a new statement goes without a sorter's order: after the last `from __future__`
    /// import before `at`, or otherwise at the top of the file (or of `at`'s notebook cell)
    fn statement_insertion(&self, at: TextSize) -> Insertion<'static> {
        if let Some(future) = self.find_last_future_import(at) {
            Insertion::end_of_statement(future.stmt, &self.source, self.stylist)
        } else {
            let range = self.cell_at(at).map(|(_, range)| range);
            Insertion::start_of_file(self.parsed.suite(), &self.source, self.stylist, range)
        }
    }

    /// the notebook cell an import for a use at `at` is added to, the one `at` is in, as its
    /// index among the notebook's cells and its range. `None` for a file that is not a notebook,
    /// whose imports go in the file as a whole
    fn cell_at(&self, at: TextSize) -> Option<(usize, TextRange)> {
        let offsets = self.source.as_notebook()?.cell_offsets();
        // the end of the last cell is the end of the notebook, which a use at the end of it is
        // still in
        offsets
            .array_windows::<2>()
            .enumerate()
            .find(|(_, [start, end])| {
                *start <= at && (at < *end || *end == TextSize::of(self.source.as_str()))
            })
            .map(|(index, [start, end])| (index, TextRange::new(*start, *end)))
    }

    /// the top-level statements of the cell an import for a use at `at` is added to, or of the
    /// whole file when it is not a notebook, as a range of the module's body. `None` when `at`
    /// is in no cell of a notebook
    fn part_at(&self, at: TextSize) -> Option<Range<usize>> {
        let body = self.parsed.suite();
        if self.source.as_notebook().is_none() {
            return Some(0..body.len());
        }
        let (_, cell) = self.cell_at(at)?;
        let start = body.partition_point(|stmt| stmt.start() < cell.start());
        let end = body.partition_point(|stmt| stmt.start() < cell.end());
        Some(start..end)
    }

    /// the edits that make the additions in `placements`: where the file's import sorter would
    /// make them, or without an order to follow, statements at the top of the file and members
    /// after an import's last
    fn placed_edits(&self, placements: &[Placement]) -> Vec<Edit> {
        if let Some(sorting) = &self.sorting
            && let Some(edits) = sorting.placed_edits(self, placements)
        {
            return edits;
        }
        let (statements, members) = combined(placements, &Unordered);
        let mut edits = Vec::new();
        if let Some(at) = statements.iter().map(|statement| statement.at).min() {
            let insertion = self.statement_insertion(at);
            let separator = if insertion.is_inline() {
                "; "
            } else {
                self.stylist.line_ending().as_str()
            };
            let text = statements
                .iter()
                .map(NewStatement::text)
                .collect::<Vec<_>>()
                .join(separator);
            edits.push(insertion.into_edit(&text));
        }
        for (import, added) in members {
            let Some(stmt) = self
                .imports()
                .find(|candidate| candidate.stmt.start() == import)
            else {
                continue;
            };
            let imported = added
                .iter()
                .map(|member| imported_member(*member))
                .collect::<Vec<_>>()
                .join(", ");
            edits.push(
                if let Some(insertion) = Insertion::existing_import(stmt.stmt, self.tokens) {
                    insertion.into_edit(&imported)
                } else {
                    let from = match stmt.kind {
                        AstImportKind::ImportFrom(from) => {
                            format_import_from(from.level, from.module.as_deref())
                        }
                        AstImportKind::Import(_) => continue,
                    };
                    Insertion::end_of_statement(stmt.stmt, &self.source, self.stylist)
                        .into_edit(&format!("from {from} import {imported}"))
                },
            );
        }
        edits
    }

    /// the imports the sorter sorts as one block that new imports join, the run of consecutive
    /// import statements the module's body starts its imports with, or in a notebook the body
    /// of `at`'s cell, which the sorter sorts on its own, and where `order` sorts each of them,
    /// worked out once into `blocks`. `None` when it starts at or after `at`, where the imports
    /// have to be available
    fn import_block<'b, O: ImportOrdering>(
        &self,
        order: &O,
        blocks: &'b ImportBlocks<O>,
        at: TextSize,
    ) -> Option<(&'a [ast::Stmt], &'b [O::Position])> {
        let body: &'a [ast::Stmt] = self.parsed.suite();
        let cell = self.cell_at(at).map_or(0, |(index, _)| index);
        let part = self.part_at(at)?;
        let (range, positions) = blocks
            .get_or_init(|| {
                let parts = self.source.as_notebook().map_or(1, |notebook| {
                    notebook.cell_offsets().len().saturating_sub(1)
                });
                (0..parts).map(|_| OnceCell::new()).collect()
            })
            .get(cell)?
            .get_or_init(|| {
                let start = part.start + body[part.clone()].iter().position(is_import)?;
                let len = body[start..part.end]
                    .iter()
                    .take_while(|stmt| is_import(stmt))
                    .count();
                let positions = body[start..start + len]
                    .iter()
                    .map(|stmt| sorted_import(stmt, order).map(|import| order.position(&import)))
                    .collect::<Option<_>>()?;
                Some((start..start + len, positions))
            })
            .as_ref()?;
        let statements = &body[range.clone()];
        (statements.first()?.start() < at).then_some((statements, positions.as_slice()))
    }

    /// the edits that make the additions in `placements` where `order` puts them
    fn placed_edits_in<O: ImportOrdering>(
        &self,
        order: &O,
        blocks: &ImportBlocks<O>,
        placements: &[Placement],
    ) -> Option<Vec<Edit>> {
        let mut edits = Vec::new();
        let (statements, members) = combined(placements, order);
        if let Some(at) = statements.iter().map(|statement| statement.at).min() {
            edits.extend(self.placed_statements(order, blocks, &statements, at));
        }
        for (import, added) in members {
            let AstImportKind::ImportFrom(from) = self
                .imports()
                .find(|candidate| candidate.stmt.start() == import)?
                .kind
            else {
                return None;
            };
            edits.extend(self.placed_members(order, from, &added)?);
        }
        Some(edits)
    }

    /// `statement` as `order`'s sorter writes it, wrapped to its line length
    fn written<O: ImportOrdering>(&self, order: &O, statement: &NewStatement) -> String {
        let line_ending = self.stylist.line_ending().as_str();
        let text = statement.text();
        order
            .write_statements(&format!("{text}{line_ending}"), self.stylist)
            .map(|written| {
                written
                    .strip_suffix(line_ending)
                    .unwrap_or(&written)
                    .to_string()
            })
            .unwrap_or(text)
    }

    /// the edits adding `statements` to the file's import block, each in its section, sorted
    /// among the section's imports and each other, with the blank lines and section headings
    /// the sorter writes around them. without an import block before `at`, the statements are
    /// one of their own, where a statement goes without an order
    fn placed_statements<O: ImportOrdering>(
        &self,
        order: &O,
        blocks: &ImportBlocks<O>,
        statements: &[NewStatement],
        at: TextSize,
    ) -> Vec<Edit> {
        let source: &str = &self.source;
        let line_ending = self.stylist.line_ending().as_str();
        // in a notebook, the imports are sorted, and added, within `at`'s cell
        let cell = self.cell_at(at).map(|(_, cell)| cell);

        let Some((block, positions)) = self.import_block(order, blocks, at) else {
            let insertion = self.statement_insertion(at);
            let insertion_is_inline = insertion.is_inline();
            let text = if insertion_is_inline {
                statements
                    .iter()
                    .map(NewStatement::text)
                    .collect::<Vec<_>>()
                    .join("; ")
            } else {
                let mut added: Vec<_> = statements
                    .iter()
                    .map(|statement| (order.position(&statement.sorted_import()), statement))
                    .collect();
                added.sort_by(|a, b| a.0.cmp(&b.0));
                let text = self.separated(order, None, &added, None);
                text.strip_suffix(line_ending).unwrap_or(&text).to_string()
            };
            let mut edit = insertion.into_edit(&text);
            // a new block is set apart from the statement after it as the sorter sets it apart,
            // unless that is an import the block runs on into
            if !insertion_is_inline
                && let Some(next) = self
                    .parsed
                    .suite()
                    .iter()
                    .find(|stmt| stmt.start() >= edit.start())
                && cell.is_none_or(|cell| cell.contains(next.start()))
                && !is_import(next)
            {
                let before_definition =
                    matches!(next, ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_));
                let existing = source[TextRange::new(edit.start(), next.start())]
                    .split_inclusive('\n')
                    .take_while(|line| line.trim().is_empty())
                    .count();
                let missing = order
                    .blank_lines_after_block(before_definition)
                    .saturating_sub(existing);
                if missing > 0 {
                    let mut text = edit.content().unwrap_or_default().to_string();
                    text.push_str(&line_ending.repeat(missing));
                    edit = Edit::insertion(text, edit.start());
                }
            }
            return vec![edit];
        };

        // the sorter's order is total, so in a sorted block a new statement goes before the
        // first import that sorts after it. in a block that isn't sorted, that is still a place
        // among the imports it sorts with. statements that go before the same import are sorted
        // among each other
        let mut slots: Vec<StatementSlot<'_, O::Position>> = Vec::new();
        for statement in statements {
            let position = order.position(&statement.sorted_import());
            let after = positions.iter().position(|existing| *existing > position);
            let slot = match slots.iter_mut().find(|(slot, _)| *slot == after) {
                Some(slot) => slot,
                None => {
                    slots.push((after, Vec::new()));
                    let Some(slot) = slots.last_mut() else {
                        continue;
                    };
                    slot
                }
            };
            slot.1.push((position, statement));
        }

        let mut edits = Vec::new();
        for (after, mut added) in slots {
            added.sort_by(|a, b| a.0.cmp(&b.0));
            let before = match after {
                Some(0) => None,
                Some(after) => Some(after - 1),
                None => positions.len().checked_sub(1),
            };

            // between two imports of one line, as in `import os; import sys`, the statements
            // can only be written on that line
            if let (Some(before), Some(after)) = (before, after)
                && !source
                    .contains_line_break(TextRange::new(block[before].end(), block[after].start()))
            {
                let mut text = String::new();
                for (_, statement) in &added {
                    text.push_str(&statement.text());
                    text.push_str("; ");
                }
                edits.push(Edit::insertion(text, block[after].start()));
                continue;
            }

            // the sorter takes the headings directly above the block's first import into the
            // block and writes them again, and moves the other comments directly above an import
            // with it. so the lines between the imports the statements go between, which are
            // regenerated with them, run from the line after the import before them to the
            // headings and blank lines above the import after them
            let start = before.map_or_else(
                || {
                    let mut start = source.line_start(block[0].start());
                    let top = cell.map_or(TextSize::default(), TextRange::start);
                    while start > top {
                        let previous = source.line_start(start - TextSize::from(1));
                        if !order.is_heading(source[TextRange::new(previous, start)].trim()) {
                            break;
                        }
                        start = previous;
                    }
                    start
                },
                |before| source.full_line_end(block[before].end()),
            );
            let end = after.map_or(start, |after| {
                let mut end = source.line_start(block[after].start());
                if after > 0 {
                    while end > start {
                        let previous = source.line_start(end - TextSize::from(1));
                        let line = source[TextRange::new(previous, end)].trim();
                        if !line.starts_with('#') || order.is_heading(line) {
                            break;
                        }
                        end = previous;
                    }
                }
                end
            });
            let between = &source[TextRange::new(start, end)];

            let previous = before.map(|before| &positions[before]);
            let next = after.map(|after| &positions[after]);
            let regenerated = between
                .lines()
                .all(|line| line.trim().is_empty() || order.is_heading(line.trim()));
            let edit = if regenerated {
                let text = self.separated(order, previous, &added, next);
                minimal_edit(between, &text, start)
            } else {
                // a comment between the imports is left where it is, and the statements go
                // right after the import before them
                let mut text = String::new();
                if !source[..start.to_usize()].ends_with(['\n', '\r']) {
                    text.push_str(line_ending);
                }
                text.push_str(&self.separated(order, previous, &added, None));
                Some(Edit::insertion(text, start))
            };
            edits.extend(edit);
        }
        edits
    }

    /// `added`, sorted, written as the sorter writes them after the import at `previous` and
    /// before the import at `next`: each preceded by the blank lines the sorter separates it
    /// from the statement before with, and by its section's heading when it starts the section,
    /// and followed by the blank lines and heading the sorter writes above `next`
    fn separated<'p, O: ImportOrdering>(
        &self,
        order: &O,
        mut previous: Option<&'p O::Position>,
        added: &'p [(O::Position, &NewStatement)],
        next: Option<&O::Position>,
    ) -> String {
        let line_ending = self.stylist.line_ending().as_str();
        let mut text = String::new();
        let write_before = |text: &mut String,
                            previous: Option<&O::Position>,
                            position: &O::Position| {
            if let Some(previous) = previous {
                for _ in 0..order.blank_lines_between(previous, position) {
                    text.push_str(line_ending);
                }
            }
            if previous.is_none_or(|previous| order.section(previous) != order.section(position))
                && let Some(heading) = order.section_heading(position)
            {
                text.push_str(heading);
                text.push_str(line_ending);
            }
        };
        for (position, statement) in added {
            write_before(&mut text, previous, position);
            text.push_str(&self.written(order, statement));
            text.push_str(line_ending);
            previous = Some(position);
        }
        if let Some(next) = next {
            write_before(&mut text, previous, next);
        }
        text
    }

    /// the edits listing `added`, sorted, among the members of the existing `from` import
    /// `from`, each where `order` sorts it. when the import was written as the sorter writes it
    /// and the sorter would write it differently with the members added, as it wraps an import
    /// that no longer fits on its line, the import is written again as the sorter writes it
    fn placed_members<O: ImportOrdering>(
        &self,
        order: &O,
        from: &ast::StmtImportFrom,
        added: &[Member],
    ) -> Option<Vec<Edit>> {
        let source: &str = &self.source;
        let names = &from.names;
        let line_ending = self.stylist.line_ending().as_str();
        let mut edits = Vec::new();
        let mut appended = Vec::new();
        let mut slots: Vec<(usize, Vec<String>)> = Vec::new();
        for member in added {
            let imported = imported_member(*member);
            let next = names.iter().position(|alias| {
                order
                    .compare_members(
                        *member,
                        (
                            alias.name.as_str(),
                            alias.asname.as_ref().map(ast::Identifier::as_str),
                        ),
                    )
                    .is_lt()
            });
            match next {
                Some(next) => match slots.iter_mut().find(|(slot, _)| *slot == next) {
                    Some(slot) => slot.1.push(imported),
                    None => slots.push((next, vec![imported])),
                },
                None => appended.push(imported),
            }
        }
        for (next, members) in slots {
            // written with the separator the import already puts between its members, so a
            // member lands on a line of its own in an import that writes one per line
            let separator = if next + 1 < names.len() {
                TextRange::new(names[next].end(), names[next + 1].start())
            } else if next > 0 {
                TextRange::new(names[next - 1].end(), names[next].start())
            } else {
                TextRange::empty(names[next].start())
            };
            let separator = &source[separator];
            let separator = if separator.is_empty() {
                ", ".to_string()
            } else if separator.contains('#') {
                // a comment trails the member before; only the line break and indentation
                // are reused
                let indentation = &source
                    [TextRange::new(source.line_start(names[next].start()), names[next].start())];
                format!(",{line_ending}{indentation}")
            } else {
                separator.to_string()
            };
            let mut text = String::new();
            for member in members {
                text.push_str(&member);
                text.push_str(&separator);
            }
            edits.push(Edit::insertion(text, names[next].start()));
        }
        if !appended.is_empty() {
            let stmt = self
                .imports()
                .find(|candidate| candidate.stmt.start() == from.start())?;
            edits.push(
                Insertion::existing_import(stmt.stmt, self.tokens)?.into_edit(&appended.join(", ")),
            );
        }

        let written = |statement: &str| {
            order
                .write_statements(&format!("{statement}{line_ending}"), self.stylist)
                .map(|written| {
                    written
                        .strip_suffix(line_ending)
                        .unwrap_or(&written)
                        .to_string()
                })
        };
        let original = &source[from.range()];
        let mut edited = original.to_string();
        let mut sorted_edits: Vec<&Edit> = edits.iter().collect();
        sorted_edits.sort_by_key(|edit| std::cmp::Reverse(edit.start()));
        for edit in sorted_edits {
            let range = edit.range() - from.start();
            edited.replace_range(range.to_std_range(), edit.content().unwrap_or_default());
        }
        if written(original).as_deref() == Some(original)
            && let Some(rewritten) = written(&edited)
            && rewritten != edited
        {
            return Some(vec![Edit::range_replacement(rewritten, from.range())]);
        }
        Some(edits)
    }

    /// Look for an import already in this importer's module that
    /// satisfies the given request. If found, the corresponding
    /// import is returned along with the way in which the import
    /// satisfies the request.
    fn find(
        &self,
        importing_file: ImportingFile<'_>,
        request: &ImportRequest<'_>,
        available_at: TextSize,
    ) -> Option<ImportResponse<'a>> {
        let mut choice = None;
        let notebook = self.source.as_notebook();

        for import in self.imports() {
            // If the import statement comes after the spot where we
            // need the symbol, then we conservatively assume that
            // the import statement does not satisfy the request. It
            // is possible the import statement *could* satisfy the
            // request. For example, if `available_at` is inside a
            // function defined before the import statement. But this
            // only works if the function is known to be called *after*
            // the import statement executes. So... it's complicated.
            // In the worst case, we'll end up inserting a superfluous
            // import statement at the top of the module.
            //
            // Also, we can stop here since our import statements are
            // sorted by their start location in the source.
            if import.stmt.start() >= available_at {
                return choice;
            }

            if let Some(response) = import.satisfies(self.db, importing_file, request) {
                let partial = matches!(response.kind, ImportResponseKind::Partial(_));

                // The LSP doesn't support edits across cell boundaries.
                // Skip over imports that only partially satisfy the import
                // because they would require changes to the import (across cell boundaries).
                if partial
                    && let Some(notebook) = notebook
                    && notebook
                        .cell_offsets()
                        .has_cell_boundary(TextRange::new(import.stmt.start(), available_at))
                {
                    continue;
                }

                if choice
                    .as_ref()
                    .is_none_or(|c| !c.kind.is_prioritized_over(&response.kind))
                {
                    let is_top_priority =
                        matches!(response.kind, ImportResponseKind::Unqualified { .. });
                    choice = Some(response);
                    // When we find an unqualified import, it's (currently)
                    // impossible for any later import to override it in
                    // priority. So we can just quit here.
                    if is_top_priority {
                        return choice;
                    }
                }
            }
        }
        choice
    }

    /// Find the last `from __future__` import statement in the AST.
    fn find_last_future_import(&self, at: TextSize) -> Option<AstImport<'a>> {
        let notebook = self.source.as_notebook();

        self.imports()
            .take_while(|import| import.stmt.start() <= at)
            // Skip over imports from other cells.
            .skip_while(|import| {
                notebook.is_some_and(|notebook| {
                    notebook
                        .cell_offsets()
                        .has_cell_boundary(TextRange::new(import.stmt.start(), at))
                })
            })
            .take_while(|import| {
                import
                    .stmt
                    .as_import_from_stmt()
                    .is_some_and(|import_from| {
                        !import_from.is_lazy && import_from.module.as_deref() == Some("__future__")
                    })
            })
            .last()
    }

    fn imports(&self) -> impl Iterator<Item = AstImport<'a>> {
        self.imports.iter().filter_map(|import| {
            let stmt = import.node(self.parsed);
            let kind = match stmt {
                ast::Stmt::Import(node) => AstImportKind::Import(node),
                ast::Stmt::ImportFrom(node) => AstImportKind::ImportFrom(node),
                _ => return None,
            };
            Some(AstImport { stmt, kind })
        })
    }
}

/// the import block new imports join, as the range of the module's body it spans, and where an
/// order sorts each of its statements. `None` when the module has no import block, or one with
/// a statement the order can't read
type ImportBlock<O> = OnceCell<Option<(Range<usize>, Vec<<O as ImportOrdering>::Position>)>>;

/// the import block of each part of the file sorted on its own: each cell of a notebook, or the
/// file as a whole
type ImportBlocks<O> = OnceCell<Box<[ImportBlock<O>]>>;

/// an importer's sorting, with the type of its order erased
trait Sorting {
    /// the edits that make the additions in `placements` where the order puts them, or `None`
    /// when there is no order or the additions can't follow it
    fn placed_edits(&self, importer: &Importer<'_>, placements: &[Placement]) -> Option<Vec<Edit>>;

    /// whether the order writes `member` (imported `as asname`, if given) from `module` in a
    /// `from` import of its own
    fn imports_member_alone(&self, module: Option<&str>, asname: Option<&str>) -> bool;
}

/// an order found the first time an import is placed, and the import block as it sorts it
struct LazyOrder<O: ImportOrdering, F> {
    resolve: F,
    order: OnceCell<Option<O>>,
    blocks: ImportBlocks<O>,
}

impl<O: ImportOrdering, F: Fn() -> Option<O>> LazyOrder<O, F> {
    fn order(&self) -> Option<&O> {
        self.order.get_or_init(&self.resolve).as_ref()
    }
}

impl<O: ImportOrdering, F: Fn() -> Option<O>> Sorting for LazyOrder<O, F> {
    fn placed_edits(&self, importer: &Importer<'_>, placements: &[Placement]) -> Option<Vec<Edit>> {
        importer.placed_edits_in(self.order()?, &self.blocks, placements)
    }

    fn imports_member_alone(&self, module: Option<&str>, asname: Option<&str>) -> bool {
        self.order()
            .is_some_and(|order| order.imports_member_alone(module, asname))
    }
}

fn is_import(stmt: &ast::Stmt) -> bool {
    matches!(stmt, ast::Stmt::Import(_) | ast::Stmt::ImportFrom(_))
}

/// an import statement as a sorter reads it to sort it: an `import` by its first module, a
/// `from` import by its module and the member `order` lists first
fn sorted_import<'a>(stmt: &'a ast::Stmt, order: &impl ImportOrdering) -> Option<SortedImport<'a>> {
    match stmt {
        ast::Stmt::Import(import) => {
            let first = import.names.first()?;
            Some(SortedImport::Straight {
                name: first.name.as_str(),
                asname: first.asname.as_ref().map(ast::Identifier::as_str),
                is_lazy: import.is_lazy,
            })
        }
        ast::Stmt::ImportFrom(from) => {
            let first_member = from
                .names
                .iter()
                .map(|alias| {
                    (
                        alias.name.as_str(),
                        alias.asname.as_ref().map(ast::Identifier::as_str),
                    )
                })
                .min_by(|a, b| order.compare_members(*a, *b));
            Some(SortedImport::From {
                module: from.module.as_ref().map(ast::Identifier::as_str),
                level: from.level,
                first_member,
                is_lazy: from.is_lazy,
            })
        }
        _ => None,
    }
}

/// Detects source style once per file revision, shared by diagnostics and IDE features.
#[salsa::tracked(returns(ref), no_eq)]
fn stylist(db: &dyn Db, file: PythonFile<'_>) -> Stylist<'static> {
    let parsed = parsed_module(db, file).load(db);
    let source = source_text(db, file.file(db));
    Stylist::from_tokens(parsed.tokens(), &source).into_owned()
}

#[salsa::tracked(returns(ref), no_eq, heap_size=ruff_memory_usage::heap_size)]
fn top_level_imports(db: &dyn Db, file: PythonFile<'_>) -> Box<[AstNodeRef<ast::Stmt>]> {
    let parsed = parsed_module(db, file).load(db);
    TopLevelImports::find(&parsed)
}

/// A map of symbols in scope at a particular location in a module.
///
/// Users of an `Importer` must create this map via
/// [`Importer::members_in_scope_at`] in order to use the [`Importer::import`]
/// API. This map provides quick access to symbols in scope to help ensure that
/// the imports inserted are correct and do not conflict with existing symbols.
///
/// Note that this isn't perfect. At time of writing (2025-09-16), the importer
/// makes the trade-off that it's better to insert an incorrect import than to
/// silently do nothing. Perhaps in the future we can find a way to prompt end
/// users for a decision. This behavior is modeled after rust-analyzer, which
/// does the same thing for auto-import on unimported completions.
#[derive(Debug)]
pub struct MembersInScope<'ast> {
    at: TextSize,
    map: FxHashMap<Name, MemberInScope<'ast>>,
}

impl<'ast> MembersInScope<'ast> {
    /// An empty scope for importing a name that is already known to be unbound.
    /// this avoids querying inferred types while constructing a diagnostic fix, and serves a
    /// refactoring that has checked itself that the names it imports are free where it imports
    /// them, as one moving a definition does: the definition is still in scope where it is
    /// moving from, and would otherwise be taken for a conflict
    pub fn empty(at: TextSize) -> Self {
        Self {
            at,
            map: FxHashMap::default(),
        }
    }

    fn new(
        db: &'ast dyn Db,
        file: ProgramFile<'ast>,
        parsed: &'ast ParsedModuleRef,
        node: ast::AnyNodeRef<'_>,
        at: TextSize,
    ) -> MembersInScope<'ast> {
        let model = SemanticModel::new(db, file);
        let map = model
            .members_in_scope_at(node)
            .into_iter()
            .map(|(name, memberdef)| {
                let def = memberdef.first_reachable_definition;
                let kind = match *def.kind(db) {
                    DefinitionKind::Import(ref kind) => {
                        MemberImportKind::Imported(AstImportKind::Import(kind.import(parsed)))
                    }
                    DefinitionKind::ImportFrom(ref kind) => {
                        MemberImportKind::Imported(AstImportKind::ImportFrom(kind.import(parsed)))
                    }
                    DefinitionKind::StarImport(ref kind) => {
                        MemberImportKind::Imported(AstImportKind::ImportFrom(kind.import(parsed)))
                    }
                    _ => MemberImportKind::Other,
                };
                (
                    name,
                    MemberInScope {
                        ty: memberdef.ty,
                        kind,
                    },
                )
            })
            .collect();
        MembersInScope { at, map }
    }

    pub fn find_member(&self, symbol_name: &str) -> Option<&MemberInScope<'ast>> {
        self.map.get(symbol_name)
    }

    pub fn satisfies(
        &self,
        db: &dyn Db,
        importing_file: ImportingFile<'_>,
        request: &ImportRequest<'_>,
    ) -> bool {
        let symbol_text = request.symbol_text();
        let Some(member) = self.find_member(symbol_text) else {
            return false;
        };
        let MemberImportKind::Imported(ref ast_import) = member.kind else {
            return false;
        };
        ast_import.start() < self.at && member.satisfies_anywhere(db, importing_file, request)
    }
}

#[derive(Debug)]
pub struct MemberInScope<'ast> {
    pub ty: Type<'ast>,
    kind: MemberImportKind<'ast>,
}

impl MemberInScope<'_> {
    /// Returns true if this symbol satisfies the given import request. This
    /// attempts to take the definition site of the symbol into account.
    fn satisfies_anywhere(
        &self,
        db: &dyn Db,
        importing_file: ImportingFile<'_>,
        request: &ImportRequest<'_>,
    ) -> bool {
        let MemberImportKind::Imported(ref ast_import) = self.kind else {
            return false;
        };
        ast_import.satisfies(db, importing_file, request).is_some()
    }
}

/// A type describing how a symbol was defined.
#[derive(Debug)]
enum MemberImportKind<'ast> {
    /// A symbol was introduced through an import statement.
    Imported(AstImportKind<'ast>),
    /// A symbol was introduced through something other
    /// than an import statement.
    Other,
}

/// The edits needed to insert the import statement.
///
/// While this is usually just an edit to add an import statement (or
/// modify an existing one), it can also sometimes just be a change
/// to the text that should be inserted for a particular symbol. For
/// example, if one were to ask for `search` from the `re` module, and
/// `re` was already imported, then we'd return no edits for import
/// statements and the text `re.search` to use for the symbol.
#[derive(Debug)]
pub struct ImportAction {
    import: Option<Edit>,
    symbol_text: Box<str>,
    /// what `import` adds, for [`Importer::combined_edits`] to place it together with what
    /// other actions add
    placement: Option<Placement>,
}

/// what an import action adds to the file
#[derive(Debug, Clone)]
enum Placement {
    /// a new import statement: `import module as asname`, or `from module import member as
    /// asname`, `level` dots up
    Statement {
        module: Option<String>,
        level: u32,
        member: Option<String>,
        asname: Option<String>,
        /// where what is imported has to be available
        at: TextSize,
    },
    /// a member listed by the existing `from` import starting at `import`
    Member {
        import: TextSize,
        member: String,
        asname: Option<String>,
    },
}

impl Placement {
    /// the statement `request` adds when the file has no import that satisfies it
    fn statement(request: &ImportRequest<'_>, at: TextSize) -> Self {
        let member = match (&request.style, request.member) {
            (ImportStyle::ImportFrom, Some(member)) => Some(member.to_string()),
            _ => None,
        };
        Placement::Statement {
            module: Some(request.module.to_string()),
            level: 0,
            member,
            asname: request.asname.map(str::to_string),
            at,
        }
    }
}

/// a member of a `from` import, and the name it is imported as
type Member<'a> = (&'a str, Option<&'a str>);

/// the statements added before the import of the block at the index, or after the block's last
/// import for `None`, each with where it sorts
type StatementSlot<'s, P> = (Option<usize>, Vec<(P, &'s NewStatement<'s>)>);

/// `member`, or `member as asname`, as a `from` import lists it
fn imported_member((member, asname): Member) -> String {
    match asname {
        Some(asname) => format!("{member} as {asname}"),
        None => member.to_string(),
    }
}

/// the edit that changes `old`, the text at `start`, to `new`, touching only what differs.
/// `None` when nothing does
fn minimal_edit(old: &str, new: &str, start: TextSize) -> Option<Edit> {
    let prefix = old
        .char_indices()
        .zip(new.chars())
        .find(|((_, a), b)| a != b)
        .map_or(old.len().min(new.len()), |((index, _), _)| index);
    let (old_rest, new_rest) = (&old[prefix..], &new[prefix..]);
    let suffix = old_rest
        .chars()
        .rev()
        .zip(new_rest.chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum::<usize>();
    let old_changed = &old_rest[..old_rest.len() - suffix];
    let new_changed = &new_rest[..new_rest.len() - suffix];
    if old_changed.is_empty() && new_changed.is_empty() {
        return None;
    }
    let range = TextRange::at(
        start + TextSize::of(&old[..prefix]),
        TextSize::of(old_changed),
    );
    Some(if new_changed.is_empty() {
        Edit::range_deletion(range)
    } else {
        Edit::range_replacement(new_changed.to_string(), range)
    })
}

/// the statements and members `placements` add, combined: two members a sorter lists together
/// that a module the file doesn't import yet adds are one statement listing both, and the
/// members added to one existing `from` import, by its start, are listed together. each
/// statement's and import's members are sorted by `compare_members`, and `imports_member_alone`
/// says which member the sorter writes in a `from` import of its own
fn combined<'p>(
    placements: &'p [Placement],
    order: &impl MemberOrder,
) -> (Vec<NewStatement<'p>>, Vec<(TextSize, Vec<Member<'p>>)>) {
    let mut statements: Vec<NewStatement> = Vec::new();
    let mut members: Vec<(TextSize, Vec<Member>)> = Vec::new();
    for placement in placements {
        match placement {
            Placement::Statement {
                module,
                level,
                member,
                asname,
                at,
            } => {
                let joins = member.is_some()
                    && !order.imports_member_alone(module.as_deref(), asname.as_deref());
                if joins
                    && let Some(member) = member
                    && let Some(existing) = statements.iter_mut().find(|existing| {
                        existing.module == module.as_deref()
                            && existing.level == *level
                            && existing.members.first().is_some_and(|first| {
                                !order.imports_member_alone(existing.module, first.1)
                            })
                    })
                {
                    existing.members.push((member.as_str(), asname.as_deref()));
                    existing.at = existing.at.min(*at);
                    continue;
                }
                statements.push(NewStatement {
                    module: module.as_deref(),
                    level: *level,
                    members: member
                        .iter()
                        .map(|member| (member.as_str(), asname.as_deref()))
                        .collect(),
                    asname: if member.is_some() {
                        None
                    } else {
                        asname.as_deref()
                    },
                    at: *at,
                });
            }
            Placement::Member {
                import,
                member,
                asname,
            } => {
                let added = (member.as_str(), asname.as_deref());
                match members.iter_mut().find(|(start, _)| start == import) {
                    Some((_, listed)) => listed.push(added),
                    None => members.push((*import, vec![added])),
                }
            }
        }
    }
    for statement in &mut statements {
        statement
            .members
            .sort_by(|a, b| order.compare_members(*a, *b));
        statement.members.dedup();
    }
    for (_, added) in &mut members {
        added.sort_by(|a, b| order.compare_members(*a, *b));
        added.dedup();
    }
    (statements, members)
}

/// how a sorter lists the members of a `from` import, as [`combined`] needs to know it
trait MemberOrder {
    fn compare_members(&self, a: Member, b: Member) -> std::cmp::Ordering;

    fn imports_member_alone(&self, module: Option<&str>, asname: Option<&str>) -> bool;
}

impl<O: ImportOrdering> MemberOrder for O {
    fn compare_members(&self, a: Member, b: Member) -> std::cmp::Ordering {
        ImportOrdering::compare_members(self, a, b)
    }

    fn imports_member_alone(&self, module: Option<&str>, asname: Option<&str>) -> bool {
        ImportOrdering::imports_member_alone(self, module, asname)
    }
}

/// no sorter: members are listed in the order they are added, all in one `from` import
struct Unordered;

impl MemberOrder for Unordered {
    fn compare_members(&self, _: Member, _: Member) -> std::cmp::Ordering {
        std::cmp::Ordering::Equal
    }

    fn imports_member_alone(&self, _: Option<&str>, _: Option<&str>) -> bool {
        false
    }
}

/// an import statement to add: `import module as asname`, or `from module import members`
#[derive(Debug)]
struct NewStatement<'a> {
    module: Option<&'a str>,
    level: u32,
    /// the members a `from` import lists, sorted; empty for an `import`
    members: Vec<Member<'a>>,
    asname: Option<&'a str>,
    at: TextSize,
}

impl<'a> NewStatement<'a> {
    fn sorted_import(&self) -> SortedImport<'a> {
        if self.members.is_empty() {
            SortedImport::Straight {
                name: self.module.unwrap_or_default(),
                asname: self.asname,
                is_lazy: false,
            }
        } else {
            SortedImport::From {
                module: self.module,
                level: self.level,
                first_member: self.members.first().copied(),
                is_lazy: false,
            }
        }
    }

    fn text(&self) -> String {
        let module = self.module.unwrap_or_default();
        if self.members.is_empty() {
            return match self.asname {
                Some(asname) => format!("import {module} as {asname}"),
                None => format!("import {module}"),
            };
        }
        let members = self
            .members
            .iter()
            .map(|(member, asname)| match asname {
                Some(asname) => format!("{member} as {asname}"),
                None => (*member).to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "from {}{module} import {members}",
            ".".repeat(self.level as usize)
        )
    }
}

impl ImportAction {
    /// Returns an edit to insert an import statement.
    pub fn import(&self) -> Option<&Edit> {
        self.import.as_ref()
    }

    /// Returns the symbol text that should be used.
    ///
    /// Usually this is identical to the symbol text given to the corresponding
    /// [`ImportRequest`], but this may sometimes be fully qualified based on
    /// existing imports or import preferences.
    pub fn symbol_text(&self) -> &str {
        &self.symbol_text
    }
}

/// A borrowed AST of a Python import statement.
#[derive(Debug, Clone, Copy)]
struct AstImport<'ast> {
    /// The original AST statement containing the import.
    stmt: &'ast ast::Stmt,
    /// The specific type of import.
    ///
    /// Storing this means we can do exhaustive case analysis
    /// on the type of the import without needing to constantly
    /// unwrap it from a more general `Stmt`. Still, we keep the
    /// `Stmt` around because some APIs want that.
    kind: AstImportKind<'ast>,
}

impl<'ast> AstImport<'ast> {
    /// Returns whether this import satisfies the given request.
    ///
    /// If it does, then this returns *how* the import satisfies
    /// the request.
    fn satisfies(
        self,
        db: &'_ dyn Db,
        importing_file: ImportingFile<'_>,
        request: &ImportRequest<'_>,
    ) -> Option<ImportResponse<'ast>> {
        self.kind
            .satisfies(db, importing_file, request)
            .map(|kind| ImportResponse { import: self, kind })
    }
}

/// The specific kind of import.
#[derive(Debug, Clone, Copy)]
enum AstImportKind<'ast> {
    Import(&'ast ast::StmtImport),
    ImportFrom(&'ast ast::StmtImportFrom),
}

impl<'ast> AstImportKind<'ast> {
    fn start(&self) -> TextSize {
        match self {
            AstImportKind::Import(ast) => ast.start(),
            AstImportKind::ImportFrom(ast) => ast.start(),
        }
    }

    /// Returns whether this import satisfies the given request.
    ///
    /// If it does, then this returns *how* the import satisfies
    /// the request.
    fn satisfies<'importer>(
        &'importer self,
        db: &'_ dyn Db,
        importing_file: ImportingFile<'_>,
        request: &ImportRequest<'_>,
    ) -> Option<ImportResponseKind<'ast>> {
        match *self {
            AstImportKind::Import(ast) => {
                if request.force_style && !matches!(request.style, ImportStyle::Import) {
                    return None;
                }
                let alias = ast.names.iter().find(|alias| {
                    // `import numpy` does not satisfy a request for `import numpy as np`: it
                    // brings the module into scope, but not under the name that was asked for
                    alias.name.as_str() == request.module
                        && (request.asname.is_none()
                            || alias.asname.as_ref().map(ast::Identifier::as_str) == request.asname)
                })?;
                Some(ImportResponseKind::Qualified { alias })
            }
            AstImportKind::ImportFrom(ast) => {
                // If the request is for a module itself, then we
                // assume that it can never be satisfies by a
                // `from ... import ...` statement. For example, a
                // `request for collections.abc` needs an
                // `import collections.abc`. Now, there could be a
                // `from collections import abc`, and we could
                // plausibly consider that a match and return a
                // symbol text of `abc`. But it's not clear if that's
                // the right choice or not.
                let member = request.member?;

                if request.force_style && !matches!(request.style, ImportStyle::ImportFrom) {
                    return None;
                }

                let module = ModuleName::from_import_statement(db, importing_file, ast).ok()?;
                if module.as_str() != request.module {
                    return None;
                }
                let kind = ast
                    .names
                    .iter()
                    .find(|alias| match request.asname {
                        // `from x import y` binds `y`, not the `z` that was asked for
                        Some(asname) => {
                            alias.name.as_str() == member
                                && alias.asname.as_ref().map(ast::Identifier::as_str)
                                    == Some(asname)
                        }
                        None => alias.name.as_str() == "*" || alias.name.as_str() == member,
                    })
                    .map(|alias| ImportResponseKind::Unqualified { alias })
                    .unwrap_or(ImportResponseKind::Partial(ast));
                Some(kind)
            }
        }
    }
}

/// A request to import a module into the global scope of a Python module.
#[derive(Debug, Clone, Copy)]
pub struct ImportRequest<'a> {
    /// The module from which the symbol should be imported (e.g.,
    /// `foo`, in `from foo import bar`).
    module: &'a str,
    /// The member to import (e.g., `bar`, in `from foo import bar`).
    ///
    /// When `member` is absent, then this request reflects an import
    /// of the module itself. i.e., `import module`.
    member: Option<&'a str>,
    /// The name to bind the module or member to (e.g., `np`, in `import
    /// numpy as np`, or `PP` in `from pathlib import PurePath as PP`).
    asname: Option<&'a str>,
    /// The preferred style to use when importing the symbol (e.g.,
    /// `import foo` or `from foo import bar`).
    ///
    /// This style isn't respected if the `module` already has
    /// an import statement. In that case, the existing style is
    /// respected.
    style: ImportStyle,
    /// Whether the import style ought to be forced for correctness
    /// reasons. For example, to avoid shadowing or introducing a
    /// conflicting name.
    force_style: bool,
}

impl<'a> ImportRequest<'a> {
    /// Create a new [`ImportRequest`] from a `module` and `member`.
    ///
    /// If `module` has no existing imports, the symbol should be
    /// imported using the `import` statement.
    pub fn import(module: &'a str, member: &'a str) -> Self {
        Self {
            module,
            member: Some(member),
            asname: None,
            style: ImportStyle::Import,
            force_style: false,
        }
    }

    /// Create a new [`ImportRequest`] from a module and member.
    ///
    /// If `module` has no existing imports, the symbol should be
    /// imported using the `import from` statement.
    pub fn import_from(module: &'a str, member: &'a str) -> Self {
        Self {
            module,
            member: Some(member),
            asname: None,
            style: ImportStyle::ImportFrom,
            force_style: false,
        }
    }

    /// Create a new [`ImportRequest`] from a module and member, bound
    /// under a name of its own, as in `from pathlib import PurePath as PP`.
    ///
    /// If `module` has no existing imports, the symbol should be
    /// imported using the `import from` statement.
    pub fn import_from_as(module: &'a str, member: &'a str, asname: &'a str) -> Self {
        Self {
            asname: Some(asname),
            ..Self::import_from(module, member)
        }
    }

    /// The text the requested symbol is referred to by once it is
    /// imported as asked: its alias, the member, or the module.
    fn symbol_text(&self) -> &'a str {
        self.asname.or(self.member).unwrap_or(self.module)
    }

    /// The member as a `from ... import` lists it: `member`, or
    /// `member as asname`.
    fn imported_member(&self) -> String {
        match (self.member, self.asname) {
            (Some(member), Some(asname)) => format!("{member} as {asname}"),
            (member, _) => member.unwrap_or(self.module).to_string(),
        }
    }

    /// Create a new [`ImportRequest`] for bringing the given module
    /// into scope.
    ///
    /// This is for just importing the module itself, always via an
    /// `import` statement.
    pub fn module(module: &'a str) -> Self {
        Self {
            module,
            member: None,
            asname: None,
            style: ImportStyle::Import,
            force_style: false,
        }
    }

    /// Create a new [`ImportRequest`] for bringing the given module
    /// into scope under a name of its own, as in `import numpy as np`.
    pub fn module_as(module: &'a str, asname: &'a str) -> Self {
        Self {
            asname: Some(asname),
            ..Self::module(module)
        }
    }

    /// Causes this request to become a command. This will force the
    /// requested import style, even if another style would be more
    /// appropriate generally.
    #[must_use]
    pub fn force(self) -> Self {
        Self {
            force_style: true,
            ..self
        }
    }

    /// Attempts to change the import request style so that the chances
    /// of an import conflict are minimized (although not always reduced
    /// to zero).
    fn avoid_conflicts(
        self,
        db: &dyn Db,
        importing_file: ImportingFile<'_>,
        members: &MembersInScope,
    ) -> Self {
        let Some(member) = self.member else {
            return Self {
                style: ImportStyle::Import,
                ..self
            };
        };
        // a member asked for under an alias can only be imported by a
        // `from ... import ... as ...`, so there is no other style to fall
        // back on: whoever asks for an alias has chosen the name it binds
        if self.asname.is_some() {
            return self;
        }
        match (members.map.get(self.module), members.map.get(member)) {
            // Neither symbol exists, so we can just proceed as
            // normal.
            (None, None) => self,
            // The symbol we want to import already exists but
            // the module symbol does not, so we can import the
            // symbol in a qualified way safely.
            (None, Some(member)) => {
                // ... unless the symbol we want is already
                // imported, then leave it as-is.
                if member.satisfies_anywhere(db, importing_file, &self) {
                    return self;
                }
                Self {
                    style: ImportStyle::Import,
                    force_style: true,
                    ..self
                }
            }
            // The symbol we want to import doesn't exist but
            // the module does. So we can import the symbol we
            // want *unqualified* safely.
            //
            // ... unless the module symbol we found here is
            // actually a module symbol.
            (
                Some(&MemberInScope {
                    ty: Type::ModuleLiteral(_),
                    ..
                }),
                None,
            ) => self,
            (Some(_), None) => Self {
                style: ImportStyle::ImportFrom,
                force_style: true,
                ..self
            },
            // Both the module and the member symbols are in
            // scope. We *assume* that the module symbol is in
            // scope because it is imported. Since the member
            // symbol is definitively in scope, we attempt a
            // qualified import.
            //
            // This could lead to a situation where we add an
            // `import` that is shadowed by some other symbol.
            // This is unfortunate, but it's not clear what we
            // should do instead. rust-analyzer will still add
            // the conflicting import. I think that's the wiser
            // choice, instead of silently doing nothing or
            // silently omitting the symbol from completions.
            // (I suppose the best choice would be to ask the
            // user for an alias for the import or something.)
            (Some(_), Some(_)) => Self {
                style: ImportStyle::Import,
                force_style: false,
                ..self
            },
        }
    }
}

impl std::fmt::Display for ImportRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match (&self.style, self.member) {
            (ImportStyle::ImportFrom, Some(_)) => {
                write!(f, "from {} import {}", self.module, self.imported_member())
            }
            _ => {
                write!(f, "import {}", self.module)?;
                match self.asname {
                    Some(asname) => write!(f, " as {asname}"),
                    None => Ok(()),
                }
            }
        }
    }
}

/// The response to an import request.
#[derive(Debug)]
struct ImportResponse<'ast> {
    import: AstImport<'ast>,
    kind: ImportResponseKind<'ast>,
}

/// The kind of response to an import request.
///
/// This encodes the answer to the question: how does a given import
/// statement satisfy an [`ImportRequest`]? This encodes the different
/// degrees to the request is satisfied.
#[derive(Debug)]
enum ImportResponseKind<'ast> {
    /// The import satisfies the request as-is. The symbol is already
    /// imported directly and may be used unqualified.
    ///
    /// This always corresponds to a `from <...> import <...>`
    /// statement. Note that `<...>` may be a wildcard import!
    Unqualified {
        /// The specific alias in the `from <...> import <...>`
        /// statement that satisfied the request's `member`.
        alias: &'ast ast::Alias,
    },
    /// The necessary module is imported, but the symbol itself is not
    /// in scope. The symbol can be used via `module.symbol`.
    ///
    /// This always corresponds to a `import <...>` statement.
    Qualified {
        /// The specific alias in the import statement that
        /// satisfied the request's `module`.
        alias: &'ast ast::Alias,
    },
    /// The necessary module is imported via `from module import ...`,
    /// but the desired symbol is not listed in `...`.
    ///
    /// This always corresponds to a `from <...> import <...>`
    /// statement.
    ///
    /// It is guaranteed that this never contains a wildcard import.
    /// (otherwise, this import wouldn't be partial).
    Partial(&'ast ast::StmtImportFrom),
}

impl ImportResponseKind<'_> {
    /// Returns true if this import statement kind should be
    /// prioritized over the one given.
    ///
    /// This assumes that `self` occurs before `other` in the source
    /// code.
    fn is_prioritized_over(&self, other: &ImportResponseKind<'_>) -> bool {
        self.priority() <= other.priority()
    }

    /// Returns an integer reflecting the "priority" of this
    /// import kind relative to other import statements.
    ///
    /// Lower values indicate higher priority.
    fn priority(&self) -> usize {
        match *self {
            ImportResponseKind::Unqualified { .. } => 0,
            ImportResponseKind::Partial(_) => 1,
            // N.B. When given the choice between adding a
            // name to an existing `from ... import ...`
            // statement and using an existing `import ...`
            // in a qualified manner, we currently choose
            // the former. Originally we preferred qualification,
            // but there is some evidence that this violates
            // expectations.
            //
            // Ref: https://github.com/astral-sh/ty/issues/1274#issuecomment-3352233790
            ImportResponseKind::Qualified { .. } => 2,
        }
    }
}

/// The style of a Python import statement.
#[derive(Debug, Clone, Copy)]
enum ImportStyle {
    /// Import the symbol using the `import` statement (e.g. `import
    /// foo; foo.bar`).
    Import,
    /// Import the symbol using the `from` statement (e.g. `from foo
    /// import bar; bar`).
    ImportFrom,
}

/// An AST visitor for extracting top-level imports.
struct TopLevelImports<'ast> {
    parsed: &'ast ParsedModuleRef,
    level: u64,
    imports: Vec<AstNodeRef<ast::Stmt>>,
}

impl<'ast> TopLevelImports<'ast> {
    /// Find all top-level imports from the given AST of a Python module.
    fn find(parsed: &'ast ParsedModuleRef) -> Box<[AstNodeRef<ast::Stmt>]> {
        let mut visitor = TopLevelImports {
            parsed,
            level: 0,
            imports: Vec::new(),
        };
        visitor.visit_body(parsed.suite());
        visitor.imports.into_boxed_slice()
    }
}

impl<'ast> SourceOrderVisitor<'ast> for TopLevelImports<'ast> {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        match *stmt {
            ast::Stmt::Import(_) | ast::Stmt::ImportFrom(_) => {
                if self.level == 0 {
                    self.imports.push(AstNodeRef::new(self.parsed, stmt));
                }
            }
            _ => {
                // OK because it's not practical for the source code
                // depth of a Python to exceed a u64.
                //
                // Also, it is perhaps a bit too eager to increment
                // this for every non-import statement, particularly
                // compared to the more refined scope tracking in the
                // semantic index builder. However, I don't think
                // we need anything more refined here. We only care
                // about top-level imports. So as soon as we get into
                // something nested, we can bail out.
                //
                // Although, this does mean, e.g.,
                //
                //     if predicate:
                //         import whatever
                //
                // at the module scope is not caught here. If we
                // need those imports, I think we'll just want some
                // more case analysis with more careful `level`
                // incrementing.
                self.level = self.level.checked_add(1).unwrap();
                walk_stmt(self, stmt);
                // Always OK because we can only be here after
                // a successful +1 from above.
                self.level = self.level.checked_sub(1).unwrap();
            }
        }
    }

    #[inline]
    fn enter_node(&mut self, node: ast::AnyNodeRef<'ast>) -> TraversalSignal {
        if node.is_statement() {
            TraversalSignal::Traverse
        } else {
            TraversalSignal::Skip
        }
    }
}
