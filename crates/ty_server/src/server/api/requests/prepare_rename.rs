use std::borrow::Cow;

use anyhow::anyhow;
use lsp_server::ErrorCode;
use lsp_types::{
    PrepareRenameParams, PrepareRenamePlaceholder, PrepareRenameRequest, PrepareRenameResult, Uri,
};
use ty_ide::{PreparedRename, PreparedTemplateRename, django_prepare_rename, prepare_rename};
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

        // a template holds no python symbol, so only the django names are asked
        // about there. in a module, the django names it writes as plain strings
        // are what is left over once the python symbol is not one
        let python = (!template).then(|| prepare_rename(db, db.program_file(file), offset));
        if let Some(PreparedRename::Ready { range, placeholder }) = python {
            return Ok(range
                .to_lsp_range(db, file, snapshot.encoding())
                .map(|lsp_range| {
                    PrepareRenamePlaceholder::new(lsp_range.local_range(), placeholder).into()
                }));
        }

        match django_prepare_rename(db, file, offset, template) {
            // the editor shows why rather than doing nothing, which from where
            // the user sits cannot be told apart from the key not working
            None => match python {
                Some(PreparedRename::Refused(why)) => {
                    Err(Error::new(anyhow!(why), ErrorCode::RequestFailed))
                }
                Some(PreparedRename::Ready { .. } | PreparedRename::NoSymbol) | None => Ok(None),
            },
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
