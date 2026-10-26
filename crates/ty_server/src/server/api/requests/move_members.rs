//! `by/prepareMoveMembers`, `by/pullMembersUp` and `by/pushMembersDown`: moving a class's members
//! into one of its superclasses, or into its subclasses. see [`ty_ide::pull_members_up`] for what
//! is moved and what is refused

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Range, Request, TextDocumentIdentifier, Uri,
};
use ruff_db::files::FileRange;
use ty_ide::{HierarchyClass, MemberKind, movable_members, pull_members_up, push_members_down};
use ty_project::{ProjectDatabase, SemanticDb as _};

use super::move_symbol::MoveSymbolAnswer;
use crate::document::{FileRangeExt, PositionExt};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum PrepareMoveMembersRequest {}

impl Request for PrepareMoveMembersRequest {
    type Params = PrepareMoveMembersParams;
    type Result = Option<MovableMembers>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/prepareMoveMembers");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// a position in a class: its header or its body
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PrepareMoveMembersParams {
    text_document: TextDocumentIdentifier,
    position: Position,
}

/// the class, its members, and the project's classes they could move to
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MovableMembers {
    class: ClassItem,
    members: Vec<MemberItem>,
    /// the project's classes it inherits from, nearest first: where `by/pullMembersUp` can pull
    superclasses: Vec<ClassItem>,
    /// the project's classes that name it as a base: where `by/pushMembersDown` pushes
    subclasses: Vec<ClassItem>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClassItem {
    name: String,
    /// its module's dotted name and its own; `null` for a class in a file no module is
    /// imported from
    qualified_name: Option<String>,
    uri: Uri,
    /// its name
    range: Range,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemberItem {
    name: String,
    /// `method`, `classmethod`, `staticmethod`, `property` or `attribute`
    kind: String,
    /// its name, in its first statement
    range: Range,
}

pub(crate) struct PrepareMoveMembersRequestHandler;

impl RequestHandler for PrepareMoveMembersRequestHandler {
    type RequestType = PrepareMoveMembersRequest;
}

fn class_item(
    db: &ProjectDatabase,
    snapshot: &DocumentSnapshot,
    class: HierarchyClass,
) -> Option<ClassItem> {
    let location = FileRange::new(class.file, class.name_range)
        .to_lsp_range(db, snapshot.encoding())?
        .to_location()?;
    Some(ClassItem {
        name: class.name,
        qualified_name: class.qualified_name,
        uri: location.uri,
        range: location.range,
    })
}

impl BackgroundDocumentRequestHandler for PrepareMoveMembersRequestHandler {
    fn document_uri(params: &PrepareMoveMembersParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` outside a class of the project, when language services are off and for a
    /// notebook or a template
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: PrepareMoveMembersParams,
    ) -> crate::server::Result<Option<MovableMembers>> {
        let Some((file, offset)) = position(db, snapshot, params.position) else {
            return Ok(None);
        };
        let Some(movable) = movable_members(db, db.program_file(file), offset) else {
            return Ok(None);
        };
        let Some(class) = class_item(db, snapshot, movable.class.clone()) else {
            return Ok(None);
        };
        let members = movable
            .members
            .into_iter()
            .filter_map(|member| {
                let range = FileRange::new(movable.class.file, member.name_range)
                    .to_lsp_range(db, snapshot.encoding())?
                    .local_range();
                Some(MemberItem {
                    name: member.name,
                    kind: match member.kind {
                        MemberKind::Method => "method",
                        MemberKind::ClassMethod => "classmethod",
                        MemberKind::StaticMethod => "staticmethod",
                        MemberKind::Property => "property",
                        MemberKind::Attribute => "attribute",
                    }
                    .to_string(),
                    range,
                })
            })
            .collect();
        Ok(Some(MovableMembers {
            class,
            members,
            superclasses: movable
                .superclasses
                .into_iter()
                .filter_map(|class| class_item(db, snapshot, class))
                .collect(),
            subclasses: movable
                .subclasses
                .into_iter()
                .filter_map(|class| class_item(db, snapshot, class))
                .collect(),
        }))
    }
}

impl RetriableRequestHandler for PrepareMoveMembersRequestHandler {}

/// the file and offset of a position in the snapshot's document, `None` where the request is not
/// answered
fn position(
    db: &ProjectDatabase,
    snapshot: &DocumentSnapshot,
    position: Position,
) -> Option<(ruff_db::files::File, ruff_text_size::TextSize)> {
    if snapshot
        .workspace_settings()
        .is_language_services_disabled()
        || snapshot.is_django_template()
    {
        return None;
    }
    let file = snapshot.document().to_notebook_or_file(db)?;
    if !file.source_type(db).is_py_file() {
        return None;
    }
    let offset = position.to_text_size(db, file, snapshot.document().uri(), snapshot.encoding())?;
    Some((file, offset))
}

pub(crate) enum PullMembersUpRequest {}

impl Request for PullMembersUpRequest {
    type Params = PullMembersUpParams;
    type Result = Option<MoveSymbolAnswer>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/pullMembersUp");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the class, the names of the members to pull up, and the superclass to pull them into
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PullMembersUpParams {
    text_document: TextDocumentIdentifier,
    position: Position,
    members: Vec<String>,
    /// the superclass's `qualifiedName`, as `by/prepareMoveMembers` lists it
    target: String,
}

pub(crate) struct PullMembersUpRequestHandler;

impl RequestHandler for PullMembersUpRequestHandler {
    type RequestType = PullMembersUpRequest;
}

impl BackgroundDocumentRequestHandler for PullMembersUpRequestHandler {
    fn document_uri(params: &PullMembersUpParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off and for a notebook or a template
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: PullMembersUpParams,
    ) -> crate::server::Result<Option<MoveSymbolAnswer>> {
        let Some((file, offset)) = position(db, snapshot, params.position) else {
            return Ok(None);
        };
        Ok(Some(MoveSymbolAnswer::of(
            db,
            snapshot,
            pull_members_up(
                db,
                db.program_file(file),
                offset,
                &params.members,
                &params.target,
            ),
        )))
    }
}

impl RetriableRequestHandler for PullMembersUpRequestHandler {
    /// the user is waiting on the move they asked for
    const RETRY_ON_CANCELLATION: bool = true;
}

pub(crate) enum PushMembersDownRequest {}

impl Request for PushMembersDownRequest {
    type Params = PushMembersDownParams;
    type Result = Option<MoveSymbolAnswer>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/pushMembersDown");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the class, and the names of the members to push down into its subclasses
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PushMembersDownParams {
    text_document: TextDocumentIdentifier,
    position: Position,
    members: Vec<String>,
}

pub(crate) struct PushMembersDownRequestHandler;

impl RequestHandler for PushMembersDownRequestHandler {
    type RequestType = PushMembersDownRequest;
}

impl BackgroundDocumentRequestHandler for PushMembersDownRequestHandler {
    fn document_uri(params: &PushMembersDownParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off and for a notebook or a template
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: PushMembersDownParams,
    ) -> crate::server::Result<Option<MoveSymbolAnswer>> {
        let Some((file, offset)) = position(db, snapshot, params.position) else {
            return Ok(None);
        };
        Ok(Some(MoveSymbolAnswer::of(
            db,
            snapshot,
            push_members_down(db, db.program_file(file), offset, &params.members),
        )))
    }
}

impl RetriableRequestHandler for PushMembersDownRequestHandler {
    /// the user is waiting on the move they asked for
    const RETRY_ON_CANCELLATION: bool = true;
}
