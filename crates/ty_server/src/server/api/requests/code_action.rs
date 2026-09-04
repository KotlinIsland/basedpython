use std::borrow::Cow;
use std::collections::HashMap;

use lsp_types::{self as types, Code, CodeActionRequest, CodeActionResponse, TextEdit, Uri};
use ruff_text_size::Ranged;
use ty_ide::{FileEdit, code_actions};
use ty_project::{ProjectDatabase, SemanticDb as _};
use types::CodeActionKind;

use crate::db::Db;
use crate::document::{RangeExt, ToRangeExt};
use crate::server::Result;
use crate::server::api::RequestHandler;
use crate::server::api::diagnostics::DiagnosticData;
use crate::server::api::requests::execute_command::add_dependency_command;

use crate::server::api::traits::{BackgroundDocumentRequestHandler, RetriableRequestHandler};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;
use crate::{DIAGNOSTIC_NAME, PositionEncoding};

pub(crate) struct CodeActionRequestHandler;

impl RequestHandler for CodeActionRequestHandler {
    type RequestType = CodeActionRequest;
}

impl BackgroundDocumentRequestHandler for CodeActionRequestHandler {
    fn document_uri(params: &types::CodeActionParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: types::CodeActionParams,
    ) -> Result<Option<Vec<CodeActionResponse>>> {
        let diagnostics = params.context.diagnostics;

        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };
        let program_file = db.program_file(file);
        let mut actions = Vec::new();

        for mut diagnostic in diagnostics.into_iter().filter(|diagnostic| {
            diagnostic.source.as_deref() == Some(DIAGNOSTIC_NAME)
                && range_intersect(&diagnostic.range, &params.range)
        }) {
            let mut diagnostic_id = match &diagnostic.code {
                Some(Code::String(diagnostic_id)) => Some(Cow::Borrowed(diagnostic_id)),
                _ => None,
            };

            // If the diagnostic includes fixes, offer those up as options.
            if let Some(data) = diagnostic.data.take() {
                let data: DiagnosticData = match serde_json::from_value(data) {
                    Ok(data) => data,
                    Err(err) => {
                        tracing::warn!("Failed to deserialize diagnostic data: {err}");
                        continue;
                    }
                };

                let fix = match data {
                    DiagnosticData::Full(full_diagnostic) => {
                        diagnostic_id = Some(Cow::Owned(full_diagnostic.diagnostic_id));
                        full_diagnostic.fix
                    }
                    DiagnosticData::Fix(fix) => Some(fix),
                };

                if let Some(fix) = fix {
                    actions.push(CodeActionResponse::CodeAction(lsp_types::CodeAction {
                        title: fix.fix_title,
                        kind: Some(CodeActionKind::QuickFix),
                        diagnostics: Some(vec![diagnostic.clone()]),
                        edit: Some(lsp_types::WorkspaceEdit {
                            changes: Some(fix.edits),
                            document_changes: None,
                            change_annotations: None,
                        }),
                        is_preferred: Some(fix.preferred),
                        command: None,
                        disabled: None,
                        data: None,
                        tags: None,
                    }));
                }
            }

            // Try to find other applicable actions.
            //
            // This is only for actions that are messy to compute at the time of the diagnostic.
            // For instance, suggesting imports requires finding symbols for the entire project,
            // which is dubious when you're in the middle of resolving symbols.
            let uri = snapshot.uri();
            let encoding = snapshot.encoding();
            if let Some(diagnostic_id) = diagnostic_id
                && let Some(range) = diagnostic.range.to_text_range(db, file, uri, encoding)
            {
                for action in code_actions(
                    db,
                    program_file,
                    range,
                    &diagnostic_id,
                    snapshot.is_django_template(),
                ) {
                    // an action that creates a file cannot be written as a text
                    // edit, so it goes through the resource-operation form
                    let document_changes = action.create.as_ref().and_then(|path| {
                        let uri = Uri::from_file_path(path.as_std_path()).ok()?;
                        Some(vec![lsp_types::DocumentChange::CreateFile(
                            lsp_types::CreateFile {
                                uri,
                                options: None,
                                annotation_id: None,
                            },
                        )])
                    });

                    // an action that installs something reaches past the files
                    // the editor can edit, so it asks the server to run the
                    // command instead. the client runs a command after the edit
                    // of the same action, so an action may carry both
                    let command = action
                        .add_dependency
                        .as_ref()
                        .map(|add| add_dependency_command(&action.title, add));

                    let changes = document_changes
                        .is_none()
                        .then(|| to_lsp_edits(db, encoding, action.edits))
                        .flatten();

                    // an action that only runs a command changes no file itself,
                    // and an empty edit is not something to hand a client
                    let edit = (changes.is_some() || document_changes.is_some()).then_some(
                        lsp_types::WorkspaceEdit {
                            changes,
                            document_changes,
                            change_annotations: None,
                        },
                    );

                    actions.push(CodeActionResponse::CodeAction(lsp_types::CodeAction {
                        title: action.title.clone(),
                        kind: Some(CodeActionKind::QuickFix),
                        diagnostics: Some(vec![diagnostic.clone()]),
                        edit,
                        is_preferred: Some(action.preferred),
                        command,
                        disabled: None,
                        data: None,
                        tags: None,
                    }));
                }
            }
        }

        if actions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(actions))
        }
    }
}

/// The edits of an action, in the files they belong to, or `None` if it has none.
///
/// An action can change a file other than the one its diagnostic is in — adding a
/// missing dependency edits `pyproject.toml` — so each edit carries its own file
/// rather than being resolved against the diagnostic's.
fn to_lsp_edits(
    db: &dyn Db,
    encoding: PositionEncoding,
    edits: Vec<FileEdit>,
) -> Option<HashMap<Uri, Vec<TextEdit>>> {
    let mut lsp_edits: HashMap<Uri, Vec<lsp_types::TextEdit>> = HashMap::new();

    for FileEdit { file, edit } in edits {
        let location = edit
            .range()
            .to_lsp_range(db, file, encoding)?
            .to_location()?;

        lsp_edits
            .entry(location.uri)
            .or_default()
            .push(lsp_types::TextEdit {
                range: location.range,
                new_text: edit.content().unwrap_or_default().to_string(),
            });
    }

    (!lsp_edits.is_empty()).then_some(lsp_edits)
}

fn range_intersect(range: &lsp_types::Range, other: &lsp_types::Range) -> bool {
    let start = range.start.max(other.start);
    let end = range.end.min(other.end);
    end >= start
}

impl RetriableRequestHandler for CodeActionRequestHandler {}
