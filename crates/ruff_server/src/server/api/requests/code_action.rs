use lsp_server::ErrorCode;
use lsp_types::{self as types, CodeActionRequest, CodeActionResponse};
use ruff_python_ast::{SourceType, TomlSourceType};
use rustc_hash::FxHashSet;
use types::CodeActionKind;

use crate::DIAGNOSTIC_NAME;
use crate::edit::WorkspaceEditTracker;
use crate::lint::{DiagnosticFix, fixes_for_diagnostics};
use crate::resolve::is_document_excluded_for_linting;
use crate::server::Result;
use crate::server::SupportedCodeAction;
use crate::server::api::LSPResult;
use crate::session::{Client, DocumentSnapshot};

use super::code_action_resolve::{
    resolve_edit_for_fix_all, resolve_edit_for_format_and_optimize_imports,
    resolve_edit_for_format_and_organize_imports, resolve_edit_for_optimize_imports,
    resolve_edit_for_organize_imports,
};

pub(crate) struct CodeActions;

impl super::RequestHandler for CodeActions {
    type RequestType = CodeActionRequest;
}

impl super::BackgroundDocumentRequestHandler for CodeActions {
    super::define_document_uri!(params: &types::CodeActionParams);

    fn run_with_snapshot(
        snapshot: Self::Snapshot,
        _client: &Client,
        params: types::CodeActionParams,
    ) -> Result<Option<Vec<types::CodeActionResponse>>> {
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(uri) => {
                tracing::warn!("Returning no code actions because document `{uri}` isn't open.");
                return Ok(None);
            }
        };

        let mut response = Vec::new();

        let query = snapshot.query();

        let is_python = match query.source_type_for_lint() {
            SourceType::Python(_) => true,
            SourceType::Toml(TomlSourceType::Pyproject | TomlSourceType::Ruff) => false,
            SourceType::Toml(_) | SourceType::Markdown => return Ok(Some(response)),
        };

        let document_path = query.virtual_file_path();
        let settings = query.settings();

        if is_document_excluded_for_linting(
            &document_path,
            &settings.file_resolver,
            &settings.linter,
            query.text_document_language_id(),
        ) {
            return Ok(Some(response));
        }

        let supported_code_actions = supported_code_actions(params.context.only.clone());
        let asked_for = params.context.only.as_deref();

        let fixes = fixes_for_diagnostics(params.context.diagnostics)
            .with_failure_code(ErrorCode::InternalError)?;

        if snapshot.client_settings().fix_violation()
            && supported_code_actions.contains(&SupportedCodeAction::QuickFix)
        {
            response
                .extend(quick_fix(&snapshot, &fixes).with_failure_code(ErrorCode::InternalError)?);
        }

        if is_python
            && snapshot.client_settings().noqa_comments()
            && supported_code_actions.contains(&SupportedCodeAction::QuickFix)
        {
            response.extend(noqa_comments(&snapshot, &fixes));
        }

        if snapshot.client_settings().fix_all() {
            if supported_code_actions.contains(&SupportedCodeAction::SourceFixAll) {
                if snapshot.is_notebook_cell() {
                    // This is ignore here because the client requests this code action for each
                    // cell in parallel and the server would send a workspace edit with the same
                    // content which would result in applying the same edit multiple times
                    // resulting in (possibly) duplicate code.
                    tracing::debug!("Ignoring `source.fixAll` code action for a notebook cell");
                } else {
                    response.push(fix_all(&snapshot).with_failure_code(ErrorCode::InternalError)?);
                }
            } else if supported_code_actions.contains(&SupportedCodeAction::NotebookSourceFixAll) {
                response
                    .push(notebook_fix_all(&snapshot).with_failure_code(ErrorCode::InternalError)?);
            }
        }

        if is_python && snapshot.client_settings().organize_imports() {
            if supported_code_actions.contains(&SupportedCodeAction::SourceOrganizeImports) {
                if snapshot.is_notebook_cell() {
                    // This is ignore here because the client requests this code action for each
                    // cell in parallel and the server would send a workspace edit with the same
                    // content which would result in applying the same edit multiple times
                    // resulting in (possibly) duplicate code.
                    tracing::debug!(
                        "Ignoring `source.organizeImports` code action for a notebook cell"
                    );
                } else {
                    response.push(
                        organize_imports(&snapshot).with_failure_code(ErrorCode::InternalError)?,
                    );
                }
            } else if supported_code_actions
                .contains(&SupportedCodeAction::NotebookSourceOrganizeImports)
            {
                response.push(
                    notebook_organize_imports(&snapshot)
                        .with_failure_code(ErrorCode::InternalError)?,
                );
            }
        }

        if is_python && !snapshot.is_notebook_cell() {
            if named_in(asked_for, &crate::SOURCE_OPTIMIZE_IMPORTS_RUFF) {
                response
                    .push(optimize_imports(&snapshot).with_failure_code(ErrorCode::InternalError)?);
            }
            if named_in(asked_for, &crate::SOURCE_FORMAT_AND_ORGANIZE_IMPORTS_RUFF) {
                response.push(
                    format_and_organize_imports(&snapshot)
                        .with_failure_code(ErrorCode::InternalError)?,
                );
            }
            if named_in(asked_for, &crate::SOURCE_FORMAT_AND_OPTIMIZE_IMPORTS_RUFF) {
                response.push(
                    format_and_optimize_imports(&snapshot)
                        .with_failure_code(ErrorCode::InternalError)?,
                );
            }
        }

        Ok(Some(response))
    }
}

/// Whether a request asked for `kind` by name.
///
/// `optimizeImports` and `formatAndOptimizeImports` are what an editor runs on save or on commit,
/// not something a reader picks out of a lightbulb menu — that menu already offers *Organize
/// imports* and *Fix all*, and a second, near-identically named entry beside each would be a puzzle
/// rather than a choice. So they are advertised in the server's capabilities and answered when a
/// client names them, but they never appear in a request that just asks for everything.
fn named_in(asked_for: Option<&[CodeActionKind]>, kind: &CodeActionKind) -> bool {
    asked_for.is_some_and(|kinds| {
        kinds
            .iter()
            .any(|requested| kind.as_str().starts_with(requested.as_str()))
    })
}

fn quick_fix(
    snapshot: &DocumentSnapshot,
    fixes: &[DiagnosticFix],
) -> crate::Result<Vec<CodeActionResponse>> {
    let document = snapshot.query();

    fixes
        .iter()
        .filter(|fix| !fix.edits.is_empty())
        .map(|fix| {
            let mut tracker = WorkspaceEditTracker::new(snapshot.resolved_client_capabilities());

            let document_uri = snapshot.query().make_key().into_uri();

            tracker.set_edits_for_document(
                document_uri.clone(),
                document.version(),
                fix.edits.clone(),
            )?;

            Ok(CodeActionResponse::CodeAction(types::CodeAction {
                title: format!("{DIAGNOSTIC_NAME} ({}): {}", fix.code, fix.title),
                kind: Some(types::CodeActionKind::QuickFix),
                edit: Some(tracker.into_workspace_edit()),
                diagnostics: Some(vec![fix.fixed_diagnostic.clone()]),
                data: Some(
                    serde_json::to_value(document_uri).expect("document uri should serialize"),
                ),
                is_preferred: fix.is_preferred,
                ..Default::default()
            }))
        })
        .collect()
}

fn noqa_comments(snapshot: &DocumentSnapshot, fixes: &[DiagnosticFix]) -> Vec<CodeActionResponse> {
    fixes
        .iter()
        .filter_map(|fix| {
            let edit = fix.noqa_edit.clone()?;

            let mut tracker = WorkspaceEditTracker::new(snapshot.resolved_client_capabilities());

            tracker
                .set_edits_for_document(
                    snapshot.query().make_key().into_uri(),
                    snapshot.query().version(),
                    vec![edit],
                )
                .ok()?;

            Some(CodeActionResponse::CodeAction(types::CodeAction {
                title: format!("{DIAGNOSTIC_NAME} ({}): Disable for this line", fix.code),
                kind: Some(types::CodeActionKind::QuickFix),
                edit: Some(tracker.into_workspace_edit()),
                diagnostics: Some(vec![fix.fixed_diagnostic.clone()]),
                data: Some(
                    serde_json::to_value(snapshot.query().make_key().into_uri())
                        .expect("document uri should serialize"),
                ),
                ..Default::default()
            }))
        })
        .collect()
}

fn fix_all(snapshot: &DocumentSnapshot) -> crate::Result<CodeActionResponse> {
    let document = snapshot.query();

    let (edit, data) = if snapshot
        .resolved_client_capabilities()
        .code_action_deferred_edit_resolution
    {
        // The editor will request the edit in a `CodeActionsResolve` request
        (
            None,
            Some(
                serde_json::to_value(snapshot.query().make_key().into_uri())
                    .expect("document uri should serialize"),
            ),
        )
    } else {
        (
            Some(resolve_edit_for_fix_all(
                document,
                snapshot.resolved_client_capabilities(),
                snapshot.encoding(),
            )?),
            None,
        )
    };

    Ok(CodeActionResponse::CodeAction(types::CodeAction {
        title: format!("{DIAGNOSTIC_NAME}: Fix all auto-fixable problems"),
        kind: Some(crate::SOURCE_FIX_ALL_RUFF),
        edit,
        data,
        ..Default::default()
    }))
}

fn notebook_fix_all(snapshot: &DocumentSnapshot) -> crate::Result<CodeActionResponse> {
    let document = snapshot.query();

    let (edit, data) = if snapshot
        .resolved_client_capabilities()
        .code_action_deferred_edit_resolution
    {
        // The editor will request the edit in a `CodeActionsResolve` request
        (
            None,
            Some(
                serde_json::to_value(snapshot.query().make_key().into_uri())
                    .expect("document uri should serialize"),
            ),
        )
    } else {
        (
            Some(resolve_edit_for_fix_all(
                document,
                snapshot.resolved_client_capabilities(),
                snapshot.encoding(),
            )?),
            None,
        )
    };

    Ok(CodeActionResponse::CodeAction(types::CodeAction {
        title: format!("{DIAGNOSTIC_NAME}: Fix all auto-fixable problems"),
        kind: Some(crate::NOTEBOOK_SOURCE_FIX_ALL_RUFF),
        edit,
        data,
        ..Default::default()
    }))
}

fn organize_imports(snapshot: &DocumentSnapshot) -> crate::Result<CodeActionResponse> {
    let document = snapshot.query();

    let (edit, data) = if snapshot
        .resolved_client_capabilities()
        .code_action_deferred_edit_resolution
    {
        // The edit will be resolved later in the `CodeActionsResolve` request
        (
            None,
            Some(
                serde_json::to_value(snapshot.query().make_key().into_uri())
                    .expect("document uri should serialize"),
            ),
        )
    } else {
        (
            Some(resolve_edit_for_organize_imports(
                document,
                snapshot.resolved_client_capabilities(),
                snapshot.encoding(),
            )?),
            None,
        )
    };

    Ok(CodeActionResponse::CodeAction(types::CodeAction {
        title: format!("{DIAGNOSTIC_NAME}: Organize imports"),
        kind: Some(crate::SOURCE_ORGANIZE_IMPORTS_RUFF),
        edit,
        data,
        ..Default::default()
    }))
}

fn notebook_organize_imports(snapshot: &DocumentSnapshot) -> crate::Result<CodeActionResponse> {
    let document = snapshot.query();

    let (edit, data) = if snapshot
        .resolved_client_capabilities()
        .code_action_deferred_edit_resolution
    {
        // The edit will be resolved later in the `CodeActionsResolve` request
        (
            None,
            Some(
                serde_json::to_value(snapshot.query().make_key().into_uri())
                    .expect("document uri should serialize"),
            ),
        )
    } else {
        (
            Some(resolve_edit_for_organize_imports(
                document,
                snapshot.resolved_client_capabilities(),
                snapshot.encoding(),
            )?),
            None,
        )
    };

    Ok(CodeActionResponse::CodeAction(types::CodeAction {
        title: format!("{DIAGNOSTIC_NAME}: Organize imports"),
        kind: Some(crate::NOTEBOOK_SOURCE_ORGANIZE_IMPORTS_RUFF),
        edit,
        data,
        ..Default::default()
    }))
}

fn optimize_imports(snapshot: &DocumentSnapshot) -> crate::Result<CodeActionResponse> {
    let (edit, data) = deferred_or_resolved(snapshot, |snapshot| {
        resolve_edit_for_optimize_imports(
            snapshot.query(),
            snapshot.resolved_client_capabilities(),
            snapshot.encoding(),
        )
    })?;

    Ok(CodeActionResponse::CodeAction(types::CodeAction {
        title: format!("{DIAGNOSTIC_NAME}: Optimize imports"),
        kind: Some(crate::SOURCE_OPTIMIZE_IMPORTS_RUFF),
        edit,
        data,
        ..Default::default()
    }))
}

fn format_and_organize_imports(snapshot: &DocumentSnapshot) -> crate::Result<CodeActionResponse> {
    let (edit, data) = deferred_or_resolved(snapshot, |snapshot| {
        resolve_edit_for_format_and_organize_imports(snapshot)
    })?;

    Ok(CodeActionResponse::CodeAction(types::CodeAction {
        title: format!("{DIAGNOSTIC_NAME}: Format document and organize imports"),
        kind: Some(crate::SOURCE_FORMAT_AND_ORGANIZE_IMPORTS_RUFF),
        edit,
        data,
        ..Default::default()
    }))
}

fn format_and_optimize_imports(snapshot: &DocumentSnapshot) -> crate::Result<CodeActionResponse> {
    let (edit, data) = deferred_or_resolved(snapshot, |snapshot| {
        resolve_edit_for_format_and_optimize_imports(snapshot)
    })?;

    Ok(CodeActionResponse::CodeAction(types::CodeAction {
        title: format!("{DIAGNOSTIC_NAME}: Format document and optimize imports"),
        kind: Some(crate::SOURCE_FORMAT_AND_OPTIMIZE_IMPORTS_RUFF),
        edit,
        data,
        ..Default::default()
    }))
}

/// Fills in either the edit or the payload a `codeAction/resolve` request needs to compute it,
/// depending on whether the client defers edit resolution.
fn deferred_or_resolved(
    snapshot: &DocumentSnapshot,
    resolve: impl FnOnce(&DocumentSnapshot) -> crate::Result<types::WorkspaceEdit>,
) -> crate::Result<(Option<types::WorkspaceEdit>, Option<serde_json::Value>)> {
    if snapshot
        .resolved_client_capabilities()
        .code_action_deferred_edit_resolution
    {
        Ok((
            None,
            Some(
                serde_json::to_value(snapshot.query().make_key().into_uri())
                    .expect("document uri should serialize"),
            ),
        ))
    } else {
        Ok((Some(resolve(snapshot)?), None))
    }
}

/// If `action_filter` is `None`, this returns [`SupportedCodeActionKind::all()`]. Otherwise,
/// the list is filtered.
fn supported_code_actions(
    action_filter: Option<Vec<CodeActionKind>>,
) -> FxHashSet<SupportedCodeAction> {
    let Some(action_filter) = action_filter else {
        return SupportedCodeAction::all().collect();
    };

    action_filter
        .into_iter()
        .flat_map(SupportedCodeAction::from_kind)
        .collect()
}
