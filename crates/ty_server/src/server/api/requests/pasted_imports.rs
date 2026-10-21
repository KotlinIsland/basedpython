//! `by/pastedImports`: which of the imports copied code depends on it is missing where it was
//! pasted, and the edits that add them
//!
//! the second half of `by/copiedImports`: an editor asks it once the text has landed, naming the
//! range it landed in and the imports `by/copiedImports` answered when it was copied. an import is
//! missing when its name is not bound in the scope the paste landed in; a name that is bound there
//! is left alone even when it is bound to something else, since an import would change what the
//! code around the paste reads
//!
//! the edits add every missing import together, against the document's text as the request names
//! it. a client that lets its user choose among them asks again with only the ones chosen, rather
//! than applying some of the edits: two imports from one module can be added to the same
//! statement, and the edits of a subset are not a subset of the edits

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Range, Request, TextDocumentIdentifier, TextEdit, Uri,
};
use ruff_text_size::Ranged;
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{RangeExt, ToRangeExt};
use crate::server::api::requests::copied_imports::CopiedImport;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum PastedImportsRequest {}

impl Request for PastedImportsRequest {
    type Params = PastedImportsParams;
    type Result = Option<PastedImports>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/pastedImports");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the document text was pasted into, the range it now occupies there, and the imports
/// `by/copiedImports` answered for it
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PastedImportsParams {
    text_document: TextDocumentIdentifier,
    range: Range,
    imports: Vec<CopiedImport>,
}

/// the imports the pasted code is missing, and the edits to the document that add all of them
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PastedImports {
    /// the ones of the imports asked about that are missing, in the order asked
    imports: Vec<CopiedImport>,
    /// edits to the document, which add every one of `imports`
    edits: Vec<TextEdit>,
}

pub(crate) struct PastedImportsRequestHandler;

impl RequestHandler for PastedImportsRequestHandler {
    type RequestType = PastedImportsRequest;
}

impl BackgroundDocumentRequestHandler for PastedImportsRequestHandler {
    fn document_uri(params: &PastedImportsParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off, for a document no import can be added to — a
    /// notebook or a template — or for a range that is not in the document. otherwise the missing
    /// imports and their edits, both empty when nothing is missing
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: PastedImportsParams,
    ) -> crate::server::Result<Option<PastedImports>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }
        if snapshot.document().is_cell_or_notebook() || snapshot.is_django_template() {
            return Ok(None);
        }
        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };
        let encoding = snapshot.encoding();
        let Some(range) = params
            .range
            .to_text_range(db, file, snapshot.uri(), encoding)
        else {
            return Ok(None);
        };
        let imports: Vec<ty_ide::CopiedImport> =
            params.imports.into_iter().map(Into::into).collect();

        let pasted = ty_ide::pasted_imports(db, db.program_file(file), range, &imports);
        let edits = pasted
            .edits
            .iter()
            .map(|edit| {
                Some(TextEdit {
                    range: edit.range().to_lsp_range(db, file, encoding)?.local_range(),
                    new_text: edit.content().unwrap_or_default().to_string(),
                })
            })
            .collect::<Option<Vec<_>>>();
        // an edit with no place in the document would leave its import out of what the edits
        // add, and the answer would no longer say what they do
        let Some(edits) = edits else {
            return Ok(None);
        };

        Ok(Some(PastedImports {
            imports: pasted.missing.into_iter().map(CopiedImport::from).collect(),
            edits,
        }))
    }
}

impl RetriableRequestHandler for PastedImportsRequestHandler {}
