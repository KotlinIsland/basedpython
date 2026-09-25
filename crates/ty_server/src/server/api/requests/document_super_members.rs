//! `by/documentSuperMembers`: every class member of a document that overrides something, and the
//! superclass members each one overrides
//!
//! `by/superMembers` for the whole document at once, answered from the same code. it is what an
//! editor asks to draw an "overrides" marker beside each member that has one: asking
//! `by/superMembers` of each member instead would be a request per member on every pass over the
//! document, most of them answered with an empty list
//!
//! one document at a time, whole. `textHash` says which revision of this document the answer is
//! about, but the answer also reads every file the document's classes inherit from, and no change
//! to those is announced for this request. a client asks again after it opens, edits or closes
//! any document of the workspace, not only this one, and when the server asks it to refresh its
//! inlay hints, which the server does, for a client that declares inlay hint refresh support,
//! after a change on disk or to the environment that changes what it answers from

use std::borrow::Cow;

use lsp_types::{LspRequestMethod, MessageDirection, Range, Request, TextDocumentIdentifier, Uri};
use ty_ide::document_super_members;
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::ToRangeExt;
use crate::server::api::requests::super_members::SuperMember;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum DocumentSuperMembersRequest {}

impl Request for DocumentSuperMembersRequest {
    type Params = DocumentSuperMembersParams;
    type Result = Option<Vec<OverridingMember>>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/documentSuperMembers");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// which document to answer for
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DocumentSuperMembersParams {
    text_document: TextDocumentIdentifier,
}

/// a class member of the document that overrides at least one superclass member
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OverridingMember {
    /// the member's name
    name: String,
    /// the class whose body declares it
    container_name: String,
    /// its name where it is declared: the name of a `def` or a nested class, a name a class body
    /// assigns, annotates or captures, or an import's alias. the position `by/superMembers` answers
    /// the same members at
    selection_range: Range,
    /// whether the member is itself abstract, as [`SuperMember`]'s `abstract` says of what it
    /// overrides
    #[serde(rename = "abstract")]
    is_abstract: bool,
    /// what it overrides directly, in MRO order, as `by/superMembers` answers it. never empty
    super_members: Vec<SuperMember>,
}

pub(crate) struct DocumentSuperMembersRequestHandler;

impl RequestHandler for DocumentSuperMembersRequestHandler {
    type RequestType = DocumentSuperMembersRequest;
}

impl BackgroundDocumentRequestHandler for DocumentSuperMembersRequestHandler {
    // what the project's files say, which is the same whether this one is a buffer or the file on
    // disk; the answer reads no version
    const ANSWERS_CLOSED_DOCUMENTS: bool = true;

    fn document_uri(params: &DocumentSuperMembersParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off, or for a document whose members are not answered
    /// here: a notebook, whose ranges are per cell, or a template. otherwise the members in source
    /// order, and an empty list when none overrides anything. a member declared more than once is
    /// listed at each declaration that names it in the source. constructors and the methods they
    /// call, `__init__`, `__new__`, `__post_init__` and `__init_subclass__`, are left out: neither
    /// `invalid-method-override` nor `missing-override-decorator` holds one to what it overrides,
    /// and `by/superMembers` still answers one when asked
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        _params: DocumentSuperMembersParams,
    ) -> crate::server::Result<Option<Vec<OverridingMember>>> {
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

        Ok(Some(
            document_super_members(db, db.program_file(file))
                .into_iter()
                .filter_map(|member| {
                    let selection_range = member
                        .name_range
                        .to_lsp_range(db, file, encoding)?
                        .local_range();
                    let super_members: Vec<_> = member
                        .super_members
                        .iter()
                        .filter_map(|member| SuperMember::from_ide(db, member, encoding))
                        .collect();
                    if super_members.is_empty() {
                        return None;
                    }
                    Some(OverridingMember {
                        name: member.name.to_string(),
                        container_name: member.class.to_string(),
                        selection_range,
                        is_abstract: member.is_abstract,
                        super_members,
                    })
                })
                .collect(),
        ))
    }
}

impl RetriableRequestHandler for DocumentSuperMembersRequestHandler {}
