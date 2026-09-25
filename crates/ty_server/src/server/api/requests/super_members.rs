//! `by/superMembers` — the superclass members a class member overrides.
//!
//! What an editor's *Go to Super* asks of a method or an attribute, and what an "overrides" gutter
//! marker points at. LSP has no request for it: `textDocument/implementation` goes the other way,
//! from a member to the members that override it, and `typeHierarchy/supertypes` answers about
//! classes, not their members.
//!
//! The answer is the override checks' own: a member is said to override exactly the superclass
//! members `invalid-method-override` compares it with — the nearest along each branch of the MRO,
//! in MRO order, so a member of a class with several bases can override several.

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Range, Request, TextDocumentIdentifier, Uri,
};
use ty_ide::super_members;
use ty_project::{ProjectDatabase, SemanticDb as _};

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

/// Which member to ask about: the document, and a position on the member's name where it is
/// declared — the name of a `def`, or a name a class body assigns or annotates.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SuperMembersParams {
    text_document: TextDocumentIdentifier,
    position: Position,
}

/// One superclass member the member overrides.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SuperMember {
    /// The member's name.
    name: String,
    /// The superclass that declares it.
    container_name: String,
    /// The file the superclass declares it in.
    uri: Uri,
    /// The whole declaration.
    range: Range,
    /// The declared name, which is where to go.
    selection_range: Range,
    /// Whether the superclass synthesizes the member rather than writing it, as a dataclass does
    /// its `__init__`. The ranges are then the superclass's own.
    synthesized: bool,
}

pub(crate) struct SuperMembersRequestHandler;

impl RequestHandler for SuperMembersRequestHandler {
    type RequestType = SuperMembersRequest;
}

impl BackgroundDocumentRequestHandler for SuperMembersRequestHandler {
    fn document_uri(params: &SuperMembersParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when there is no class member declared at the position; an empty list when there is
    /// one and it overrides nothing.
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
                .into_iter()
                .filter_map(|member| {
                    let link = member.target.to_link(db, None, snapshot.encoding())?;
                    Some(SuperMember {
                        name: member.name.to_string(),
                        container_name: member.superclass.to_string(),
                        uri: link.target_uri,
                        range: link.target_range,
                        selection_range: link.target_selection_range,
                        synthesized: member.synthesized,
                    })
                })
                .collect(),
        ))
    }
}

impl RetriableRequestHandler for SuperMembersRequestHandler {}
