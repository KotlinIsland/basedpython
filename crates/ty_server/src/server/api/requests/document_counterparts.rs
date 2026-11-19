//! `by/documentCounterparts`: the stub a runtime module is described by, or the runtime module a
//! stub describes, and the declarations the two share
//!
//! what an editor marks each such declaration with, a gutter icon going to the other side, for
//! the whole document at once: asking `textDocument/definition` of each name would be a request
//! per declaration on every pass over the document. which declarations pair, and why, is
//! `ty_ide::document_counterparts`'s: by where they sit among the module's members, every overload
//! and version branch included, and a stub's class-body `x: int` with the `self.x = ...` a
//! runtime method assigns.
//!
//! both sides are read as the editor has them, unsaved edits included: the counterpart is read
//! from its buffer when it is open. the answer is about this revision of the document and of its
//! counterpart, and no change to the counterpart is announced for this request, so a client asks
//! again when either changes.

use std::borrow::Cow;

use lsp_types::{LspRequestMethod, MessageDirection, Range, Request, TextDocumentIdentifier, Uri};
use ty_ide::{CounterpartKind, StubRole, document_counterparts};
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{ToLink, ToRangeExt};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum DocumentCounterpartsRequest {}

impl Request for DocumentCounterpartsRequest {
    type Params = DocumentCounterpartsParams;
    type Result = Option<DocumentCounterparts>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/documentCounterparts");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DocumentCounterpartsParams {
    text_document: TextDocumentIdentifier,
}

/// what the document is
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Role {
    /// a stub, describing the runtime module in `counterparts`
    Stub,
    /// a runtime module, described by the stub in `counterparts`
    Implementation,
}

/// what a declaration declares
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Kind {
    Class,
    Function,
    /// a variable, an attribute, a type alias, or an import of something other than a class or a
    /// function
    Variable,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DocumentCounterparts {
    role: Role,
    /// the file on the other side; never empty
    counterparts: Vec<Counterpart>,
    /// the document's declarations its counterpart declares too, in source order. a declaration
    /// the counterpart does not declare is not listed
    declarations: Vec<CounterpartDeclaration>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Counterpart {
    uri: Uri,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CounterpartDeclaration {
    /// where it sits among the module's members, dotted: `A.f`
    name: String,
    kind: Kind,
    /// the whole declaration, its decorators included, in this document: what a caret anywhere
    /// in it is on
    range: Range,
    /// the name it declares, in this document
    selection_range: Range,
    /// the counterpart's declarations of it; never empty. several for a stub's overloads, or a
    /// declaration in each branch of a version check
    targets: Vec<CounterpartTarget>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CounterpartTarget {
    uri: Uri,
    /// the whole declaration, its decorators included
    range: Range,
    /// the name it declares, which is where to go
    selection_range: Range,
}

pub(crate) struct DocumentCounterpartsRequestHandler;

impl RequestHandler for DocumentCounterpartsRequestHandler {
    type RequestType = DocumentCounterpartsRequest;
}

impl BackgroundDocumentRequestHandler for DocumentCounterpartsRequestHandler {
    fn document_uri(params: &DocumentCounterpartsParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off, for a notebook or a template, and for a document
    /// that is neither a stub with a runtime module nor a runtime module with a stub
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        _params: DocumentCounterpartsParams,
    ) -> crate::server::Result<Option<DocumentCounterparts>> {
        let start = std::time::Instant::now();
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
            || snapshot.document().is_cell_or_notebook()
            || snapshot.is_django_template()
        {
            return Ok(None);
        }
        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(None);
        };
        let encoding = snapshot.encoding();
        let Some(counterparts) = document_counterparts(db, db.program_file(file)) else {
            return Ok(None);
        };

        let declarations: Vec<CounterpartDeclaration> = counterparts
            .declarations
            .into_iter()
            .filter_map(|declaration| {
                let targets: Vec<CounterpartTarget> = declaration
                    .targets
                    .iter()
                    .filter_map(|target| {
                        let link = target.to_link(db, None, encoding)?;
                        Some(CounterpartTarget {
                            uri: link.target_uri,
                            range: link.target_range,
                            selection_range: link.target_selection_range,
                        })
                    })
                    .collect();
                if targets.is_empty() {
                    return None;
                }
                Some(CounterpartDeclaration {
                    range: declaration
                        .full_range
                        .to_lsp_range(db, file, encoding)?
                        .local_range(),
                    selection_range: declaration
                        .name_range
                        .to_lsp_range(db, file, encoding)?
                        .local_range(),
                    name: declaration.name,
                    kind: match declaration.kind {
                        CounterpartKind::Class => Kind::Class,
                        CounterpartKind::Function => Kind::Function,
                        CounterpartKind::Variable => Kind::Variable,
                    },
                    targets,
                })
            })
            .collect();

        let counterpart_uris: Vec<Counterpart> = counterparts
            .counterparts
            .into_iter()
            .filter_map(|counterpart| {
                let range = ruff_text_size::TextRange::default();
                let location = range
                    .to_lsp_range(db, counterpart, encoding)?
                    .into_location()?;
                Some(Counterpart { uri: location.uri })
            })
            .collect();
        if counterpart_uris.is_empty() {
            return Ok(None);
        }

        tracing::debug!(
            "by/documentCounterparts answered {} declarations in {:?}",
            declarations.len(),
            start.elapsed()
        );
        Ok(Some(DocumentCounterparts {
            role: match counterparts.role {
                StubRole::Stub => Role::Stub,
                StubRole::Implementation => Role::Implementation,
            },
            counterparts: counterpart_uris,
            declarations,
        }))
    }
}

impl RetriableRequestHandler for DocumentCounterpartsRequestHandler {}
