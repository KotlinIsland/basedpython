//! `by/alignmentGroups` — which assignments the author lined up
//!
//! a custom request rather than something read off `textDocument/inlayHint`, because that request
//! can only answer *about hints* and the lines that matter most here are the ones with no hint at
//! all. in
//!
//! ```python
//! a     = [1, 2]
//! basdf = 1
//! ```
//!
//! it is `basdf` that has to move, and `basdf` gets no hint — `type_hint_is_excessive_for_expr`
//! suppresses a bare literal's type — so there is nowhere in an inlay hint reply to hang it
//!
//! the server answers which lines belong together and how much room the author left, and stops
//! there. how wide anything ends up is the client's, because only a client knows which hints are on
//! screen at a given instant: a kind can be switched off per editor, and push-to-hint draws a hint
//! only while a key is held. see [`ty_ide::alignment_groups`]

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Range, Request, TextDocumentIdentifier, Uri,
};
use ty_ide::alignment_groups;
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{RangeExt, TextSizeExt};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

/// the request a client sends before laying inlay hints out in a document
pub(crate) enum AlignmentGroupsRequest {}

impl Request for AlignmentGroupsRequest {
    type Params = AlignmentGroupsParams;
    type Result = Option<Vec<AlignmentGroup>>;
    // not a method LSP defines, so it goes across as a custom one, under the same `by/` prefix as
    // the rest of them
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/alignmentGroups");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the document to look through, and how much of it
///
/// `range` mirrors [`lsp_types::InlayHintParams`] so a client can ask both questions about the same
/// span. a group that only partly overlaps it comes back whole, since a column is a property of
/// every member at once
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AlignmentGroupsParams {
    pub(crate) text_document: TextDocumentIdentifier,
    pub(crate) range: Range,
}

/// assignments sharing one `=` column, which therefore have to be laid out together
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AlignmentGroup {
    pub(crate) members: Vec<AlignmentMember>,
}

/// one assignment's contribution to the column
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AlignmentMember {
    /// where the padding before the `=` starts
    ///
    /// this is not the one place a hint can land on this line. what displaces this line's column is
    /// every hint drawn on it at or before `gapEnd`, added together — `a, b = 1, 2` is hinted after
    /// `a` and again after `b`, and either alone understates how far the `=` moves
    pub(crate) gap_start: Position,

    /// the `=`
    pub(crate) gap_end: Position,
}

pub(crate) struct AlignmentGroupsRequestHandler;

impl RequestHandler for AlignmentGroupsRequestHandler {
    type RequestType = AlignmentGroupsRequest;
}

impl BackgroundDocumentRequestHandler for AlignmentGroupsRequestHandler {
    fn document_uri(params: &AlignmentGroupsParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: AlignmentGroupsParams,
    ) -> crate::server::Result<Option<Vec<AlignmentGroup>>> {
        // tied to the inlay hint settings rather than to one of its own: this exists only to keep
        // hints from disturbing a column, so with no hints being drawn there is nothing to keep
        let workspace_settings = snapshot.workspace_settings();
        if workspace_settings.is_language_services_disabled()
            || !workspace_settings.inlay_hints().any_enabled()
        {
            return Ok(None);
        }

        // a template is not laid out by this analysis: its hints sit in markup rather than in a
        // suite of statements, and there is no `=` column to keep
        if snapshot.is_django_template() {
            return Ok(None);
        }

        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };

        let Some(range) = params
            .range
            .to_text_range(db, file, snapshot.uri(), snapshot.encoding())
        else {
            return Ok(None);
        };

        let groups = alignment_groups(db, db.program_file(file).python_file(db), range)
            .into_iter()
            .filter_map(|group| {
                let positions = group
                    .members
                    .into_iter()
                    .map(|member| {
                        Some((
                            member
                                .gap_start
                                .to_lsp_position(db, file, snapshot.encoding())?,
                            member
                                .gap_end
                                .to_lsp_position(db, file, snapshot.encoding())?,
                        ))
                    })
                    // a member whose position will not convert takes the whole group with it: the
                    // column is the maximum over every member, and a group missing one is a group
                    // sized against the wrong maximum
                    .collect::<Option<Vec<_>>>()?;

                // a notebook is one file written as many cells, and a run of assignments can carry
                // across the seam between two of them. the positions sent below are cell-local, so
                // a group that straddles a seam would go out as lines from two different documents
                // presented as one, with the later line numbered from a later origin. drop it
                // rather than answer in coordinates that do not share one
                let (first, _) = positions.first()?;
                let document = first.uri();
                if !positions
                    .iter()
                    .all(|(start, end)| start.uri() == document && end.uri() == document)
                {
                    return None;
                }

                Some(AlignmentGroup {
                    members: positions
                        .into_iter()
                        .map(|(gap_start, gap_end)| AlignmentMember {
                            gap_start: gap_start.local_position(),
                            gap_end: gap_end.local_position(),
                        })
                        .collect(),
                })
            })
            .collect();

        Ok(Some(groups))
    }
}

impl RetriableRequestHandler for AlignmentGroupsRequestHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    /// the wire form, exactly as a client sends it. `range` mirrors `textDocument/inlayHint`, so a
    /// client can ask both questions about one span
    #[test]
    fn the_params_a_client_sends_parse() {
        let parsed: AlignmentGroupsParams = serde_json::from_str(
            r#"{"textDocument":{"uri":"file:///main.by"},"range":{"start":{"line":0,"character":0},"end":{"line":9,"character":0}}}"#,
        )
        .expect("a client sends a document and a range");

        assert_eq!(parsed.text_document.uri.as_str(), "file:///main.by");
        assert_eq!(parsed.range.end.line, 9);
    }

    /// the shape a client reads back. two members in one group is the whole point of the reply, so
    /// it is what is written out here
    #[test]
    fn the_response_a_client_reads_serialises() {
        let response = vec![AlignmentGroup {
            members: vec![
                AlignmentMember {
                    gap_start: Position::new(0, 1),
                    gap_end: Position::new(0, 6),
                },
                AlignmentMember {
                    gap_start: Position::new(1, 5),
                    gap_end: Position::new(1, 6),
                },
            ],
        }];

        assert_eq!(
            serde_json::to_string(&response).expect("a response to serialise"),
            r#"[{"members":[{"gapStart":{"line":0,"character":1},"gapEnd":{"line":0,"character":6}},{"gapStart":{"line":1,"character":5},"gapEnd":{"line":1,"character":6}}]}]"#
        );
    }
}
