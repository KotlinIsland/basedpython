//! `by/overridableMembers`: the methods the class at a position could override, for an editor to
//! offer
//!
//! what an editor's *Override Members* and *Implement Members* show before writing anything: the
//! methods the class inherits, grouped by the superclass each comes from, with the abstract ones —
//! those the class has to implement — marked. `by/overrideMembers` writes the ones picked. LSP has
//! no request for either: a code action carries an edit worked out before anything is picked, and
//! here what the edit is depends on what is picked

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Request, TextDocumentIdentifier, Uri,
};
use ty_ide::{InheritedMethodKind, members_to_override};
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::PositionExt;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum OverridableMembersRequest {}

impl Request for OverridableMembersRequest {
    type Params = OverridableMembersParams;
    type Result = Option<OverridableMembers>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/overridableMembers");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// which class to ask about: the document, and a position in the class — its header, its body,
/// or an indented line below its last statement, where a member would be added
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct OverridableMembersParams {
    text_document: TextDocumentIdentifier,
    position: Position,
}

/// the class, and what it could override
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OverridableMembers {
    /// the class's name
    class_name: String,
    /// in MRO order of the superclass each is inherited from
    members: Vec<OverridableMember>,
}

/// one method the class inherits and does not define
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OverridableMember {
    /// the method's name, which is what `by/overrideMembers` is asked to write it by
    name: String,
    /// the superclass it is inherited from
    container_name: String,
    /// the module that declares the superclass
    #[serde(skip_serializing_if = "Option::is_none")]
    container_module: Option<String>,
    /// whether it is abstract where it is inherited from, so that the class has to implement it:
    /// what `abstract-instantiation` counts as abstract
    #[serde(rename = "abstract")]
    is_abstract: bool,
    kind: OverridableMemberKind,
    /// the declaration `by/overrideMembers` writes for it, from its name on:
    /// `greet(self, name: str) -> str`. an overloaded method's first overload
    signature: String,
}

/// how the method is reached, which is how its declaration says so
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum OverridableMemberKind {
    Method,
    ClassMethod,
    StaticMethod,
}

pub(crate) struct OverridableMembersRequestHandler;

impl RequestHandler for OverridableMembersRequestHandler {
    type RequestType = OverridableMembersRequest;
}

impl BackgroundDocumentRequestHandler for OverridableMembersRequestHandler {
    fn document_uri(params: &OverridableMembersParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off, for a document that is not basedpython source,
    /// and when there is no class at the position; a class that inherits nothing it could
    /// override has an empty list
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: OverridableMembersParams,
    ) -> crate::server::Result<Option<OverridableMembers>> {
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
        let Some(offset) =
            params
                .position
                .to_text_size(db, file, snapshot.document().uri(), snapshot.encoding())
        else {
            return Ok(None);
        };
        let Some(class) = members_to_override(db, db.program_file(file), offset) else {
            return Ok(None);
        };
        Ok(Some(OverridableMembers {
            class_name: class.class_name.to_string(),
            members: class
                .members
                .into_iter()
                .map(|member| OverridableMember {
                    name: member.name.to_string(),
                    container_name: member.superclass.to_string(),
                    container_module: member.superclass_module,
                    is_abstract: member.is_abstract,
                    kind: match member.kind {
                        InheritedMethodKind::Instance => OverridableMemberKind::Method,
                        InheritedMethodKind::Class => OverridableMemberKind::ClassMethod,
                        InheritedMethodKind::Static => OverridableMemberKind::StaticMethod,
                    },
                    signature: member.signature,
                })
                .collect(),
        }))
    }
}

impl RetriableRequestHandler for OverridableMembersRequestHandler {}
