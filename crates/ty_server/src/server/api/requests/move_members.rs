//! `by/prepareMoveMembers`, `by/pullMembersUp` and `by/pushMembersDown`: moving a class's members
//! into one of its superclasses, or into its subclasses. see [`ty_ide::pull_members_up`] for what
//! is moved and what is refused

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Range, Request, TextDocumentIdentifier, Uri,
};
use ruff_db::files::FileRange;
use ruff_text_size::TextSize;
use ty_ide::{
    HierarchyClass, MemberKind, extract_superclass, movable_members, pull_members_up,
    push_members_down,
};
use ty_project::{ProjectDatabase, SemanticDb as _};
use ty_python_core::ProgramFile;

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
    /// `method`, `classmethod`, `staticmethod`, `property` or `attribute`, or `base` for a base
    /// the class statement writes, which a pull up moves into the superclass's bases
    kind: String,
    /// how it reads in a list: a method or property as its name and parameters as written,
    /// `fetch(self, key: str)`, an attribute as its name
    detail: String,
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
                        MemberKind::Base => "base",
                    }
                    .to_string(),
                    detail: member.detail,
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

/// the class, the members and bases to pull up, and the superclass to pull them into
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PullMembersUpParams {
    text_document: TextDocumentIdentifier,
    position: Position,
    /// the names of the members to pull up, as `by/prepareMoveMembers` lists them. without
    /// `bases`, the bases to pull up are named here too, each as listed with the kind `base`
    members: Vec<String>,
    /// the bases to pull up, each as `by/prepareMoveMembers` lists one of the kind `base`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bases: Option<Vec<String>>,
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
        let file = db.program_file(file);
        let parts = match params.bases {
            Some(bases) => Ok((params.members, bases)),
            None => members_and_bases(db, file, offset, params.members),
        };
        Ok(Some(MoveSymbolAnswer::of(
            db,
            snapshot,
            parts.and_then(|(members, bases)| {
                pull_members_up(db, file, offset, &members, &bases, &params.target)
            }),
        )))
    }
}

/// `names`, a request's `members` that names the bases to pull up among the members, split into
/// the members and the bases by the kind `by/prepareMoveMembers` lists each with. a name it lists
/// as both is refused, and one it does not list is left with the members, for the pull to refuse
fn members_and_bases(
    db: &ProjectDatabase,
    file: ProgramFile<'_>,
    offset: TextSize,
    names: Vec<String>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let movable =
        movable_members(db, file, offset).ok_or_else(|| "there is no class here".to_string())?;
    let mut members = Vec::new();
    let mut bases = Vec::new();
    for name in names {
        let listed = |base: bool| {
            movable
                .members
                .iter()
                .any(|member| member.name == name && (member.kind == MemberKind::Base) == base)
        };
        match (listed(false), listed(true)) {
            (true, true) => {
                return Err(format!(
                    "`{name}` is both a member of {} and one of its bases; name the bases to pull \
                     up in `bases`",
                    movable.class.name
                ));
            }
            (false, true) => bases.push(name),
            _ => members.push(name),
        }
    }
    Ok((members, bases))
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

pub(crate) enum ExtractSuperclassRequest {}

impl Request for ExtractSuperclassRequest {
    type Params = ExtractSuperclassParams;
    type Result = Option<MoveSymbolAnswer>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/extractSuperclass");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the class, the names of the members the new class takes, and the new class's name
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExtractSuperclassParams {
    text_document: TextDocumentIdentifier,
    position: Position,
    members: Vec<String>,
    name: String,
}

pub(crate) struct ExtractSuperclassRequestHandler;

impl RequestHandler for ExtractSuperclassRequestHandler {
    type RequestType = ExtractSuperclassRequest;
}

impl BackgroundDocumentRequestHandler for ExtractSuperclassRequestHandler {
    fn document_uri(params: &ExtractSuperclassParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off and for a notebook or a template
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: ExtractSuperclassParams,
    ) -> crate::server::Result<Option<MoveSymbolAnswer>> {
        let Some((file, offset)) = position(db, snapshot, params.position) else {
            return Ok(None);
        };
        Ok(Some(MoveSymbolAnswer::of(
            db,
            snapshot,
            extract_superclass(
                db,
                db.program_file(file),
                offset,
                &params.members,
                &params.name,
            ),
        )))
    }
}

impl RetriableRequestHandler for ExtractSuperclassRequestHandler {
    /// the user is waiting on the refactoring they asked for
    const RETRY_ON_CANCELLATION: bool = true;
}
