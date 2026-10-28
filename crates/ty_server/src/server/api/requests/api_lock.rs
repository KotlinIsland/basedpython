//! `by/apiLock`: a project's `api.lock` as the server generates it
//!
//! the lockfile is generated from the project as the server reads it, unsaved edits included,
//! and incrementally: what an edit did not change, the server does not compute again. it is
//! generated for the target the `#python:` header of the project's `api.lock` names, so that a
//! lockfile kept for the lowest python a library supports is checked, and regenerated, for that
//! python. see [`ty_python_semantic::api_lockfile`] for the format

use lsp_types::{LspRequestMethod, MessageDirection, Request, Uri};
use ruff_db::Db as _;
use ruff_db::system::SystemPathBuf;
use ruff_python_ast::PythonVersionDeserializationError;
use ty_project::api_lock::{api_lock_target, read_api_lock};
use ty_project::{Db as _, ProjectDatabase};
use ty_python_semantic::api_lockfile::{ApiLockTarget, first_stale_line};

use crate::server::api::Error;
use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;

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
