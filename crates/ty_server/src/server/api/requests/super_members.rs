//! `by/superMembers`: the superclass members a class member overrides
//!
//! what an editor's *Go to Super* asks of a method or an attribute, and what an "overrides" gutter
//! marker points at. LSP has no request for it: `textDocument/implementation` goes the other way,
//! from a member to the members that override it, and `typeHierarchy/supertypes` answers about
//! classes, not their members
//!
//! what counts as an override is the override checks' own walk up the MRO. of the declarations it
//! finds, the answer is the nearest along each branch of the MRO, in MRO order, so a member of a
//! class with several bases can override several

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Range, Request, TextDocumentIdentifier, Uri,
};
use ty_ide::super_members;
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::PositionEncoding;
use crate::document::{PositionExt, ToLink};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum SuperMembersRequest {}

impl Request for SuperMembersRequest {
    type Params = SuperMembersParams;
    type Result = Option<Vec<SuperMember>>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/superMembers");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// which member to ask about: the document, and a position on the member's name where it is
/// declared: the name of a `def` or a nested class, a name a class body assigns, annotates or
/// captures, or an import's alias
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SuperMembersParams {
    text_document: TextDocumentIdentifier,
    position: Position,
}

/// one superclass member the member overrides
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SuperMember {
    /// the member's name
    name: String,
    /// the superclass that declares it
    container_name: String,
    /// the file the superclass declares it in
    uri: Uri,
    /// the whole declaration
    range: Range,
    /// the declared name, which is where to go
    selection_range: Range,
    /// whether the superclass synthesizes the member rather than writing it, as a dataclass does
    /// its `__init__`. the ranges are then the superclass's own
    synthesized: bool,
    /// whether the member is abstract where the superclass declares it, an `@abstractmethod` or a
    /// protocol method with no implementation, so that overriding it implements it: what
    /// `abstract-instantiation` counts as abstract
    #[serde(rename = "abstract")]
    is_abstract: bool,
}

impl SuperMember {
    /// the member as a client reads it, or `None` when where it is cannot be said in the client's
    /// terms
    pub(super) fn from_ide(
        db: &ProjectDatabase,
        member: &ty_ide::SuperMember,
        encoding: PositionEncoding,
    ) -> Option<Self> {
        let link = member.target.to_link(db, None, encoding)?;
        Some(SuperMember {
            name: member.name.to_string(),
            container_name: member.superclass.to_string(),
            uri: link.target_uri,
            range: link.target_range,
            selection_range: link.target_selection_range,
            synthesized: member.synthesized,
            is_abstract: member.is_abstract,
        })
    }
}

pub(crate) struct SuperMembersRequestHandler;

impl RequestHandler for SuperMembersRequestHandler {
    type RequestType = SuperMembersRequest;
}

impl BackgroundDocumentRequestHandler for SuperMembersRequestHandler {
    // what the project's files say about a position, the same whether this one is a buffer or the
    // file on disk; the answer reads no version
    const ANSWERS_CLOSED_DOCUMENTS: bool = true;

    fn document_uri(params: &SuperMembersParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off, for a template, which has no python members, and
    /// when there is no class member declared at the position; an empty list when there is one and
    /// it overrides nothing
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: SuperMembersParams,
    ) -> crate::server::Result<Option<Vec<SuperMember>>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }
        if snapshot.is_django_template() {
            return Ok(None);
        }

        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };

        let Some(offset) =
            params
                .position
                .to_text_size(db, file, snapshot.uri(), snapshot.encoding())
        else {
            return Ok(None);
        };

        let Some(members) = super_members(db, db.program_file(file), offset) else {
            return Ok(None);
        };

        Ok(Some(
            members
                .iter()
                .filter_map(|member| SuperMember::from_ide(db, member, snapshot.encoding()))
                .collect(),
        ))
    }
}

impl RetriableRequestHandler for SuperMembersRequestHandler {}
