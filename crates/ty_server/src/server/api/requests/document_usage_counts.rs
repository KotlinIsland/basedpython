//! `by/documentUsageCounts`: how many places use each declaration of a document,
//! and how many classes subclass it or members override it
//!
//! the counts an editor draws above each declaration, "3 usages" or "2
//! overrides", for the whole document at once. each count is what
//! `by/references` (without the declaration, less the references of kind
//! `import`) or `textDocument/implementation` (less the declaration itself)
//! answers at the declaration's name, from the same code, so a client goes to as
//! many places as the count says, and to the imports besides, by sending that
//! request when one is clicked. an import names a declaration without using it,
//! so it is listed but not counted. asking either of
//! those of each declaration instead would be two requests per declaration on
//! every pass over the document
//!
//! not a `textDocument/codeLens`, because what a client wants of these is not
//! what it wants of a lens: it draws them as its own kind of hint, under a
//! setting of its own, and asks again whenever anything in the workspace changes,
//! where a lens is asked for when its document changes. each count is a search
//! of the workspace, so a client asks only for the ones it draws, and for all of
//! them in one request
//!
//! the searches run on the search lane (`BackgroundSchedule::Search`), one at a
//! time on a thread of their own, so that a client asking on every pass never
//! fills the workers the requests someone waits on run on, and a count the
//! client cancels stops where it is
//!
//! one document at a time, whole. `textHash` says which revision of this
//! document the answer is about, but the counts also read every other file of the
//! workspace, and no change to those is announced for this request. a client asks
//! again after it opens, edits or closes any document of the workspace

use std::borrow::Cow;

use lsp_types::{LspRequestMethod, MessageDirection, Range, Request, TextDocumentIdentifier, Uri};
use ty_ide::{DeclarationKind, UsageCountKinds, document_usage_counts};
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{ToLink, ToRangeExt};
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
    /// how many places use it, as `by/references` answers without the declaration,
    /// less its imports. there when asked for, except on a dunder method, which
    /// the language calls rather than its name
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
    /// what `implementations` counts, in the order `textDocument/implementation`
    /// answers them, so that a client can name them without asking. there when
    /// `implementations` is
    #[serde(skip_serializing_if = "Option::is_none")]
    implementors: Option<Vec<Implementor>>,
}

/// a subclass of a class, or a member overriding a method
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Implementor {
    /// the subclass, or the class whose member overrides the method: `Circle`.
    /// absent for an implementation in no class
    #[serde(skip_serializing_if = "Option::is_none")]
    class_name: Option<String>,
    /// the dotted name of that class, `shapes.base.Circle`. absent for a class
    /// local to a function, or in a file no module is imported from
    #[serde(skip_serializing_if = "Option::is_none")]
    qualified_name: Option<String>,
    /// the file the subclass or the overriding member is declared in
    uri: Uri,
    /// the whole subclass or member, its decorators included
    range: Range,
    /// its name, which is where to go
    selection_range: Range,
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
    /// properties of those classes, less `extension` blocks, a module's dunder functions and a
    /// class's constructors and the methods they call (`__init__`, `__new__`, `__post_init__`,
    /// `__init_subclass__`), which override nothing
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
        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
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
                        implementors: declaration.implementations.map(|_| {
                            declaration
                                .implementors
                                .iter()
                                .filter_map(|implementor| {
                                    let link = implementor.target.to_link(db, None, encoding)?;
                                    let class = implementor.class.as_ref();
                                    Some(Implementor {
                                        class_name: class.map(|class| class.name.clone()),
                                        qualified_name: class
                                            .and_then(|class| class.qualified_name.clone()),
                                        uri: link.target_uri,
                                        range: link.target_range,
                                        selection_range: link.target_selection_range,
                                    })
                                })
                                .collect()
                        }),
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
