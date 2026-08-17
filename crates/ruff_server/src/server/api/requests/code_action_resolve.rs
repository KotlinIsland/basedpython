use lsp_server::ErrorCode;
use lsp_types::CodeActionResolveRequest;
use lsp_types::{self as types};

use ruff_linter::codes::Rule;

use crate::edit::WorkspaceEditTracker;
use crate::fix::Fixes;
use crate::resolve::is_document_excluded_for_formatting;
use crate::server::Result;
use crate::server::SupportedCodeAction;
use crate::server::api::LSPResult;
use crate::session::Client;
use crate::session::{DocumentQuery, DocumentSnapshot, ResolvedClientCapabilities, Session};
use crate::{PositionEncoding, TextDocument};

pub(crate) struct CodeActionResolve;

impl super::RequestHandler for CodeActionResolve {
    type RequestType = CodeActionResolveRequest;
}

impl super::BackgroundRequestHandler for CodeActionResolve {
    type Snapshot = std::result::Result<DocumentSnapshot, String>;

    fn snapshot(session: &Session, action: &types::CodeAction) -> Self::Snapshot {
        let data = action
            .data
            .clone()
            .ok_or_else(|| "it doesn't contain Ruff's document URI payload".to_string())?;

        let uri: lsp_types::Uri = serde_json::from_value(data)
            .map_err(|err| format!("its Ruff document URI payload is invalid: {err}"))?;

        session
            .take_snapshot(uri.clone())
            .ok_or_else(|| format!("document `{uri}` isn't open"))
    }

    fn run_with_snapshot(
        snapshot: Self::Snapshot,
        _client: &Client,
        mut action: types::CodeAction,
    ) -> Result<types::CodeAction> {
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(err) => {
                tracing::warn!("Returning code action unchanged because {err}.");
                return Ok(action);
            }
        };

        let query = snapshot.query();

        let code_actions = SupportedCodeAction::from_kind(
            action
                .kind
                .clone()
                .ok_or(anyhow::anyhow!("No kind was given for code action"))
                .with_failure_code(ErrorCode::InvalidParams)?,
        )
        .collect::<Vec<_>>();

        // Ensure that the code action maps to _exactly one_ supported code action
        let [action_kind] = code_actions.as_slice() else {
            return Err(anyhow::anyhow!(
                "Code action resolver did not expect code action kind {:?}",
                action.kind.as_ref().unwrap()
            ))
            .with_failure_code(ErrorCode::InvalidParams);
        };

        match action_kind {
            SupportedCodeAction::SourceFixAll | SupportedCodeAction::SourceOrganizeImports
                if snapshot.is_notebook_cell() =>
            {
                // This should never occur because we ignore generating these code actions for a
                // notebook cell in the `textDocument/codeAction` request handler.
                return Err(anyhow::anyhow!(
                    "Code action resolver cannot resolve {:?} for a notebook cell",
                    action_kind.to_kind().as_str()
                ))
                .with_failure_code(ErrorCode::InvalidParams);
            }
            _ => {}
        }

        action.edit = match action_kind {
            SupportedCodeAction::SourceFixAll | SupportedCodeAction::NotebookSourceFixAll => Some(
                resolve_edit_for_fix_all(
                    query,
                    snapshot.resolved_client_capabilities(),
                    snapshot.encoding(),
                )
                .with_failure_code(ErrorCode::InternalError)?,
            ),
            SupportedCodeAction::SourceOrganizeImports
            | SupportedCodeAction::NotebookSourceOrganizeImports => Some(
                resolve_edit_for_organize_imports(
                    query,
                    snapshot.resolved_client_capabilities(),
                    snapshot.encoding(),
                )
                .with_failure_code(ErrorCode::InternalError)?,
            ),
            SupportedCodeAction::SourceOptimizeImports => Some(
                resolve_edit_for_optimize_imports(
                    query,
                    snapshot.resolved_client_capabilities(),
                    snapshot.encoding(),
                )
                .with_failure_code(ErrorCode::InternalError)?,
            ),
            SupportedCodeAction::SourceFormatAndOrganizeImports => Some(
                resolve_edit_for_format_and_organize_imports(&snapshot)
                    .with_failure_code(ErrorCode::InternalError)?,
            ),
            SupportedCodeAction::SourceFormatAndOptimizeImports => Some(
                resolve_edit_for_format_and_optimize_imports(&snapshot)
                    .with_failure_code(ErrorCode::InternalError)?,
            ),
            SupportedCodeAction::QuickFix => {
                // The client may ask us to resolve a code action, as it has no way of knowing
                // whether e.g. `command` field will be filled out by the resolution callback.
                return Ok(action);
            }
        };

        Ok(action)
    }
}

pub(super) fn resolve_edit_for_fix_all(
    query: &DocumentQuery,
    client_capabilities: &ResolvedClientCapabilities,
    encoding: PositionEncoding,
) -> crate::Result<types::WorkspaceEdit> {
    let mut tracker = WorkspaceEditTracker::new(client_capabilities);
    tracker.set_fixes_for_document(fix_all_edit(query, encoding)?, query.version())?;
    Ok(tracker.into_workspace_edit())
}

pub(super) fn fix_all_edit(
    query: &DocumentQuery,
    encoding: PositionEncoding,
) -> crate::Result<Fixes> {
    crate::fix::fix_all(query, &query.settings().linter, encoding)
}

pub(super) fn resolve_edit_for_organize_imports(
    query: &DocumentQuery,
    client_capabilities: &ResolvedClientCapabilities,
    encoding: PositionEncoding,
) -> crate::Result<types::WorkspaceEdit> {
    let mut tracker = WorkspaceEditTracker::new(client_capabilities);
    tracker.set_fixes_for_document(organize_imports_edit(query, encoding)?, query.version())?;
    Ok(tracker.into_workspace_edit())
}

/// The rules that put a module's imports in order, the way isort does.
fn import_sorting_rules() -> impl Iterator<Item = Rule> {
    [
        Rule::UnsortedImports,       // I001
        Rule::MissingRequiredImport, // I002
        // Note: ModuleImportNotAtTopOfFile's fixes are unsafe. We include them
        // here in order to match isort's behaviour and what we believe
        // developers want. Since the fixes are unsafe, we're relying on this
        // edit action not performing them unless the user has opted-in to these
        // fixes in their settings (i.e: `extend-safe-fixes` in the
        // `pyproject.toml` or similar).
        Rule::ModuleImportNotAtTopOfFile, // E402
    ]
    .into_iter()
}

/// Builds the settings for a fix pass that runs exactly `rules` and nothing else.
fn settings_for_rules(
    query: &DocumentQuery,
    rules: impl Iterator<Item = Rule>,
) -> ruff_linter::settings::LinterSettings {
    let mut linter_settings = query.settings().linter.clone();
    linter_settings.rules = rules.collect();
    linter_settings
}

pub(super) fn organize_imports_edit(
    query: &DocumentQuery,
    encoding: PositionEncoding,
) -> crate::Result<Fixes> {
    let linter_settings = settings_for_rules(query, import_sorting_rules());
    crate::fix::fix_all(query, &linter_settings, encoding)
}

/// Sorts a module's imports and drops the ones nothing uses.
///
/// This is deliberately more than `source.organizeImports`, which only sorts because that is
/// isort's scope. An editor's *Optimize Imports* means both halves — PyCharm's own Python
/// implementation removes unused imports — so the sorting rules are run together with F401.
///
/// F401's fix is only safe outside `__init__.py`; inside one, removing an import can break a
/// package's public interface, so the rule marks that fix unsafe and it is skipped here unless the
/// user has opted into unsafe fixes.
pub(super) fn optimize_imports_edit(
    query: &DocumentQuery,
    encoding: PositionEncoding,
) -> crate::Result<Fixes> {
    let linter_settings = optimize_imports_settings(query);
    crate::fix::fix_all(query, &linter_settings, encoding)
}

pub(super) fn resolve_edit_for_optimize_imports(
    query: &DocumentQuery,
    client_capabilities: &ResolvedClientCapabilities,
    encoding: PositionEncoding,
) -> crate::Result<types::WorkspaceEdit> {
    let mut tracker = WorkspaceEditTracker::new(client_capabilities);
    tracker.set_fixes_for_document(optimize_imports_edit(query, encoding)?, query.version())?;
    Ok(tracker.into_workspace_edit())
}

pub(super) fn resolve_edit_for_format_and_optimize_imports(
    snapshot: &DocumentSnapshot,
) -> crate::Result<types::WorkspaceEdit> {
    let query = snapshot.query();
    let mut tracker = WorkspaceEditTracker::new(snapshot.resolved_client_capabilities());
    tracker.set_fixes_for_document(format_and_optimize_imports_edit(snapshot)?, query.version())?;
    Ok(tracker.into_workspace_edit())
}

pub(super) fn resolve_edit_for_format_and_organize_imports(
    snapshot: &DocumentSnapshot,
) -> crate::Result<types::WorkspaceEdit> {
    let query = snapshot.query();
    let mut tracker = WorkspaceEditTracker::new(snapshot.resolved_client_capabilities());
    tracker.set_fixes_for_document(format_and_organize_imports_edit(snapshot)?, query.version())?;
    Ok(tracker.into_workspace_edit())
}

fn optimize_imports_settings(query: &DocumentQuery) -> ruff_linter::settings::LinterSettings {
    settings_for_rules(
        query,
        import_sorting_rules().chain(std::iter::once(Rule::UnusedImport)), // F401
    )
}

/// Optimizes a module's imports and formats it, as a single edit.
///
/// See [`format_and_imports_edit`] for why the two are composed here rather than asked for
/// separately.
pub(super) fn format_and_optimize_imports_edit(
    snapshot: &DocumentSnapshot,
) -> crate::Result<Fixes> {
    let settings = optimize_imports_settings(snapshot.query());
    format_and_imports_edit(snapshot, &settings)
}

/// Sorts a module's imports and formats it, as a single edit.
///
/// The same as [`format_and_optimize_imports_edit`] but for F401: laying a file out is not licence
/// to delete anything from it, so *Reformat Code* sorts imports without pruning them. Dropping the
/// unused ones is what *Optimize Imports* is for, and the user asks for that separately.
pub(super) fn format_and_organize_imports_edit(
    snapshot: &DocumentSnapshot,
) -> crate::Result<Fixes> {
    let settings = settings_for_rules(snapshot.query(), import_sorting_rules());
    format_and_imports_edit(snapshot, &settings)
}

/// Runs an import pass and the formatter against one buffer, as a single edit.
///
/// Asking a client to run the two separately cannot be made correct: the second request is answered
/// against whatever text the server last saw, so the client has to wait for the first edit to be
/// applied *and* for its own `didChange` to arrive before sending it. Composing the two here runs
/// them against one buffer, so there is one diff, one undo step, and no window in which the
/// document is sorted but not yet formatted.
///
/// The order matters and is fixed: imports move first, then the formatter lays out whatever that
/// left behind. Running it the other way round leaves the file needing a format again.
///
/// Notebooks are not handled — their cells are fixed as a group and formatted one at a time, which
/// a single whole-document diff cannot express. They keep the separate `notebook.source.*` actions.
fn format_and_imports_edit(
    snapshot: &DocumentSnapshot,
    import_settings: &ruff_linter::settings::LinterSettings,
) -> crate::Result<Fixes> {
    let query = snapshot.query();
    let Ok(document) = query.as_single_document() else {
        return Ok(Fixes::default());
    };
    if query.as_notebook().is_some() {
        return Ok(Fixes::default());
    }

    let source = document.contents();

    let sorted = crate::fix::fix_all_text(query, import_settings)?;
    let sorted = sorted.as_deref().unwrap_or(source);

    let settings = query.settings();
    let file_path = query.virtual_file_path();
    let formatted = if is_document_excluded_for_formatting(
        &file_path,
        &settings.file_resolver,
        &settings.formatter,
        document.language_id(),
    ) {
        None
    } else {
        // The formatter reads a whole document, so the fixed source is handed to it as one. The
        // version is irrelevant here: this document never leaves this function, and the edit is
        // stamped with the real document's version by the caller.
        let intermediate = TextDocument::new(sorted.to_string(), document.version());
        crate::format::format(
            &intermediate,
            query.source_type_for_format(),
            &settings.formatter,
            &file_path,
            snapshot
                .client_settings()
                .editor_settings()
                .format_backend(),
        )?
        .into_formatted()
    };

    let modified = formatted.as_deref().unwrap_or(sorted);
    if modified == source {
        return Ok(Fixes::default());
    }

    Ok(crate::fix::text_document_fixes(
        query,
        source,
        modified,
        snapshot.encoding(),
    ))
}
