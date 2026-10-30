use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use anyhow::anyhow;
use lsp_server::ErrorCode;
use lsp_types::RenameRequest;
use lsp_types::{
    AnnotatedTextEdit, ChangeAnnotation, DocumentChange, Edit,
    OptionalVersionedTextDocumentIdentifier, RenameFile, RenameParams, TextDocumentEdit,
    TextDocumentIdentifier, TextEdit, Uri, WorkspaceEdit,
};
use ruff_db::files::FileRange;
use ruff_text_size::Ranged;
use ty_ide::{
    PreparedRename, RenameExtra, TemplateRename, TemplateRenameOutcome, django_rename,
    invalid_new_name, prepare_rename, rename, rename_extras,
};
use ty_project::{ProjectDatabase, SemanticDb as _};

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

        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(None);
        };

        let Some(offset) = params.text_document_position_params.position.to_text_size(
            db,
            file,
            snapshot.document().uri(),
            snapshot.encoding(),
        ) else {
            return Ok(None);
        };

        let template = snapshot.is_django_template();

        // a rename is checked as `textDocument/prepareRename` checks it, since a
        // client need not ask that first. the django names a module writes as
        // plain strings are what is left over once the python symbol is not one
        let python = (!template).then(|| prepare_rename(db, db.program_file(file), offset));
        // only a python symbol's new name must be an identifier: a django rename
        // may be naming a template file
        if let Some(PreparedRename::Ready { .. }) = python
            && let Some(why) = invalid_new_name(&params.new_name)
        {
            return Err(Error::new(anyhow!(why), ErrorCode::RequestFailed));
        }
        if let Some(PreparedRename::Ready { .. }) = python
            && let Some(rename_results) =
                rename(db, db.program_file(file), offset, &params.new_name)
        {
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

            let capabilities = snapshot.resolved_client_capabilities();
            if capabilities.supports_workspace_edit_change_annotations() {
                let extras = rename_extras(db, db.program_file(file), offset, &params.new_name);
                if !extras.is_empty() {
                    return Ok(Some(annotated_workspace_edit(
                        db,
                        snapshot,
                        &params.new_name,
                        changes,
                        extras,
                        capabilities.supports_workspace_edit_rename_file(),
                    )));
                }
            }

            return Ok(Some(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            }));
        }

        match django_rename(db, file, offset, &params.new_name, template) {
            None => match python {
                Some(PreparedRename::Refused(why)) => {
                    Err(Error::new(anyhow!(why), ErrorCode::RequestFailed))
                }
                Some(PreparedRename::Ready { .. } | PreparedRename::NoSymbol) | None => Ok(None),
            },
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

/// basedpython: the rename `changes` makes, with the renames that go with it — see
/// [`RenameExtra`] — as edits the client asks the user to confirm
///
/// Each extra is its own change annotation, with `needsConfirmation` set, so that a client can
/// offer it as a checkbox and apply the rename without it. Its label says what kind of extra it is
/// and is shared by every extra of that kind, which is what a client that groups on labels groups
/// by; its description names what it renames. The identifiers are `override:<n>`,
/// `parameter:<n>` and `module:<n>`, numbered from 0 within each kind. The rename itself is made
/// of plain text edits, as without extras. A module is moved by a `RenameFile` after every text
/// edit, the edits to the module itself among them, and only for a client that says it can.
fn annotated_workspace_edit(
    db: &ProjectDatabase,
    snapshot: &DocumentSnapshot,
    new_name: &str,
    changes: HashMap<Uri, Vec<TextEdit>>,
    extras: Vec<RenameExtra>,
    renames_files: bool,
) -> WorkspaceEdit {
    let mut edits: BTreeMap<String, (Uri, Vec<Edit>)> = BTreeMap::new();
    #[expect(
        clippy::iter_over_hash_type,
        reason = "each file's edits go into an ordered map, which sets the order they are sent in"
    )]
    for (uri, file_edits) in changes {
        edits
            .entry(uri.as_str().to_owned())
            .or_insert_with(|| (uri, Vec::new()))
            .1
            .extend(file_edits.into_iter().map(Edit::from));
    }

    let mut annotations: HashMap<String, ChangeAnnotation> = HashMap::new();
    let mut file_moves = Vec::new();
    let mut counts = [0usize; 3];
    for extra in extras {
        let (kind, label, description, occurrences): (
            usize,
            &str,
            String,
            Vec<(FileRange, String)>,
        ) = match extra {
            RenameExtra::Override {
                member,
                occurrences,
            } => (
                0,
                "Rename overriding methods",
                format!("`{member}`"),
                occurrences
                    .into_iter()
                    .map(|range| (range, new_name.to_owned()))
                    .collect(),
            ),
            RenameExtra::Parameter {
                member,
                parameter,
                occurrences,
            } => (
                1,
                "Rename parameters of overriding methods",
                format!("`{parameter}` of `{member}`"),
                occurrences
                    .into_iter()
                    .map(|range| (range, new_name.to_owned()))
                    .collect(),
            ),
            RenameExtra::Module { file_move, edits } => {
                if !renames_files {
                    continue;
                }
                let description = format!(
                    "`{}` to `{}`",
                    file_move.old_path.file_name().unwrap_or_default(),
                    file_move.new_path.file_name().unwrap_or_default()
                );
                file_moves.push((counts[2], file_move));
                (
                    2,
                    "Rename module",
                    description,
                    edits
                        .into_iter()
                        .map(|edit| {
                            (
                                FileRange::new(edit.file, edit.edit.range()),
                                edit.edit.content().unwrap_or_default().to_owned(),
                            )
                        })
                        .collect(),
                )
            }
        };
        let id = format!(
            "{}:{}",
            ["override", "parameter", "module"][kind],
            counts[kind]
        );
        counts[kind] += 1;
        for (range, new_text) in occurrences {
            let Some(location) = range
                .to_lsp_range(db, snapshot.encoding())
                .and_then(LspRange::into_location)
            else {
                continue;
            };
            edits
                .entry(location.uri.as_str().to_owned())
                .or_insert_with(|| (location.uri.clone(), Vec::new()))
                .1
                .push(Edit::AnnotatedTextEdit(AnnotatedTextEdit {
                    annotation_id: id.clone(),
                    text_edit: TextEdit {
                        range: location.range,
                        new_text,
                    },
                }));
        }
        annotations.insert(
            id,
            ChangeAnnotation {
                label: label.to_owned(),
                needs_confirmation: Some(true),
                description: Some(description),
            },
        );
    }

    let mut document_changes: Vec<DocumentChange> = edits
        .into_values()
        .map(|(uri, edits)| {
            TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    version: None,
                    text_document_identifier: TextDocumentIdentifier { uri },
                },
                edits,
            }
            .into()
        })
        .collect();
    for (index, file_move) in file_moves {
        let (Ok(old_uri), Ok(new_uri)) = (
            Uri::from_file_path(file_move.old_path.as_std_path()),
            Uri::from_file_path(file_move.new_path.as_std_path()),
        ) else {
            continue;
        };
        document_changes.push(DocumentChange::RenameFile(RenameFile {
            old_uri,
            new_uri,
            options: None,
            annotation_id: Some(format!("module:{index}")),
        }));
    }

    WorkspaceEdit {
        changes: None,
        document_changes: Some(document_changes),
        change_annotations: Some(annotations),
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
