use std::borrow::Cow;
use std::time::Instant;

use lsp_types::InlayHintRequest;
use lsp_types::{InlayHintParams, Uri};
use ruff_db::files::File;
use ty_ide::{
    InlayHintKind, InlayHintLabel, InlayHintTextEdit, TemplateInlayHint, TemplateInlayHintKind,
    django_template_inlay_hints, inlay_hints,
};
use ty_project::{ProjectDatabase, SemanticDb as _};
use ty_python_semantic::ProgramEnvironment;

use crate::PositionEncoding;
use crate::document::{RangeExt, TextSizeExt, ToLink};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) struct InlayHintRequestHandler;

impl RequestHandler for InlayHintRequestHandler {
    type RequestType = InlayHintRequest;
}

impl BackgroundDocumentRequestHandler for InlayHintRequestHandler {
    fn document_uri(params: &InlayHintParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: InlayHintParams,
    ) -> crate::server::Result<Option<Vec<lsp_types::InlayHint>>> {
        let start = Instant::now();
        let workspace_settings = snapshot.workspace_settings();
        if workspace_settings.is_language_services_disabled()
            || !workspace_settings.inlay_hints().any_enabled()
        {
            return Ok(None);
        }

        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };

        let Some(range) = params
            .range
            .to_text_range(db, file, snapshot.uri(), snapshot.encoding())
        else {
            return Ok(None);
        };

        if snapshot.is_django_template() {
            let hints = django_template_inlay_hints(
                db,
                &ProgramEnvironment::from_file(db.program_file(file)),
                file,
                range,
                workspace_settings.inlay_hints(),
            )
            .into_iter()
            .filter_map(|hint| template_inlay_hint(hint, db, file, snapshot.encoding()))
            .collect();

            return Ok(Some(hints));
        }

        let inlay_hints = inlay_hints(
            db,
            db.program_file(file),
            range,
            workspace_settings.inlay_hints(),
        );

        let inlay_hints: Vec<lsp_types::InlayHint> = inlay_hints
            .into_iter()
            .filter_map(|hint| {
                Some(lsp_types::InlayHint {
                    position: hint
                        .position
                        .to_lsp_position(db, file, snapshot.encoding())?
                        .local_position(),
                    label: inlay_hint_label(&hint.label, db, snapshot.encoding()),
                    kind: Some(inlay_hint_kind(&hint.kind)),
                    tooltip: None,
                    padding_left: None,
                    padding_right: None,
                    data: None,
                    text_edits: Some(
                        hint.text_edits
                            .into_iter()
                            .filter_map(|text_edit| {
                                inlay_hint_text_edit(text_edit, db, file, snapshot.encoding())
                            })
                            .collect(),
                    ),
                })
            })
            .collect();

        tracing::debug!(
            "Inlay hint request returned {} hints in {:?}",
            inlay_hints.len(),
            start.elapsed()
        );

        Ok(Some(inlay_hints))
    }
}

impl RetriableRequestHandler for InlayHintRequestHandler {}

/// one django template hint, as the client shows it
///
/// a resolved template's path is neither a type nor a parameter, and LSP has no
/// kind that fits it, so it is sent with none — which is what leaves a client's
/// type-specific styling for the hints that really are types.
fn template_inlay_hint(
    hint: TemplateInlayHint,
    db: &ProjectDatabase,
    file: File,
    encoding: PositionEncoding,
) -> Option<lsp_types::InlayHint> {
    Some(lsp_types::InlayHint {
        position: hint
            .position
            .to_lsp_position(db, file, encoding)?
            .local_position(),
        label: lsp_types::Label::String(hint.label),
        kind: match hint.kind {
            TemplateInlayHintKind::Type => Some(lsp_types::InlayHintKind::Type),
            TemplateInlayHintKind::Template => None,
        },
        tooltip: None,
        padding_left: None,
        padding_right: None,
        data: None,
        text_edits: None,
    })
}

fn inlay_hint_kind(inlay_hint_kind: &InlayHintKind) -> lsp_types::InlayHintKind {
    match inlay_hint_kind {
        InlayHintKind::Type
        // basedpython: an inferred exception set is a type, like a return type
        | InlayHintKind::Raises
        | InlayHintKind::Variance
        | InlayHintKind::Reification
        | InlayHintKind::TypeArgument
        | InlayHintKind::Override
        | InlayHintKind::NumericPromotion
        | InlayHintKind::RevealedType => lsp_types::InlayHintKind::Type,
        InlayHintKind::CallArgumentName
        | InlayHintKind::ImplicitParameter
        | InlayHintKind::ImplicitArgument => lsp_types::InlayHintKind::Parameter,
    }
}

fn inlay_hint_label(
    inlay_hint_label: &InlayHintLabel,
    db: &ProjectDatabase,
    encoding: PositionEncoding,
) -> lsp_types::Label {
    let mut label_parts = Vec::new();
    for part in inlay_hint_label.parts() {
        label_parts.push(lsp_types::InlayHintLabelPart {
            value: part.text().into(),
            location: part
                .target()
                .and_then(|target| target.to_location(db, encoding)),
            tooltip: None,
            command: None,
        });
    }
    lsp_types::Label::InlayHintLabelPartList(label_parts)
}

fn inlay_hint_text_edit(
    inlay_hint_text_edit: InlayHintTextEdit,
    db: &ProjectDatabase,
    file: File,
    encoding: PositionEncoding,
) -> Option<lsp_types::TextEdit> {
    Some(lsp_types::TextEdit {
        range: lsp_types::Range {
            start: inlay_hint_text_edit
                .range
                .start()
                .to_lsp_position(db, file, encoding)?
                .local_position(),
            end: inlay_hint_text_edit
                .range
                .end()
                .to_lsp_position(db, file, encoding)?
                .local_position(),
        },
        new_text: inlay_hint_text_edit.new_text,
    })
}
