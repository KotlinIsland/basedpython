use std::borrow::Cow;
use std::collections::HashMap;

use anyhow::anyhow;
use lsp_server::ErrorCode;
use lsp_types::RenameRequest;
use lsp_types::{RenameParams, TextEdit, Uri, WorkspaceEdit};
use ty_ide::{TemplateRename, TemplateRenameOutcome, django_rename, rename};
use ty_project::ProjectDatabase;

use crate::document::{FileRangeExt, LspRange, PositionExt, ToLink};
use crate::server::api::Error;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) struct RenameRequestHandler;

impl RequestHandler for RenameRequestHandler {
    type RequestType = RenameRequest;
}

impl BackgroundDocumentRequestHandler for RenameRequestHandler {
    fn document_uri(params: &RenameParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document_position_params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: RenameParams,
    ) -> crate::server::Result<Option<WorkspaceEdit>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }

        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };

        let Some(offset) = params.text_document_position_params.position.to_text_size(
            db,
            file,
            snapshot.uri(),
            snapshot.encoding(),
        ) else {
            return Ok(None);
        };

        let template = snapshot.is_django_template();

        // a python symbol is renamed exactly as it was; the django names a module
        // writes as plain strings are what is left over once it declines
        if !template && let Some(rename_results) = rename(db, file, offset, &params.new_name) {
            // Group text edits by file
            let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();

            for reference in rename_results {
                if let Some(location) = reference.to_location(db, snapshot.encoding()) {
                    let edit = TextEdit {
                        range: location.range,
                        new_text: params.new_name.clone(),
                    };

                    changes.entry(location.uri).or_default().push(edit);
                }
            }

            if changes.is_empty() {
                return Ok(None);
            }

            return Ok(Some(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            }));
        }

        match django_rename(db, file, offset, &params.new_name, template) {
            None => Ok(None),
            Some(TemplateRenameOutcome::Refused(why)) => {
                Err(Error::new(anyhow!(why), ErrorCode::RequestFailed))
            }
            Some(TemplateRenameOutcome::Edits(renamed)) => Ok(django_workspace_edit(
                db,
                snapshot,
                &params.new_name,
                renamed,
            )),
        }
    }
}

/// the edits a django rename makes, as the client applies them
///
/// a rename that moves a file cannot be written as text edits alone, so it goes
/// through the ordered `document_changes` form — with the edits first, since the
/// file being moved may be one of the files being edited.
fn django_workspace_edit(
    db: &ProjectDatabase,
    snapshot: &DocumentSnapshot,
    new_name: &str,
    renamed: TemplateRename,
) -> Option<WorkspaceEdit> {
    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();

    for edit in renamed.edits {
        let location = edit
            .to_lsp_range(db, snapshot.encoding())
            .and_then(LspRange::into_location)?;

        changes
            .entry(location.uri)
            .or_default()
            .push(lsp_types::TextEdit {
                range: location.range,
                new_text: new_name.to_string(),
            });
    }

    let Some((from, to)) = renamed.file_rename else {
        return (!changes.is_empty()).then_some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        });
    };

    let mut by_file: Vec<(Uri, Vec<TextEdit>)> = changes.into_iter().collect();
    by_file.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));

    let mut document_changes: Vec<lsp_types::DocumentChange> = by_file
        .into_iter()
        .map(|(uri, edits)| {
            lsp_types::TextDocumentEdit {
                text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                    version: None,
                    text_document_identifier: lsp_types::TextDocumentIdentifier { uri },
                },
                edits: edits.into_iter().map(Into::into).collect(),
            }
            .into()
        })
        .collect();

    document_changes.push(lsp_types::DocumentChange::RenameFile(
        lsp_types::RenameFile {
            old_uri: Uri::from_file_path(from.as_std_path()).ok()?,
            new_uri: Uri::from_file_path(to.as_std_path()).ok()?,
            options: None,
            annotation_id: None,
        },
    ));

    Some(WorkspaceEdit {
        changes: None,
        document_changes: Some(document_changes),
        change_annotations: None,
    })
}

impl RetriableRequestHandler for RenameRequestHandler {}
