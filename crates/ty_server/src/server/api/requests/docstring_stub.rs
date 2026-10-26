//! `by/docstringStub`: the docstring an editor writes into an empty one, with the function's
//! parameters and what it returns, in the format the project writes docstrings in. see
//! [`ty_ide::docstring_stub`] for what is answered

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Range, Request, TextDocumentIdentifier, Uri,
};
use ty_ide::{DocstringFormat, docstring_stub};
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{PositionExt, ToRangeExt};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum DocstringStubRequest {}

impl Request for DocstringStubRequest {
    type Params = DocstringStubParams;
    type Result = Option<DocstringStubAnswer>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/docstringStub");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DocstringStubParams {
    text_document: TextDocumentIdentifier,
    /// a position inside the empty docstring
    position: Position,
    /// the format to write; the one the file's other docstrings are written in when absent
    #[serde(default)]
    format: Option<StubFormat>,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum StubFormat {
    Rest,
    Google,
    Numpy,
    Plain,
}

/// replace `range` with `newText`, then put the caret at `caret`, a position in the document as
/// it is after the edit
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DocstringStubAnswer {
    range: Range,
    new_text: String,
    caret: Position,
}

pub(crate) struct DocstringStubRequestHandler;

impl RequestHandler for DocstringStubRequestHandler {
    type RequestType = DocstringStubRequest;
}

impl BackgroundDocumentRequestHandler for DocstringStubRequestHandler {
    fn document_uri(params: &DocstringStubParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` where there is no empty docstring to fill in, when language services are off and
    /// for a notebook or a template
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: DocstringStubParams,
    ) -> crate::server::Result<Option<DocstringStubAnswer>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
            || snapshot.is_django_template()
        {
            return Ok(None);
        }
        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(None);
        };
        if !file.source_type(db).is_py_file() {
            return Ok(None);
        }
        let Some(offset) =
            params
                .position
                .to_text_size(db, file, snapshot.document().uri(), snapshot.encoding())
        else {
            return Ok(None);
        };
        let format = params.format.map(|format| match format {
            StubFormat::Rest => DocstringFormat::Rest,
            StubFormat::Google => DocstringFormat::Google,
            StubFormat::Numpy => DocstringFormat::Numpy,
            StubFormat::Plain => DocstringFormat::Plain,
        });
        let Some(stub) = docstring_stub(db, db.program_file(file), offset, format) else {
            return Ok(None);
        };
        let Some(range) = stub.range.to_lsp_range(db, file, snapshot.encoding()) else {
            return Ok(None);
        };
        let range = range.local_range();
        Ok(Some(DocstringStubAnswer {
            caret: Position::new(range.start.line + stub.caret_line, stub.caret_column),
            range,
            new_text: stub.text,
        }))
    }
}

impl RetriableRequestHandler for DocstringStubRequestHandler {}
