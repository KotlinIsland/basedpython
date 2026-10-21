//! `by/overrideMembers`: the declarations overriding the methods an editor's user picked from
//! `by/overridableMembers`
//!
//! each is written the way basedpython writes an override — the `override` modifier, the
//! signature as the class inherits it, each type spelled for the document and imported when it is
//! not reached already — with a body that calls the method it overrides, or `...` for one the class
//! has to implement. the answer is the edits that write them and where the first one's body is
//! once they are applied, which is where the user goes next

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Range, Request, TextDocumentIdentifier, TextEdit,
    Uri,
};
use ruff_python_ast::name::Name;
use ruff_source_file::LineIndex;
use ruff_text_size::Ranged;
use ty_ide::override_members;
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{PositionExt, ToRangeExt, text_range_to_lsp_range};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum OverrideMembersRequest {}

impl Request for OverrideMembersRequest {
    type Params = OverrideMembersParams;
    type Result = Option<OverrideMembers>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/overrideMembers");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// which class, as `by/overridableMembers` is asked about it, and which of the members it answered
/// to write. the position is also where they are written: after the statement of the class body
/// it is in or follows
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct OverrideMembersParams {
    text_document: TextDocumentIdentifier,
    position: Position,
    /// the names of the members to write, as `by/overridableMembers` gives them
    members: Vec<String>,
}

/// the declarations, as edits to the document
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OverrideMembers {
    /// the declarations, and the imports they need. none of them overlap
    edits: Vec<TextEdit>,
    /// the first declaration's body, in the document as the edits leave it: what the user
    /// replaces first
    selection: Range,
}

pub(crate) struct OverrideMembersRequestHandler;

impl RequestHandler for OverrideMembersRequestHandler {
    type RequestType = OverrideMembersRequest;
}

impl BackgroundDocumentRequestHandler for OverrideMembersRequestHandler {
    fn document_uri(params: &OverrideMembersParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when `by/overridableMembers` would be, and when none of the members asked for is
    /// one it lists
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: OverrideMembersParams,
    ) -> crate::server::Result<Option<OverrideMembers>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
            || snapshot.is_django_template()
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
        let names: Vec<Name> = params.members.iter().map(Name::new).collect();
        let Some(overrides) = override_members(db, db.program_file(file), offset, &names) else {
            return Ok(None);
        };

        let mut edits = Vec::with_capacity(overrides.edits.len());
        for edit in &overrides.edits {
            let Some(range) = edit.range().to_lsp_range(db, file, snapshot.encoding()) else {
                return Ok(None);
            };
            edits.push(TextEdit {
                range: range.local_range(),
                new_text: edit.content().unwrap_or_default().to_string(),
            });
        }

        // the selection is in the text the edits leave behind, so it is a position in that text
        let source = ruff_db::source::source_text(db, file);
        let mut edited = source.as_str().to_string();
        let mut applied = overrides.edits.clone();
        applied.sort_by_key(|edit| std::cmp::Reverse(edit.start()));
        for edit in &applied {
            edited.replace_range(
                edit.range().to_std_range(),
                edit.content().unwrap_or_default(),
            );
        }
        let selection = text_range_to_lsp_range(
            overrides.selection,
            &edited,
            &LineIndex::from_source_text(&edited),
            snapshot.encoding(),
        );

        Ok(Some(OverrideMembers { edits, selection }))
    }
}

impl RetriableRequestHandler for OverrideMembersRequestHandler {}
