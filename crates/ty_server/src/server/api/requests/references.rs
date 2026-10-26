use std::borrow::Cow;

use lsp_types::ReferencesRequest;
use lsp_types::{Location, Position, ReferenceParams, Uri};
use ty_ide::{ReferenceTarget, django_references, find_references};
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{PositionExt, ToLink};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) struct ReferencesRequestHandler;

impl RequestHandler for ReferencesRequestHandler {
    type RequestType = ReferencesRequest;
}

impl BackgroundDocumentRequestHandler for ReferencesRequestHandler {
    fn document_uri(params: &ReferenceParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document_position_params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: ReferenceParams,
    ) -> crate::server::Result<Option<Vec<Location>>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }

        let Some(references_result) = references_at(
            db,
            snapshot,
            params.text_document_position_params.position,
            params.context.include_declaration,
        ) else {
            return Ok(None);
        };

        let locations: Vec<_> = references_result
            .into_iter()
            .filter_map(|target| target.to_location(db, snapshot.encoding()))
            .collect();

        Ok(Some(locations))
    }
}

impl RetriableRequestHandler for ReferencesRequestHandler {}

/// what `textDocument/references` finds from `position` in the snapshot's document, `None` where
/// there is no name to search for. `by/references` answers the same references
pub(crate) fn references_at(
    db: &ProjectDatabase,
    snapshot: &DocumentSnapshot,
    position: Position,
    include_declaration: bool,
) -> Option<Vec<ReferenceTarget>> {
    let file = snapshot.document().to_notebook_or_file(db)?;
    let offset = position.to_text_size(db, file, snapshot.document().uri(), snapshot.encoding())?;
    let template = snapshot.is_django_template();

    // a python symbol's references are found exactly as they were; the django
    // names a module writes as plain strings are what is left over once it
    // declines
    (!template)
        .then(|| find_references(db, db.program_file(file), offset, include_declaration))
        .flatten()
        .or_else(|| django_references(db, file, offset, include_declaration, template))
}
