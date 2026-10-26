//! `by/declaration`: what the name at a position declares, or refers to
//!
//! its kind, its name, and the dotted name it is reached by from outside its module:
//! `shapes.base.Circle.area` on `area` in `circle.area()`. what an editor's *copy reference*
//! copies, and how it names the target of a search for usages. LSP has no request for it:
//! `textDocument/hover` is prose for a reader, and `workspace/symbol` answers a query rather than
//! a position. see [`ty_ide::declaration_at`] for what is answered

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Range, Request, TextDocumentIdentifier, Uri,
};
use ruff_db::files::FileRange;
use ty_ide::declaration_at;
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{PositionExt, ToLink};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum DeclarationRequest {}

impl Request for DeclarationRequest {
    type Params = DeclarationParams;
    type Result = Option<Declaration>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/declaration");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the position of a name, where it is declared or where it is used
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DeclarationParams {
    text_document: TextDocumentIdentifier,
    position: Position,
}

/// what the name declares, and where
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Declaration {
    /// `module`, `class`, `function`, `method`, `property`, `attribute`, `variable`,
    /// `parameter`, `type parameter` or `type alias`
    kind: String,
    /// the declared name
    name: String,
    /// the module's dotted name, each class the declaration is made in, and the name. `null` for
    /// a name local to a function, a parameter, a type parameter, and a declaration in a file no
    /// module is imported from
    qualified_name: Option<String>,
    /// the name at the position the request asked about
    origin_selection_range: Option<Range>,
    /// the file the declaration is in
    uri: Uri,
    /// the whole declaration
    range: Range,
    /// the declared name
    selection_range: Range,
}

pub(crate) struct DeclarationRequestHandler;

impl RequestHandler for DeclarationRequestHandler {
    type RequestType = DeclarationRequest;
}

impl BackgroundDocumentRequestHandler for DeclarationRequestHandler {
    // what the project's files say about a position; the answer reads no version
    const ANSWERS_CLOSED_DOCUMENTS: bool = true;

    fn document_uri(params: &DeclarationParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off, for a template, where there is no name, and for a
    /// name whose definitions declare different things
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: DeclarationParams,
    ) -> crate::server::Result<Option<Declaration>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }
        if snapshot.is_django_template() {
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

        let Some(found) = declaration_at(db, db.program_file(file), offset) else {
            return Ok(None);
        };
        let origin: FileRange = found.range;
        let Some(link) = found
            .value
            .target
            .to_link(db, Some(origin), snapshot.encoding())
        else {
            return Ok(None);
        };
        let declaration = found.value.declaration;
        Ok(Some(Declaration {
            kind: declaration.kind.as_str().to_string(),
            name: declaration.name,
            qualified_name: declaration.qualified_name,
            origin_selection_range: link.origin_selection_range,
            uri: link.target_uri,
            range: link.target_range,
            selection_range: link.target_selection_range,
        }))
    }
}

impl RetriableRequestHandler for DeclarationRequestHandler {}
