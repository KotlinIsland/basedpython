//! `by/mro`: the classes a class inherits from, in its method resolution order
//!
//! where python looks a member up after the class itself: what an editor's *go to super* offers on
//! a class, as PyCharm lists a class's superclasses. `typeHierarchy/supertypes` answers only the
//! direct bases, and putting their ancestors in python's order is C3 linearization over every
//! base, which a client should not redo. see [`ty_ide::type_hierarchy_mro`]

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Request, TextDocumentIdentifier,
    TypeHierarchyItem, Uri,
};
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::PositionExt;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::server::api::type_hierarchy::convert_to_lsp_item;
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum MroRequest {}

impl Request for MroRequest {
    type Params = MroParams;
    type Result = Option<Vec<TypeHierarchyItem>>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/mro");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// a position on a class's name, where it is declared or where it is used, as
/// `textDocument/prepareTypeHierarchy` takes it
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MroParams {
    text_document: TextDocumentIdentifier,
    position: Position,
}

pub(crate) struct MroRequestHandler;

impl RequestHandler for MroRequestHandler {
    type RequestType = MroRequest;
}

impl BackgroundDocumentRequestHandler for MroRequestHandler {
    fn document_uri(params: &MroParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off, and where there is no class. otherwise the classes
    /// the class inherits from, without itself, `object` last, each as `typeHierarchy/supertypes`
    /// answers a class
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: MroParams,
    ) -> crate::server::Result<Option<Vec<TypeHierarchyItem>>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
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
        let Some(classes) = ty_ide::type_hierarchy_mro(db, db.program_file(file), offset) else {
            return Ok(None);
        };
        Ok(Some(
            classes
                .into_iter()
                .filter_map(|class| convert_to_lsp_item(db, class, snapshot.encoding()))
                .collect(),
        ))
    }
}

impl RetriableRequestHandler for MroRequestHandler {}
