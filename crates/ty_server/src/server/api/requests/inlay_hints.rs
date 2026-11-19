use std::borrow::Cow;
use std::time::Instant;

use lsp_types::InlayHintRequest;
use lsp_types::{InlayHintParams, Uri};
use ruff_db::files::File;
use ty_ide::{
    BlockPlacement, InlayHint, InlayHintKind, InlayHintLabel, InlayHintTextEdit, TemplateInlayHint,
    TemplateInlayHintKind, django_template_inlay_hints, inlay_hints,
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

        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(None);
        };

        let Some(range) =
            params
                .range
                .to_text_range(db, file, snapshot.document().uri(), snapshot.encoding())
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
            .filter_map(|hint| lsp_inlay_hint(hint, db, file, snapshot.encoding()))
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

/// one hint of a python or basedpython file, as the client shows it. `None` for a hint at an
/// offset the file does not have
///
/// also what `by/inlayHintCatalog` draws its examples with, so an example's hints are in the
/// shape a file's are
pub(super) fn lsp_inlay_hint(
    hint: InlayHint,
    db: &ProjectDatabase,
    file: File,
    encoding: PositionEncoding,
) -> Option<lsp_types::InlayHint> {
    Some(lsp_types::InlayHint {
        position: hint
            .position
            .to_lsp_position(db, file, encoding)?
            .local_position(),
        label: inlay_hint_label(&hint.label, db, encoding),
        kind: Some(inlay_hint_kind(hint.kind)),
        tooltip: hint
            .tooltip
            .map(|value| lsp_types::Tooltip::MarkupContent(markdown(value))),
        padding_left: hint.padding_left.then_some(true),
        padding_right: hint.padding_right.then_some(true),
        data: Some(match hint.placement {
            Some(placement) => block_tag(hint.kind.setting(), placement),
            None => kind_tag(hint.kind.setting()),
        }),
        text_edits: Some(
            hint.text_edits
                .into_iter()
                .filter_map(|text_edit| inlay_hint_text_edit(text_edit, db, file, encoding))
                .collect(),
        ),
    })
}

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
        padding_left: hint.padding_left.then_some(true),
        padding_right: None,
        data: Some(kind_tag(hint.kind.setting())),
        text_edits: None,
    })
}

/// which kind of hint this is, named by the `inlayHints` option that switches
/// it, as `data.kind`
///
/// LSP's own `kind` has two values for the two dozen things these hints are,
/// and a client that lets its user treat each kind differently has nothing else
/// to go on but the label, which says what a hint shows rather than what it is.
/// `data` is otherwise only carried back on `inlayHint/resolve`, which this
/// server does not offer
fn kind_tag(setting: &str) -> serde_json::Value {
    serde_json::json!({ "kind": setting })
}

/// the tag of a hint drawn as whole lines of its own rather than inline: its kind, and
/// `placement`, `"above"` or `"below"`, the side of the line holding its `position` its lines go
/// on. its label is those lines, separated by `\n`, each with all the indentation it is drawn with.
/// LSP has no such hint, so a client that does not read the tag draws the lines inline, at the
/// position
fn block_tag(setting: &str, placement: BlockPlacement) -> serde_json::Value {
    serde_json::json!({ "kind": setting, "placement": placement.as_str() })
}

fn inlay_hint_kind(inlay_hint_kind: InlayHintKind) -> lsp_types::InlayHintKind {
    if inlay_hint_kind.is_parameter() {
        lsp_types::InlayHintKind::Parameter
    } else {
        lsp_types::InlayHintKind::Type
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
            tooltip: part
                .tooltip()
                .map(|value| lsp_types::Tooltip::MarkupContent(markdown(value.to_string()))),
            command: None,
        });
    }
    lsp_types::Label::InlayHintLabelPartList(label_parts)
}

/// a tooltip, which `ty_ide` writes in markdown: a type or a declaration in it is code
fn markdown(value: String) -> lsp_types::MarkupContent {
    lsp_types::MarkupContent {
        kind: lsp_types::MarkupKind::Markdown,
        value,
    }
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
