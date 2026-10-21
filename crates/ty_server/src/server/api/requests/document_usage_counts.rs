//! `by/documentUsageCounts`: how many places use each declaration of a document,
//! and how many classes subclass it or members override it
//!
//! the counts an editor draws above each declaration, "3 usages" or "2
//! overrides", for the whole document at once. each count is what
//! `textDocument/references` (without the declaration) or
//! `textDocument/implementation` (less the declaration itself) answers at the
//! declaration's name, from the same code, so a client goes to as many places as
//! the count says by sending that request when one is clicked. asking either of
//! those of each declaration instead would be two requests per declaration on
//! every pass over the document
//!
//! not a `textDocument/codeLens`, because what a client wants of these is not
//! what it wants of a lens: it draws them as its own kind of hint, under a
//! setting of its own, and asks again whenever anything in the workspace changes,
//! where a lens is asked for when its document changes. the counts are a search
//! of the workspace for each declaration, so a client asks only for the ones it
//! draws
//!
//! one document at a time, whole. `textHash` says which revision of this
//! document the answer is about, but the counts also read every other file of the
//! workspace, and no change to those is announced for this request. a client asks
//! again after it opens, edits or closes any document of the workspace

use std::borrow::Cow;

use lsp_types::{LspRequestMethod, MessageDirection, Range, Request, TextDocumentIdentifier, Uri};
use ty_ide::{DeclarationKind, UsageCountKinds, document_usage_counts};
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::ToRangeExt;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum DocumentUsageCountsRequest {}

impl Request for DocumentUsageCountsRequest {
    type Params = DocumentUsageCountsParams;
    type Result = Option<Vec<DeclarationUsages>>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/documentUsageCounts");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// which document to answer for, and which counts to work out
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DocumentUsageCountsParams {
    text_document: TextDocumentIdentifier,
    /// the counts wanted. a count not named is not worked out, and is left out
    /// of the answer
    counts: Vec<UsageCount>,
}

/// a count of [`DeclarationUsages`]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum UsageCount {
    References,
    Implementations,
}

/// what a declaration is
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Kind {
    Class,
    Function,
    /// a method or a property of a class
    Method,
}

/// a declaration of the document, with its counts
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeclarationUsages {
    name: String,
    kind: Kind,
    /// the whole declaration, its decorators included
    range: Range,
    /// its name, where `textDocument/references` and `textDocument/implementation`
    /// answer for it
    selection_range: Range,
    /// how many places use it, as `textDocument/references` answers without the
    /// declaration. there when asked for
    #[serde(skip_serializing_if = "Option::is_none")]
    references: Option<usize>,
    /// how many classes subclass a class, directly or not, and how many of their
    /// members override a method: what `textDocument/implementation` answers,
    /// less the declaration itself. there when asked for, on a class or a method
    #[serde(skip_serializing_if = "Option::is_none")]
    implementations: Option<usize>,
    /// whether a method is abstract, an `@abstractmethod` or a protocol member
    /// with no body, so that what overrides it implements it. only said of a
    /// method with implementations counted and found
    #[serde(rename = "abstract")]
    is_abstract: bool,
}

pub(crate) struct DocumentUsageCountsRequestHandler;

impl RequestHandler for DocumentUsageCountsRequestHandler {
    type RequestType = DocumentUsageCountsRequest;
}

impl BackgroundDocumentRequestHandler for DocumentUsageCountsRequestHandler {
    // what the project's files say, which is the same whether this one is a buffer or the file on
    // disk; the answer reads no version
    const ANSWERS_CLOSED_DOCUMENTS: bool = true;

    fn document_uri(params: &DocumentUsageCountsParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off, or for a document whose declarations are not
    /// counted here: a notebook, whose ranges are per cell, or a template. otherwise the
    /// declarations in source order: the document's classes and functions, and the methods and
    /// properties of those classes, less dunders and `extension` blocks
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: DocumentUsageCountsParams,
    ) -> crate::server::Result<Option<Vec<DeclarationUsages>>> {
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
        let kinds = UsageCountKinds {
            references: params.counts.contains(&UsageCount::References),
            implementations: params.counts.contains(&UsageCount::Implementations),
        };

        Ok(Some(
            document_usage_counts(db, db.program_file(file), kinds)
                .into_iter()
                .filter_map(|declaration| {
                    Some(DeclarationUsages {
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
                            DeclarationKind::Class => Kind::Class,
                            DeclarationKind::Function => Kind::Function,
                            DeclarationKind::Method => Kind::Method,
                        },
                        references: declaration.references,
                        implementations: declaration.implementations,
                        is_abstract: declaration.is_abstract,
                    })
                })
                .collect(),
        ))
    }
}

impl RetriableRequestHandler for DocumentUsageCountsRequestHandler {}
