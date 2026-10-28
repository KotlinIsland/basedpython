//! `by/resolveMergeConflicts`, `by/reconcileMergeImports` and `by/diffIgnoredRanges`: what an
//! editor's merge and diff tools ask a language about the texts they compare
//!
//! each takes texts rather than documents: a merge's base, ours and theirs, and a diff's two sides,
//! are versions no file on disk holds. a `uri`, where one is given, names the file the texts are
//! versions of, which says how they are parsed and which isort configuration sorts their imports.
//! see [`ty_ide::resolve_file_merge_conflicts`], [`ty_ide::reconcile_file_merge_imports`] and
//! [`ty_ide::diff_ignored_ranges`] for what is answered

use lsp_types::{LspRequestMethod, MessageDirection, Position, Range, Request, TextEdit, Uri};
use ruff_db::system::SystemPathBuf;
use ruff_python_ast::PySourceType;
use ruff_source_file::LineIndex;
use ruff_text_size::TextRange;
use ty_ide::{
    ConflictLines, MergeTexts, MergeVersion, ReconciledImports, diff_ignored_ranges,
    reconcile_file_merge_imports, resolve_file_merge_conflicts,
};

use crate::PositionEncoding;
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
        let path = path_of(params.uri.as_ref());
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
        let Some(db) = snapshot.project_for_path(path.as_deref()) else {
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

fn path_of(uri: Option<&Uri>) -> Option<SystemPathBuf> {
    uri.and_then(|uri| uri.to_file_path().ok())
        .and_then(|path| SystemPathBuf::from_path_buf(path).ok())
}

pub(crate) enum ReconcileMergeImportsRequest {}

impl Request for ReconcileMergeImportsRequest {
    type Params = ReconcileMergeImportsParams;
    type Result = ReconciledMergeImports;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/reconcileMergeImports");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReconcileMergeImportsParams {
    base: String,
    ours: String,
    theirs: String,
    /// the file the merge wrote, which may still have conflicts in markers of `marker_size`
    merged: String,
    /// the size of the conflict markers in `merged`, git's 7 by default
    #[serde(default = "default_marker_size")]
    marker_size: usize,
    #[serde(default)]
    uri: Option<Uri>,
}

const fn default_marker_size() -> usize {
    7
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReconciledMergeImports {
    /// the edits to `merged` that reconcile its imports, in order
    edits: Vec<TextEdit>,
    /// where the versions import a name from different places, which there is to choose, and
    /// then no edits
    choice: Option<ImportChoice>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportChoice {
    name: String,
    /// each version's import statements binding the name, ours, base and theirs in that order
    imports: Vec<VersionImports>,
    /// where in `merged` the import belongs
    position: Position,
    /// the lines of `merged`'s import statements that bind the name and nothing else, which
    /// whatever is chosen replaces
    bindings: Vec<Range>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VersionImports {
    version: Version,
    statements: Vec<String>,
}

/// one of a merge's three versions, `ours`, `base` or `theirs` on the wire
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Version {
    Ours,
    Base,
    Theirs,
}

impl From<MergeVersion> for Version {
    fn from(version: MergeVersion) -> Self {
        match version {
            MergeVersion::Ours => Self::Ours,
            MergeVersion::Base => Self::Base,
            MergeVersion::Theirs => Self::Theirs,
        }
    }
}

pub(crate) struct ReconcileMergeImportsHandler;

impl RequestHandler for ReconcileMergeImportsHandler {
    type RequestType = ReconcileMergeImportsRequest;
}

impl BackgroundRequestHandler for ReconcileMergeImportsHandler {
    /// no edits and no choice without a project to read the configuration of
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        params: ReconcileMergeImportsParams,
    ) -> crate::server::Result<ReconciledMergeImports> {
        let path = path_of(params.uri.as_ref());
        let Some(db) = snapshot.project_for_path(path.as_deref()) else {
            return Ok(ReconciledMergeImports {
                edits: Vec::new(),
                choice: None,
            });
        };
        let texts = MergeTexts {
            base: &params.base,
            ours: &params.ours,
            theirs: &params.theirs,
        };
        let merged = &params.merged;
        let index = LineIndex::from_source_text(merged);
        let encoding: PositionEncoding = snapshot.position_encoding();
        let range = |range: TextRange| text_range_to_lsp_range(range, merged, &index, encoding);
        Ok(
            match reconcile_file_merge_imports(
                db,
                path.as_deref(),
                texts,
                merged,
                params.marker_size,
            ) {
                ReconciledImports::Edits(edits) => ReconciledMergeImports {
                    edits: edits
                        .into_iter()
                        .map(|edit| TextEdit {
                            range: range(edit.range),
                            new_text: edit.replacement,
                        })
                        .collect(),
                    choice: None,
                },
                ReconciledImports::Refused(choice) => ReconciledMergeImports {
                    edits: Vec::new(),
                    choice: Some(ImportChoice {
                        name: choice.name,
                        imports: choice
                            .imports
                            .into_iter()
                            .map(|(version, statements)| VersionImports {
                                version: version.into(),
                                statements,
                            })
                            .collect(),
                        position: range(TextRange::empty(choice.offset)).start,
                        bindings: choice.bindings.into_iter().map(range).collect(),
                    }),
                },
            },
        )
    }
}

impl RetriableRequestHandler for ReconcileMergeImportsHandler {}

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
