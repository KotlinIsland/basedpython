use std::borrow::Cow;
use std::str::FromStr;

use lsp_types::{self as types, CodeActionResolveRequest, Uri};
use ruff_source_file::LineIndex;
use ruff_text_size::Ranged;
use ty_ide::RefactorKind;
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{RangeExt, text_range_to_lsp_range};
use crate::server::api::RequestHandler;
use crate::server::api::requests::code_action::{RefactorData, to_lsp_edits};
use crate::server::api::traits::{BackgroundDocumentRequestHandler, RetriableRequestHandler};
use crate::server::{Error, Result};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

/// Computes the edit of a refactoring offered by `textDocument/codeAction`.
///
/// Only the refactorings are resolved: every other action already carries its edit.
pub(crate) struct CodeActionResolveRequestHandler;

impl RequestHandler for CodeActionResolveRequestHandler {
    type RequestType = CodeActionResolveRequest;
}

impl BackgroundDocumentRequestHandler for CodeActionResolveRequestHandler {
    fn document_uri(params: &types::CodeAction) -> Cow<'_, Uri> {
        // an action this server did not offer names no document; one that is never
        // open is answered as not open
        Cow::Owned(refactor_data(params).map_or_else(
            || Uri::from_str("untitled:unresolvable-code-action").expect("a valid URI"),
            |data| data.uri,
        ))
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        mut action: types::CodeAction,
    ) -> Result<types::CodeAction> {
        let Some(data) = refactor_data(&action) else {
            return Err(invalid_params(
                "the code action carries no refactoring to resolve",
            ));
        };
        let Some(kind) = RefactorKind::from_id(&data.refactor) else {
            return Err(invalid_params(&format!(
                "unknown refactoring `{}`",
                data.refactor
            )));
        };
        if snapshot.document().version() != data.version {
            return Err(Error {
                code: lsp_server::ErrorCode::ContentModified,
                error: anyhow::anyhow!("the document changed since the refactoring was offered"),
            });
        }
        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Err(invalid_params(
                "the document is not a file the server knows",
            ));
        };
        let Some(range) =
            data.range
                .to_text_range(db, file, snapshot.document().uri(), snapshot.encoding())
        else {
            return Err(invalid_params(
                "the refactoring's range is not in the document",
            ));
        };

        match ty_ide::refactor(db, db.program_file(file), kind, range) {
            Ok(refactor) => {
                if let Some(name) = refactor.name {
                    // the name is in the text the edits leave behind, so it is a range in that text
                    let source = ruff_db::source::source_text(db, file);
                    let mut edited = source.as_str().to_string();
                    let mut applied: Vec<_> = refactor
                        .edits
                        .iter()
                        .filter(|edit| edit.file == file)
                        .map(|edit| &edit.edit)
                        .collect();
                    applied.sort_by_key(|edit| std::cmp::Reverse(edit.start()));
                    for edit in applied {
                        edited.replace_range(
                            edit.range().to_std_range(),
                            edit.content().unwrap_or_default(),
                        );
                    }
                    let range = text_range_to_lsp_range(
                        name,
                        &edited,
                        &LineIndex::from_source_text(&edited),
                        snapshot.encoding(),
                    );
                    let data = RefactorData {
                        rename: Some(types::Location {
                            uri: snapshot.document().uri().clone(),
                            range,
                        }),
                        ..data
                    };
                    action.data = serde_json::to_value(data).ok();
                }
                action.edit = Some(types::WorkspaceEdit {
                    changes: to_lsp_edits(db, snapshot.encoding(), refactor.edits),
                    ..types::WorkspaceEdit::default()
                });
            }
            // the document is the version the action was offered for, so this is a
            // refactoring the offer already refused; say why rather than fail
            Err(reason) => action.disabled = Some(types::CodeActionDisabled { reason }),
        }
        Ok(action)
    }
}

impl RetriableRequestHandler for CodeActionResolveRequestHandler {}

fn refactor_data(action: &types::CodeAction) -> Option<RefactorData> {
    serde_json::from_value(action.data.clone()?).ok()
}

fn invalid_params(message: &str) -> Error {
    Error {
        code: lsp_server::ErrorCode::InvalidParams,
        error: anyhow::anyhow!("{message}"),
    }
}
