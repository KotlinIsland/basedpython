//! `by/resolveMergeConflicts` and `by/diffIgnoredRanges`: what an editor's merge and diff tools
//! ask a language about the texts they compare
//!
//! both take texts rather than documents: a merge's base, ours and theirs, and a diff's two sides,
//! are versions no file on disk holds. a `uri`, where one is given, names the file the texts are
//! versions of, which says how they are parsed and which isort configuration sorts their imports.
//! see [`ty_ide::resolve_file_merge_conflicts`] and [`ty_ide::diff_ignored_ranges`] for what is
//! answered

use lsp_types::{LspRequestMethod, MessageDirection, Range, Request, Uri};
use ruff_db::system::SystemPathBuf;
use ruff_python_ast::PySourceType;
use ruff_source_file::LineIndex;
use ty_ide::{ConflictLines, MergeTexts, diff_ignored_ranges, resolve_file_merge_conflicts};

use crate::document::text_range_to_lsp_range;
use crate::server::api::Error;
use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;

pub(crate) enum ResolveMergeConflictsRequest {}

impl Request for ResolveMergeConflictsRequest {
    type Params = ResolveMergeConflictsParams;
    type Result = Vec<Option<String>>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/resolveMergeConflicts");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ResolveMergeConflictsParams {
    base: String,
    ours: String,
    theirs: String,
    conflicts: Vec<Conflict>,
    #[serde(default)]
    uri: Option<Uri>,
}

/// the lines a conflict spans in each version
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Conflict {
    base: LineRange,
    ours: LineRange,
    theirs: LineRange,
}

/// lines `start` to `end`, 0-based, `end` excluded, which is not before `start`
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LineRange {
    start: usize,
    end: usize,
}

impl TryFrom<&LineRange> for std::ops::Range<usize> {
    type Error = Error;

    fn try_from(lines: &LineRange) -> Result<Self, Error> {
        if lines.start > lines.end {
            return Err(Error {
                code: lsp_server::ErrorCode::InvalidParams,
                error: anyhow::anyhow!(
                    "a conflict's lines end at {} before they start at {}",
                    lines.end,
                    lines.start
                ),
            });
        }
        Ok(lines.start..lines.end)
    }
}

pub(crate) struct ResolveMergeConflictsHandler;

impl RequestHandler for ResolveMergeConflictsHandler {
    type RequestType = ResolveMergeConflictsRequest;
}

impl BackgroundRequestHandler for ResolveMergeConflictsHandler {
    /// the text each conflict resolves to, or `null` for one that is not resolved: all `null`
    /// without a project to read the configuration of
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        params: ResolveMergeConflictsParams,
    ) -> crate::server::Result<Vec<Option<String>>> {
        let path = params
            .uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok())
            .and_then(|path| SystemPathBuf::from_path_buf(path).ok());
        let conflicts = params
            .conflicts
            .iter()
            .map(|conflict| {
                Ok(ConflictLines {
                    base: (&conflict.base).try_into()?,
                    ours: (&conflict.ours).try_into()?,
                    theirs: (&conflict.theirs).try_into()?,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        // the project the file is in, or the first, whose configuration is read for it
        let db = path
            .as_ref()
            .and_then(|path| {
                snapshot
                    .projects()
                    .iter()
                    .find(|db| path.starts_with(ty_project::Db::project(*db).root(*db)))
            })
            .or_else(|| snapshot.projects().first());
        let Some(db) = db else {
            return Ok(vec![None; conflicts.len()]);
        };
        Ok(resolve_file_merge_conflicts(
            db,
            path.as_deref(),
            MergeTexts {
                base: &params.base,
                ours: &params.ours,
                theirs: &params.theirs,
            },
            &conflicts,
        ))
    }
}

impl RetriableRequestHandler for ResolveMergeConflictsHandler {}

pub(crate) enum DiffIgnoredRangesRequest {}

impl Request for DiffIgnoredRangesRequest {
    type Params = DiffIgnoredRangesParams;
    type Result = DiffIgnoredRanges;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/diffIgnoredRanges");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DiffIgnoredRangesParams {
    text: String,
    #[serde(default)]
    uri: Option<Uri>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiffIgnoredRanges {
    /// each import statement
    imports: Vec<Range>,
    /// each run of text that is only formatting
    formatting: Vec<Range>,
}

pub(crate) struct DiffIgnoredRangesHandler;

impl RequestHandler for DiffIgnoredRangesHandler {
    type RequestType = DiffIgnoredRangesRequest;
}

impl BackgroundRequestHandler for DiffIgnoredRangesHandler {
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        params: DiffIgnoredRangesParams,
    ) -> crate::server::Result<DiffIgnoredRanges> {
        let source_type = params
            .uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok())
            .and_then(PySourceType::try_from_path)
            .unwrap_or_default();
        let ignored = diff_ignored_ranges(&params.text, source_type);
        let index = LineIndex::from_source_text(&params.text);
        let encoding = snapshot.position_encoding();
        let convert = |ranges: Vec<ruff_text_size::TextRange>| {
            ranges
                .into_iter()
                .map(|range| text_range_to_lsp_range(range, &params.text, &index, encoding))
                .collect()
        };
        Ok(DiffIgnoredRanges {
            imports: convert(ignored.imports),
            formatting: convert(ignored.formatting),
        })
    }
}

impl RetriableRequestHandler for DiffIgnoredRangesHandler {}
