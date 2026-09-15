use std::borrow::Cow;
use std::collections::HashMap;

use lsp_types::{self as types, Code, CodeActionRequest, CodeActionResponse, TextEdit, Uri};
use ruff_text_size::Ranged;
use ty_ide::{FileEdit, RefactorOffer, code_actions, refactors};
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
        let only = params.context.only;
        let invoked = params.context.trigger_kind == Some(types::CodeActionTriggerKind::Invoked);

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

        if !snapshot.is_django_template()
            && let Some(range) =
                params
                    .range
                    .to_text_range(db, file, snapshot.uri(), snapshot.encoding())
        {
            for offer in refactors(db, program_file, range) {
                let kind = CodeActionKind::new(offer.kind.code_action_kind());
                if !is_requested(only.as_deref(), &kind) {
                    continue;
                }
                let action = match offer.availability {
                    Ok(()) => {
                        refactor_action(db, snapshot, program_file, &offer, params.range, range)
                    }
                    // a refactoring that cannot be applied is only worth showing to a
                    // user who asked for refactorings, who is owed the reason
                    Err(reason) if invoked => Some(lsp_types::CodeAction {
                        title: offer.title,
                        kind: Some(kind),
                        disabled: Some(types::CodeActionDisabled { reason }),
                        ..lsp_types::CodeAction::default()
                    }),
                    Err(_) => None,
                };
                actions.extend(action.map(CodeActionResponse::CodeAction));
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
pub(crate) fn to_lsp_edits(
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

/// The code action for a refactoring that applies. A client that can resolve an
/// action's edit gets it on request, carrying what is needed to compute it again;
/// any other client gets the edit up front.
fn refactor_action(
    db: &ProjectDatabase,
    snapshot: &DocumentSnapshot,
    program_file: ty_python_core::ProgramFile<'_>,
    offer: &RefactorOffer,
    lsp_range: types::Range,
    range: ruff_text_size::TextRange,
) -> Option<lsp_types::CodeAction> {
    let kind = CodeActionKind::new(offer.kind.code_action_kind());
    if snapshot
        .resolved_client_capabilities()
        .supports_code_action_edit_resolve()
    {
        let data = RefactorData {
            uri: snapshot.uri().clone(),
            version: snapshot.document().version(),
            refactor: offer.kind.id().to_string(),
            range: lsp_range,
        };
        return Some(lsp_types::CodeAction {
            title: offer.title.clone(),
            kind: Some(kind),
            data: serde_json::to_value(data).ok(),
            ..lsp_types::CodeAction::default()
        });
    }
    let refactor = ty_ide::refactor(db, program_file, offer.kind, range).ok()?;
    Some(lsp_types::CodeAction {
        title: refactor.title,
        kind: Some(kind),
        edit: Some(lsp_types::WorkspaceEdit {
            changes: to_lsp_edits(db, snapshot.encoding(), refactor.edits),
            ..lsp_types::WorkspaceEdit::default()
        }),
        ..lsp_types::CodeAction::default()
    })
}

/// What a refactoring's code action carries for `codeAction/resolve` to compute
/// its edit: the refactoring, and the document and range it was offered for.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct RefactorData {
    pub(crate) uri: Uri,
    /// The document version the refactoring was offered against. An edit
    /// computed against any other version would land on the wrong text.
    pub(crate) version: crate::document::DocumentVersion,
    pub(crate) refactor: String,
    pub(crate) range: types::Range,
}

/// Whether a client that asked only for `only` wants actions of `kind`: a
/// requested kind covers itself and every kind nested under it.
fn is_requested(only: Option<&[CodeActionKind]>, kind: &CodeActionKind) -> bool {
    let Some(only) = only else {
        return true;
    };
    let kind = kind.to_string();
    only.iter().any(|requested| {
        let requested = requested.to_string();
        kind == requested || kind.starts_with(&format!("{requested}."))
    })
}

fn range_intersect(range: &lsp_types::Range, other: &lsp_types::Range) -> bool {
    let start = range.start.max(other.start);
    let end = range.end.min(other.end);
    end >= start
}

impl RetriableRequestHandler for CodeActionRequestHandler {}
