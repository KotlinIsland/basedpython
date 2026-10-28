//! `by/apiLock`, and what the server says about a project's `api.lock` as a document: whether it
//! is stale, and the edit that regenerates it
//!
//! the lockfile is generated from the project as the server reads it, unsaved edits included,
//! and incrementally: what an edit did not change, the server does not compute again. it is
//! generated for the target the `#python:` header of the project's `api.lock` names, so that a
//! lockfile kept for the lowest python a library supports is checked, and regenerated, for that
//! python. see [`ty_python_semantic::api_lockfile`] for the format

use lsp_types::{
    Code, Diagnostic, DiagnosticSeverity, LspRequestMethod, MessageDirection, Range, Request,
    TextEdit, Uri,
};
use ruff_db::Db as _;
use ruff_db::system::{SystemPath, SystemPathBuf};
use ruff_python_ast::PythonVersionDeserializationError;
use ruff_source_file::LineIndex;
use ruff_text_size::{TextRange, TextSize};
use ty_project::api_lock::{api_lock_target, read_api_lock};
use ty_project::{Db as _, ProjectDatabase};
use ty_python_semantic::api_lockfile::{ApiLockTarget, first_stale_line};

use crate::DIAGNOSTIC_NAME;
use crate::PositionEncoding;
use crate::document::text_range_to_lsp_range;
use crate::server::api::Error;
use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;
use crate::system::AnySystemPath;

/// the code of the diagnostic on a stale `api.lock`
pub(crate) const STALE_API_LOCK: &str = "stale-api-lock";

/// the code of the diagnostic on an `api.lock` the server cannot check, being unable to read it
/// or the python it names
const UNREADABLE_API_LOCK: &str = "unreadable-api-lock";

/// the title of the quick fix that regenerates a stale `api.lock`
const REGENERATE: &str = "Regenerate api.lock";

pub(crate) enum ApiLockRequest {}

impl Request for ApiLockRequest {
    type Params = ApiLockParams;
    type Result = Option<ApiLock>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/apiLock");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ApiLockParams {
    /// a file or directory of the project whose lockfile is asked for, the one the server
    /// analyses it in; without one, the first project's
    #[serde(default)]
    uri: Option<Uri>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApiLock {
    /// where the project keeps its lockfile, whether or not it is there
    uri: Uri,
    /// the lockfile, as generated from the project as the server reads it now
    text: String,
    /// the target `text` is generated for, as its `#python:` header names it: the one the
    /// project's `api.lock` names, or `default` without one
    python: String,
    /// whether the project has an `api.lock`, as the server reads it: the client's buffer where
    /// it has the file open
    exists: bool,
    /// the line, 0-based, at which the project's `api.lock` first says something other than
    /// `text`, or `null` where it is up to date or there is none
    stale_line: Option<usize>,
}

pub(crate) struct ApiLockHandler;

impl RequestHandler for ApiLockHandler {
    type RequestType = ApiLockRequest;
}

impl BackgroundRequestHandler for ApiLockHandler {
    /// `null` without a project
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        params: ApiLockParams,
    ) -> crate::server::Result<Option<ApiLock>> {
        let path = params
            .uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok())
            .and_then(|path| SystemPathBuf::from_path_buf(path).ok());
        let Some(db) = snapshot.project_for_path(path.as_deref()) else {
            return Ok(None);
        };
        let state = ApiLockState::of(db).map_err(|error| Error {
            code: lsp_server::ErrorCode::RequestFailed,
            error: anyhow::anyhow!("{error}"),
        })?;
        let uri = Uri::from_file_path(state.path.as_std_path()).map_err(|()| Error {
            code: lsp_server::ErrorCode::InternalError,
            error: anyhow::anyhow!("`{}` cannot be written as a uri", state.path),
        })?;
        Ok(Some(ApiLock {
            uri,
            python: state.target.to_string(),
            exists: state.committed.is_some(),
            stale_line: state.stale_line(),
            text: state.generated.to_string(),
        }))
    }
}

impl RetriableRequestHandler for ApiLockHandler {}

/// a project's `api.lock` as the server reads it, beside the lockfile the server generates for it
struct ApiLockState<'db> {
    path: SystemPathBuf,
    /// the lockfile the project has, `None` where it has none
    committed: Option<String>,
    target: ApiLockTarget,
    generated: &'db str,
}

impl<'db> ApiLockState<'db> {
    /// refused where the project's lockfile cannot be read, or names a python version `by`
    /// cannot read: what it is to be generated for is not known
    fn of(db: &'db ProjectDatabase) -> Result<Self, UnreadableApiLock> {
        let path = db.project().api_lock_path(db);
        let committed =
            read_api_lock(db.system(), &path).map_err(|error| UnreadableApiLock::Unreadable {
                path: path.clone(),
                error,
            })?;
        let target = match api_lock_target(committed.as_deref()) {
            Ok(target) => target,
            Err(error) => {
                return Err(UnreadableApiLock::UnknownPython {
                    path,
                    committed: committed.unwrap_or_default(),
                    error,
                });
            }
        };
        let generated = db.project().api_lock(db, target);
        Ok(Self {
            path,
            committed,
            target,
            generated,
        })
    }

    fn stale_line(&self) -> Option<usize> {
        first_stale_line(self.committed.as_deref()?, self.generated)
    }
}

/// a project's `api.lock` the server cannot check: what it is to be generated for is not known
#[derive(Debug, thiserror::Error)]
enum UnreadableApiLock {
    /// it is there, but cannot be read, not being utf-8 among other reasons
    #[error("`{path}` cannot be read: {error}")]
    Unreadable {
        path: SystemPathBuf,
        error: std::io::Error,
    },
    /// it names a python version `by` cannot read
    #[error("`{path}` names a python version by cannot read: {error}")]
    UnknownPython {
        path: SystemPathBuf,
        committed: String,
        error: PythonVersionDeserializationError,
    },
}

/// whether the document at `path` is the `api.lock` of `db`'s project
pub(crate) fn is_api_lock(db: &ProjectDatabase, path: &SystemPath) -> bool {
    db.project().api_lock_path(db).as_path() == path
}

/// the diagnostics of the document at `path` where it is the `api.lock` of `db`'s project, as
/// [`api_lock_diagnostics`] has them, and `None` for any other document
pub(crate) fn api_lock_document_diagnostics(
    db: &ProjectDatabase,
    path: &AnySystemPath,
    encoding: PositionEncoding,
) -> Option<Vec<Diagnostic>> {
    let path = path.as_system()?;
    is_api_lock(db, path).then(|| api_lock_diagnostics(db, encoding))
}

/// the diagnostics of the project's `api.lock`: one on the line at which it is first stale, one
/// where it cannot be checked, on the `#python:` header naming a python version `by` cannot read,
/// or none
fn api_lock_diagnostics(db: &ProjectDatabase, encoding: PositionEncoding) -> Vec<Diagnostic> {
    let (range, code, message) = match ApiLockState::of(db) {
        Ok(state) => {
            let (Some(committed), Some(line)) = (&state.committed, state.stale_line()) else {
                return Vec::new();
            };
            (
                lsp_line_range(committed, line, encoding),
                STALE_API_LOCK,
                format!(
                    "`api.lock` is stale: the api of the project, for python `{}`, differs from \
                    here",
                    state.target
                ),
            )
        }
        Err(UnreadableApiLock::Unreadable { error, .. }) => (
            Range::default(),
            UNREADABLE_API_LOCK,
            format!("`api.lock` cannot be read: {error}"),
        ),
        Err(UnreadableApiLock::UnknownPython {
            committed, error, ..
        }) => (
            ApiLockTarget::header_line(&committed)
                .map(|line| lsp_line_range(&committed, line, encoding))
                .unwrap_or_default(),
            UNREADABLE_API_LOCK,
            format!("`api.lock` names a python version by cannot read: {error}"),
        ),
    };
    vec![Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::Warning),
        code: Some(Code::String(code.to_string())),
        code_description: None,
        source: Some(DIAGNOSTIC_NAME.to_string()),
        message: message.into(),
        related_information: None,
        tags: None,
        data: None,
    }]
}

/// line `line` of `text`, 0-based, without its line ending, as a range in `encoding`
fn lsp_line_range(text: &str, line: usize, encoding: PositionEncoding) -> Range {
    let index = LineIndex::from_source_text(text);
    text_range_to_lsp_range(line_range(text, &index, line), text, &index, encoding)
}

/// the edit that rewrites the project's `api.lock` as the server generates it, or `None` where
/// it is up to date
pub(crate) fn regenerate_api_lock(
    db: &ProjectDatabase,
    encoding: PositionEncoding,
) -> Option<(&'static str, TextEdit)> {
    let state = ApiLockState::of(db).ok()?;
    let committed = state.committed.as_deref()?;
    state.stale_line()?;
    let index = LineIndex::from_source_text(committed);
    let whole = TextRange::up_to(TextSize::of(committed));
    let range: Range = text_range_to_lsp_range(whole, committed, &index, encoding);
    Some((
        REGENERATE,
        TextEdit {
            range,
            new_text: state.generated.to_string(),
        },
    ))
}

/// line `line` of `text`, 0-based, without its line ending: the end of the text for the line
/// after the last
fn line_range(text: &str, index: &LineIndex, line: usize) -> TextRange {
    let end_of_text = TextSize::of(text);
    let Some(start) = index.line_starts().get(line).copied() else {
        return TextRange::empty(end_of_text);
    };
    let end = index
        .line_starts()
        .get(line + 1)
        .copied()
        .unwrap_or(end_of_text);
    let content = text[TextRange::new(start, end)].trim_end_matches(['\n', '\r']);
    TextRange::at(start, TextSize::of(content))
}
