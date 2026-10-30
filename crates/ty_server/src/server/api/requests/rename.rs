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
                let offers = rename_offers(
                    db,
                    snapshot,
                    &params.new_name,
                    extras,
                    capabilities.supports_workspace_edit_rename_file(),
                );
                if !offers.is_empty() {
                    return Ok(Some(annotated_workspace_edit(changes, offers)));
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

/// basedpython: one rename offered beside a rename, for the user to keep or leave — see
/// [`RenameExtra`] — as both `textDocument/rename` and `by/renameOffers` hand it over
///
/// the identifiers are `override:<n>`, `parameter:<n>` and `module:<n>`, numbered from 0 within
/// each kind. the label says what kind of offer it is, and is shared by every offer of that kind;
/// the description names what it renames
struct RenameOffer {
    id: String,
    label: &'static str,
    description: String,
    /// the text edits it makes, each in the document it is in
    edits: Vec<(Uri, TextEdit)>,
    /// the file it moves, from and to, made after every text edit
    file_move: Option<(Uri, Uri)>,
}

/// the offers that go with a rename to `new_name`, made of `extras`. a module is offered only to
/// a client that `renames_files`
fn rename_offers(
    db: &ProjectDatabase,
    snapshot: &DocumentSnapshot,
    new_name: &str,
    extras: Vec<RenameExtra>,
    renames_files: bool,
) -> Vec<RenameOffer> {
    let mut offers = Vec::new();
    let mut counts = OfferCounts::default();
    let plain = |occurrences: Vec<FileRange>| -> Vec<(FileRange, String)> {
        occurrences
            .into_iter()
            .map(|range| (range, new_name.to_owned()))
            .collect()
    };
    for extra in extras {
        let (prefix, count, label, description, occurrences, file_move) = match extra {
            RenameExtra::Override {
                member,
                occurrences,
            } => (
                "override",
                &mut counts.overrides,
                "Rename overriding methods",
                format!("`{member}`"),
                plain(occurrences),
                None,
            ),
            RenameExtra::Parameter {
                member,
                parameter,
                occurrences,
            } => (
                "parameter",
                &mut counts.parameters,
                "Rename parameters of overriding methods",
                format!("`{parameter}` of `{member}`"),
                plain(occurrences),
                None,
            ),
            RenameExtra::Module { file_move, edits } => {
                if !renames_files {
                    continue;
                }
                let (Ok(old_uri), Ok(new_uri)) = (
                    Uri::from_file_path(file_move.old_path.as_std_path()),
                    Uri::from_file_path(file_move.new_path.as_std_path()),
                ) else {
                    continue;
                };
                (
                    "module",
                    &mut counts.modules,
                    "Rename module",
                    format!(
                        "`{}` to `{}`",
                        file_move.old_path.file_name().unwrap_or_default(),
                        file_move.new_path.file_name().unwrap_or_default()
                    ),
                    edits
                        .into_iter()
                        .map(|edit| {
                            (
                                FileRange::new(edit.file, edit.edit.range()),
                                edit.edit.content().unwrap_or_default().to_owned(),
                            )
                        })
                        .collect(),
                    Some((old_uri, new_uri)),
                )
            }
        };
        let id = format!("{prefix}:{count}");
        *count += 1;
        let edits = occurrences
            .into_iter()
            .filter_map(|(range, new_text)| {
                let location = range
                    .to_lsp_range(db, snapshot.encoding())
                    .and_then(LspRange::into_location)?;
                Some((
                    location.uri,
                    TextEdit {
                        range: location.range,
                        new_text,
                    },
                ))
            })
            .collect();
        offers.push(RenameOffer {
            id,
            label,
            description,
            edits,
            file_move,
        });
    }
    offers
}

/// how many offers of each kind have been made, which numbers the next one
#[derive(Default)]
struct OfferCounts {
    overrides: usize,
    parameters: usize,
    modules: usize,
}

/// one text edit per file, as `documentChanges` holds them, in the order of the files' uris
fn text_document_edits(edits: impl IntoIterator<Item = (Uri, Edit)>) -> Vec<DocumentChange> {
    let mut by_file: BTreeMap<String, (Uri, Vec<Edit>)> = BTreeMap::new();
    for (uri, edit) in edits {
        by_file
            .entry(uri.as_str().to_owned())
            .or_insert_with(|| (uri, Vec::new()))
            .1
            .push(edit);
    }
    by_file
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
        .collect()
}

/// basedpython: the rename `changes` makes, with the `offers` that go with it, as edits the
/// client asks the user to confirm
///
/// each offer is its own change annotation, with `needsConfirmation` set, so that a client can
/// offer it as a checkbox and apply the rename without it. its label is the offer's kind, which is
/// what a client that groups on labels groups by. the rename itself is made of plain text edits,
/// as without offers. a module is moved by a `RenameFile` after every text edit, the edits to the
/// module itself among them
fn annotated_workspace_edit(
    changes: HashMap<Uri, Vec<TextEdit>>,
    offers: Vec<RenameOffer>,
) -> WorkspaceEdit {
    let mut edits: Vec<(Uri, Edit)> = Vec::new();
    #[expect(
        clippy::iter_over_hash_type,
        reason = "`text_document_edits` orders the files, and a file's edits stay in its own order"
    )]
    for (uri, file_edits) in changes {
        edits.extend(
            file_edits
                .into_iter()
                .map(|edit| (uri.clone(), Edit::from(edit))),
        );
    }

    let mut annotations: HashMap<String, ChangeAnnotation> = HashMap::new();
    let mut file_moves = Vec::new();
    for offer in offers {
        edits.extend(offer.edits.into_iter().map(|(uri, text_edit)| {
            (
                uri,
                Edit::AnnotatedTextEdit(AnnotatedTextEdit {
                    annotation_id: offer.id.clone(),
                    text_edit,
                }),
            )
        }));
        if let Some((old_uri, new_uri)) = offer.file_move {
            file_moves.push(DocumentChange::RenameFile(RenameFile {
                old_uri,
                new_uri,
                options: None,
                annotation_id: Some(offer.id.clone()),
            }));
        }
        annotations.insert(
            offer.id,
            ChangeAnnotation {
                label: offer.label.to_owned(),
                needs_confirmation: Some(true),
                description: Some(offer.description),
            },
        );
    }

    let mut document_changes = text_document_edits(edits);
    document_changes.extend(file_moves);
    WorkspaceEdit {
        changes: None,
        document_changes: Some(document_changes),
        change_annotations: Some(annotations),
    }
}

/// `by/renameOffers`: the renames that go with renaming the symbol at a position to a new name,
/// each as an edit of its own, for a client whose workspace edits carry no change annotations
pub(crate) enum RenameOffersRequest {}

impl lsp_types::Request for RenameOffersRequest {
    type Params = RenameOffersParams;
    type Result = Vec<RenameOfferAnswer>;
    const METHOD: lsp_types::LspRequestMethod<'static> =
        lsp_types::LspRequestMethod::Custom("by/renameOffers");
    const MESSAGE_DIRECTION: lsp_types::MessageDirection =
        lsp_types::MessageDirection::ClientToServer;
}

/// the rename, as `textDocument/rename` is asked for it
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RenameOffersParams {
    text_document: TextDocumentIdentifier,
    position: lsp_types::Position,
    new_name: String,
}

/// one offer: what [`RenameOffer`] says, with its edits as a workspace edit of their own
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RenameOfferAnswer {
    id: String,
    label: String,
    description: String,
    edit: WorkspaceEdit,
}

impl From<RenameOffer> for RenameOfferAnswer {
    /// text edits alone go across as `changes`. an offer that also moves a file goes across as
    /// `documentChanges`, a plain text edit per file followed by the `RenameFile`
    fn from(offer: RenameOffer) -> Self {
        let edit = match offer.file_move {
            None => {
                let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
                for (uri, text_edit) in offer.edits {
                    changes.entry(uri).or_default().push(text_edit);
                }
                WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                }
            }
            Some((old_uri, new_uri)) => {
                let mut document_changes = text_document_edits(
                    offer
                        .edits
                        .into_iter()
                        .map(|(uri, text_edit)| (uri, Edit::TextEdit(text_edit))),
                );
                document_changes.push(DocumentChange::RenameFile(RenameFile {
                    old_uri,
                    new_uri,
                    options: None,
                    annotation_id: None,
                }));
                WorkspaceEdit {
                    changes: None,
                    document_changes: Some(document_changes),
                    change_annotations: None,
                }
            }
        };
        Self {
            id: offer.id,
            label: offer.label.to_owned(),
            description: offer.description,
            edit,
        }
    }
}

pub(crate) struct RenameOffersRequestHandler;

impl RequestHandler for RenameOffersRequestHandler {
    type RequestType = RenameOffersRequest;
}

impl BackgroundDocumentRequestHandler for RenameOffersRequestHandler {
    fn document_uri(params: &RenameOffersParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// empty where there is no python symbol to rename, and refused with the reason
    /// `textDocument/rename` gives for a new name it would refuse
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: RenameOffersParams,
    ) -> crate::server::Result<Vec<RenameOfferAnswer>> {
        if snapshot.is_django_template()
            || snapshot
                .workspace_settings()
                .is_language_services_disabled()
        {
            return Ok(Vec::new());
        }
        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(Vec::new());
        };
        let Some(offset) =
            params
                .position
                .to_text_size(db, file, snapshot.document().uri(), snapshot.encoding())
        else {
            return Ok(Vec::new());
        };
        let PreparedRename::Ready { .. } = prepare_rename(db, db.program_file(file), offset) else {
            return Ok(Vec::new());
        };
        if let Some(why) = invalid_new_name(&params.new_name) {
            return Err(Error::new(anyhow!(why), ErrorCode::RequestFailed));
        }
        let extras = rename_extras(db, db.program_file(file), offset, &params.new_name);
        Ok(rename_offers(db, snapshot, &params.new_name, extras, true)
            .into_iter()
            .map(RenameOfferAnswer::from)
            .collect())
    }
}

impl RetriableRequestHandler for RenameOffersRequestHandler {}

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
