//! `textDocument/onTypeFormatting` — a clause moved out to the statement it continues when its
//! `:` is typed. See [`ty_ide::on_type_formatting`].

use std::borrow::Cow;

use lsp_types::{DocumentOnTypeFormattingParams, DocumentOnTypeFormattingRequest, TextEdit, Uri};
use ty_ide::on_type_formatting;
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{PositionExt, ToRangeExt};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) struct OnTypeFormattingRequestHandler;

impl RequestHandler for OnTypeFormattingRequestHandler {
    type RequestType = DocumentOnTypeFormattingRequest;
}

impl BackgroundDocumentRequestHandler for OnTypeFormattingRequestHandler {
    fn document_uri(params: &DocumentOnTypeFormattingParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: DocumentOnTypeFormattingParams,
    ) -> crate::server::Result<Option<Vec<TextEdit>>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }
        // A notebook cell is not a file whose lines the statements above can be read from.
        if snapshot.document().is_cell_or_notebook() || snapshot.is_django_template() {
            return Ok(None);
        }
        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(None);
        };
        let Some(offset) =
            params
                .position
                .to_text_size(db, file, snapshot.document().uri(), snapshot.encoding())
        else {
            return Ok(None);
        };

        let python_file = db.program_file(file).python_file(db);
        let Some(reindent) = on_type_formatting(db, python_file, offset, &params.ch) else {
            return Ok(None);
        };
        let Some(range) = reindent
            .range
            .to_lsp_range(db, file, snapshot.encoding())
            .map(|range| range.local_range())
        else {
            return Ok(None);
        };
        Ok(Some(vec![TextEdit {
            range,
            new_text: reindent.indentation,
        }]))
    }
}

// The edit is only right for the text it was worked out on: one overtaken by an edit is refused
// rather than asked again, since the client applies it to the text as it is when it arrives.
impl RetriableRequestHandler for OnTypeFormattingRequestHandler {}
