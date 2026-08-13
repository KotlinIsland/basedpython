use std::borrow::Cow;

use anyhow::anyhow;
use lsp_server::ErrorCode;
use lsp_types::{
    PrepareRenameParams, PrepareRenamePlaceholder, PrepareRenameRequest, PrepareRenameResult, Uri,
};
use ty_ide::{PreparedTemplateRename, can_rename, django_prepare_rename};
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{PositionExt, ToRangeExt};
use crate::server::api::Error;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) struct PrepareRenameRequestHandler;

impl RequestHandler for PrepareRenameRequestHandler {
    type RequestType = PrepareRenameRequest;
}

impl BackgroundDocumentRequestHandler for PrepareRenameRequestHandler {
    fn document_uri(params: &PrepareRenameParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document_position_params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: PrepareRenameParams,
    ) -> crate::server::Result<Option<PrepareRenameResult>> {
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

        // a python symbol is answered exactly as it was; the django names a
        // module writes as plain strings are what is left over once it declines
        if !template && let Some(range) = can_rename(db, db.program_file(file), offset) {
            return Ok(range
                .to_lsp_range(db, file, snapshot.encoding())
                .map(|lsp_range| lsp_range.local_range().into()));
        }

        match django_prepare_rename(db, file, offset, template) {
            None => Ok(None),
            // the editor shows this rather than offering a rename it could not
            // finish, which is the whole point of asking first
            Some(PreparedTemplateRename::Refused(why)) => {
                Err(Error::new(anyhow!(why), ErrorCode::RequestFailed))
            }
            Some(PreparedTemplateRename::Ready { range, placeholder }) => Ok(range
                .to_lsp_range(db, file, snapshot.encoding())
                .map(|lsp_range| {
                    PrepareRenamePlaceholder::new(lsp_range.local_range(), placeholder).into()
                })),
        }
    }
}

impl RetriableRequestHandler for PrepareRenameRequestHandler {}
