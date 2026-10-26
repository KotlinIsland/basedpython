use std::borrow::Cow;

use lsp_types::FoldingRangeRequest;
use lsp_types::{FoldingRange, FoldingRangeKind, FoldingRangeParams, Uri};
use ty_ide::{django_template_folding_ranges, folding_ranges};
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::capabilities::ResolvedClientCapabilities;
use crate::document::{ToRangeExt, cell_range};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) struct FoldingRangeRequestHandler;

impl RequestHandler for FoldingRangeRequestHandler {
    type RequestType = FoldingRangeRequest;
}

impl BackgroundDocumentRequestHandler for FoldingRangeRequestHandler {
    fn document_uri(params: &FoldingRangeParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        _params: FoldingRangeParams,
    ) -> crate::server::Result<Option<Vec<FoldingRange>>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }

        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(None);
        };

        // If this document is a notebook cell, only return folding ranges for this cell.
        // Folding ranges are reported as local ranges, so returning notebook-wide ranges
        // would produce invalid coordinates for every other cell. A cell without a range in the
        // notebook's Python source, such as one that uses a cell magic, has no folding ranges.
        let cell_range = if snapshot.document().is_cell() {
            let Some(range) = cell_range(db, file, snapshot.document().uri()) else {
                return Ok(None);
            };
            Some(range)
        } else {
            None
        };

        let ranges = if snapshot.is_django_template() {
            django_template_folding_ranges(db, file)
        } else {
            folding_ranges(db, db.program_file(file).python_file(db), cell_range)
        };

        let capabilities = snapshot.resolved_client_capabilities();
        let results: Vec<_> = ranges
            .into_iter()
            .filter_map(|folding_range| {
                let lsp_range = folding_range
                    .range
                    .to_lsp_range(db, file, snapshot.encoding())?;
                let range = lsp_range.local_range();

                // a range on one line folds part of that line, which a client that folds whole
                // lines cannot do, and which a client that does not know what the range is (an
                // annotation is the only such range) has no reason to want
                if range.start.line == range.end.line
                    && (capabilities.folds_whole_lines_only()
                        || !folding_range
                            .kind
                            .is_some_and(|kind| capabilities.supports_folding_range_kind(kind)))
                {
                    return None;
                }

                let kind = folding_range
                    .kind
                    .and_then(|kind| lsp_kind(kind, capabilities));
                let collapsed_text = folding_range
                    .collapsed_text
                    .filter(|_| capabilities.supports_folding_range_collapsed_text());

                Some(FoldingRange {
                    start_line: range.start.line,
                    start_character: Some(range.start.character),
                    end_line: range.end.line,
                    end_character: Some(range.end.character),
                    kind,
                    collapsed_text,
                })
            })
            .collect();

        if results.is_empty() {
            Ok(None)
        } else {
            Ok(Some(results))
        }
    }
}

/// the kind a client is told a range is
///
/// the kinds LSP defines go to every client. basedpython's own go to a client that listed them
/// in `textDocument.foldingRange.foldingRangeKind.valueSet`; any other client is told a docstring
/// is a comment, as LSP has no kind closer to it, and is told nothing of the others
fn lsp_kind(
    kind: ty_ide::FoldingRangeKind,
    capabilities: ResolvedClientCapabilities,
) -> Option<FoldingRangeKind> {
    match kind {
        ty_ide::FoldingRangeKind::Comment => Some(FoldingRangeKind::Comment),
        ty_ide::FoldingRangeKind::Imports => Some(FoldingRangeKind::Imports),
        ty_ide::FoldingRangeKind::Region => Some(FoldingRangeKind::Region),
        custom if capabilities.supports_folding_range_kind(custom) => custom
            .as_lsp_custom()
            .map(|name| FoldingRangeKind::Custom(name.into())),
        ty_ide::FoldingRangeKind::Docstring => Some(FoldingRangeKind::Comment),
        ty_ide::FoldingRangeKind::Function
        | ty_ide::FoldingRangeKind::Class
        | ty_ide::FoldingRangeKind::String
        | ty_ide::FoldingRangeKind::Collection
        | ty_ide::FoldingRangeKind::Annotation => None,
    }
}

impl RetriableRequestHandler for FoldingRangeRequestHandler {}
