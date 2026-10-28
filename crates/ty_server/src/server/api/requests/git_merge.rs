//! `by/gitMergeIntegration`: whether git merges a project with `by merge-driver`, and setting
//! that up, as `by init --git-merge` does, for an editor that offers it
//!
//! see [`ty_project::git_merge`] for what the integration is

use lsp_types::{LspRequestMethod, MessageDirection, Request, Uri};
use ruff_db::Db as _;
use ruff_db::system::{OsSystem, SystemPathBuf};
use ty_project::Db as _;
use ty_project::git_merge::{GitMergeIntegration, invoked_executable};

use crate::server::api::Error;
use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;

pub(crate) enum GitMergeIntegrationRequest {}

impl Request for GitMergeIntegrationRequest {
    type Params = GitMergeIntegrationParams;
    type Result = Option<GitMergeIntegrationState>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/gitMergeIntegration");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GitMergeIntegrationParams {
    /// a file or directory of the project asked about; without one, the first project
    #[serde(default)]
    uri: Option<Uri>,
    /// set the integration up before answering
    #[serde(default)]
    enable: bool,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the answer on the wire, each flag a separate thing an editor shows"
)]
pub(crate) struct GitMergeIntegrationState {
    /// whether git merges every path the driver is for with a driver that is there
    enabled: bool,
    /// whether the project is in a git work tree
    repository: bool,
    /// the version of the git on the language server's `PATH`, as `git version` writes it
    git_version: String,
    /// whether that git is one the driver can merge with, 2.44 or later
    git_supported: bool,
    /// the project's `.gitattributes`, whether or not it is there
    attributes: Uri,
    /// the patterns `.gitattributes` lacks a `merge=by` line for, as git reads it
    missing_attributes: Vec<String>,
    /// the command `merge.by.driver` runs, where it is set
    driver: Option<String>,
    /// whether the `by` that command runs is there
    driver_found: bool,
    /// whether the command is the one setting the integration up writes, with its fallback to
    /// git's own line merge where `by` does not merge the file
    driver_current: bool,
}

pub(crate) struct GitMergeIntegrationHandler;

impl RequestHandler for GitMergeIntegrationHandler {
    type RequestType = GitMergeIntegrationRequest;
}

impl BackgroundRequestHandler for GitMergeIntegrationHandler {
    /// `null` without a project. setting up a project that is not in a git work tree fails
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        params: GitMergeIntegrationParams,
    ) -> crate::server::Result<Option<GitMergeIntegrationState>> {
        let path = params
            .uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok())
            .and_then(|path| SystemPathBuf::from_path_buf(path).ok());
        let Some(db) = snapshot.project_for_path(path.as_deref()) else {
            return Ok(None);
        };
        let root = db.project().root(db);
        // the file system itself rather than the editor's view of it: git reads and writes
        // `.gitattributes` and its configuration on disk
        let system = OsSystem::new(root);
        let failed = |error: anyhow::Error| Error {
            code: lsp_server::ErrorCode::RequestFailed,
            error,
        };
        let integration = if params.enable {
            // the directory the language server was started in, which `argv[0]` is relative to
            let by =
                invoked_executable(&system, db.system().current_directory()).map_err(failed)?;
            GitMergeIntegration::enable(&system, root, &by)
        } else {
            GitMergeIntegration::of(&system, root)
        }
        .map_err(failed)?;
        let attributes =
            Uri::from_file_path(integration.attributes_path.as_std_path()).map_err(|()| {
                failed(anyhow::anyhow!(
                    "`{}` cannot be written as a uri",
                    integration.attributes_path
                ))
            })?;
        Ok(Some(GitMergeIntegrationState {
            enabled: integration.is_enabled(),
            repository: integration.repository,
            git_supported: integration.git_supported(),
            git_version: integration.git_version,
            attributes,
            missing_attributes: integration
                .missing_attributes
                .iter()
                .map(ToString::to_string)
                .collect(),
            driver: integration.driver,
            driver_found: integration.driver_found,
            driver_current: integration.driver_current,
        }))
    }
}

impl RetriableRequestHandler for GitMergeIntegrationHandler {}
