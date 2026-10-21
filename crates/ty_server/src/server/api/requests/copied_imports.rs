//! `by/copiedImports`: the imports some code copied out of a document depends on
//!
//! what an editor asks once copied text has been pasted, so that the paste can import what the code
//! needs where it landed: `by/pastedImports` takes the answer and says which of the imports the
//! paste is missing. the request carries the copied text and the place in the document it was copied
//! from, rather than a range of the document: a cut takes the text out of the document, and the
//! names in it meant what the scope at that place binds, which the cut leaves where it was. the
//! answer describes each import — a module and a member — rather than pointing into this document,
//! so it means the same thing in another one
//!
//! ask with `textHash` naming the text the positions are in, the document as it is when asked

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Request, TextDocumentIdentifier, Uri,
};
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::PositionExt;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum CopiedImportsRequest {}

impl Request for CopiedImportsRequest {
    type Params = CopiedImportsParams;
    type Result = Option<Vec<CopiedImport>>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/copiedImports");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the document text was copied from, and what was copied: one piece, or one per caret
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CopiedImportsParams {
    text_document: TextDocumentIdentifier,
    copies: Vec<Copied>,
}

/// a piece of copied text, and the position in the document it was copied from: where its first
/// character was
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Copied {
    text: String,
    position: Position,
}

/// a name the copied code uses, and the import that binds it
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CopiedImport {
    /// the name the copied code uses
    name: String,
    /// the module to import, by its absolute name
    module: String,
    /// what to import from the module, when the name is bound to a member of it rather than to the
    /// module itself: the member's own name, which differs from `name` when it was imported under
    /// an alias. absent for `import module` and `import module as name`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    member: Option<String>,
}

impl From<ty_ide::CopiedImport> for CopiedImport {
    fn from(import: ty_ide::CopiedImport) -> Self {
        Self {
            name: import.name,
            module: import.module,
            member: import.member,
        }
    }
}

impl From<CopiedImport> for ty_ide::CopiedImport {
    fn from(import: CopiedImport) -> Self {
        Self {
            name: import.name,
            module: import.module,
            member: import.member,
        }
    }
}

pub(crate) struct CopiedImportsRequestHandler;

impl RequestHandler for CopiedImportsRequestHandler {
    type RequestType = CopiedImportsRequest;
}

impl BackgroundDocumentRequestHandler for CopiedImportsRequestHandler {
    fn document_uri(params: &CopiedImportsParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off, or for a document whose code cannot be pasted into a
    /// module: a notebook, whose positions are per cell, or a template. otherwise each name the
    /// copied text uses that the document's global scope binds where it was copied from — by an
    /// import, or by defining it, when the document is a module others can import — once, in the
    /// order the text first uses them, and an empty list when there are none. a piece whose
    /// position is not in the document is left out
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: CopiedImportsParams,
    ) -> crate::server::Result<Option<Vec<CopiedImport>>> {
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
        let copies: Vec<_> = params
            .copies
            .iter()
            .filter_map(|copy| {
                let at =
                    copy.position
                        .to_text_size(db, file, snapshot.uri(), snapshot.encoding())?;
                Some(ty_ide::CopiedCode {
                    text: &copy.text,
                    at,
                })
            })
            .collect();

        Ok(Some(
            ty_ide::copied_imports(db, db.program_file(file), &copies)
                .into_iter()
                .map(CopiedImport::from)
                .collect(),
        ))
    }
}

impl RetriableRequestHandler for CopiedImportsRequestHandler {}
