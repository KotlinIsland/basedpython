use std::borrow::Cow;

use crate::document::{FileRangeExt, PositionExt};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;
use lsp_types::HoverRequest;
use lsp_types::{HoverParams, MarkupContent, Uri};
use ty_ide::{MarkupKind, django_template_hover, hover};
use ty_project::ProjectDatabase;

pub(crate) struct HoverRequestHandler;

impl RequestHandler for HoverRequestHandler {
    type RequestType = HoverRequest;
}

impl BackgroundDocumentRequestHandler for HoverRequestHandler {
    fn document_uri(params: &HoverParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document_position_params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: HoverParams,
    ) -> crate::server::Result<Option<lsp_types::Hover>> {
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

        let (markup_kind, lsp_markup_kind) = if snapshot
            .resolved_client_capabilities()
            .prefers_markdown_in_hover()
        {
            (MarkupKind::Markdown, lsp_types::MarkupKind::Markdown)
        } else {
            (MarkupKind::PlainText, lsp_types::MarkupKind::PlainText)
        };

        let hovered = if snapshot.is_django_template() {
            django_template_hover(db, file, offset).map(|range_info| {
                (
                    range_info.display(markup_kind).to_string(),
                    range_info.range,
                )
            })
        } else {
            hover(db, file, offset).map(|range_info| {
                (
                    range_info.display(db, markup_kind).to_string(),
                    range_info.range,
                )
            })
        };

        let Some((contents, range)) = hovered else {
            return Ok(None);
        };

        Ok(Some(lsp_types::Hover {
            contents: MarkupContent {
                kind: lsp_markup_kind,
                value: contents,
            }
            .into(),
            range: range
                .to_lsp_range(db, snapshot.encoding())
                .map(|lsp_range| lsp_range.local_range()),
        }))
    }
}

impl RetriableRequestHandler for HoverRequestHandler {}
