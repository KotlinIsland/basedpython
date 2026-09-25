//! `by/checkWorkspace` — the workspace's diagnostics, answered once they are checked
//!
//! the same report as `workspace/diagnostic`, from the same check, for a client that needs the
//! answer rather than the next change. `workspace/diagnostic` is long-polled: when nothing differs
//! from the result ids the client sent, the request is held open until something does. that is
//! what an editor keeping a problems list current wants, and it leaves a client that asks once —
//! an inspection run over the whole project, a batch run with no editor at all — waiting forever
//! on a project with no diagnostics, because "nothing" is never different from nothing. this one
//! answers as soon as the check is done, unchanged or not
//!
//! the one thing it still waits for is the workspace being checkable: while a script's initial
//! environment is being set up it is held open the way a long poll is, and answered once the
//! environment is there

use anyhow::anyhow;
use lsp_server::{ErrorCode, RequestId};
use lsp_types::{
    LspRequestMethod, MessageDirection, Request, WorkspaceDiagnosticParams,
    WorkspaceDiagnosticReport, WorkspaceDiagnosticRequest,
};
use serde_json::json;

use crate::server::Action;
use crate::server::api::Error;
use crate::server::api::requests::workspace_diagnostic::{WorkspaceCheck, check_workspace};
use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::client::Client;
use crate::session::{SessionSnapshot, SuspendedWorkspaceDiagnosticRequest};

/// the request a client sends to have the workspace checked and be told the result
pub(crate) enum CheckWorkspaceRequest {}

impl Request for CheckWorkspaceRequest {
    type Params = WorkspaceDiagnosticParams;
    type Result = WorkspaceDiagnosticReport;
    // not a method LSP defines, so it goes across as a custom one, under the same `by/` prefix as
    // the rest of them
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/checkWorkspace");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

pub(crate) struct CheckWorkspaceRequestHandler;

impl RequestHandler for CheckWorkspaceRequestHandler {
    type RequestType = CheckWorkspaceRequest;
}

impl BackgroundRequestHandler for CheckWorkspaceRequestHandler {
    // the request is answered by `handle_request`, which holds the request open while the
    // workspace is not ready rather than refuse it. this is the answer on its own, without the
    // holding
    fn run(
        snapshot: &SessionSnapshot,
        client: &Client,
        params: WorkspaceDiagnosticParams,
    ) -> crate::server::Result<WorkspaceDiagnosticReport> {
        match check_workspace(snapshot, client, answered_whole(params)) {
            WorkspaceCheck::Checked(report) => Ok(report),
            // an answer that would be different a moment later, which is what `ContentModified`
            // says
            WorkspaceCheck::NotReady => Err(Error::new(
                anyhow!("the workspace is not ready to be checked"),
                ErrorCode::ContentModified,
            )),
            WorkspaceCheck::Disabled => Err(not_checked()),
        }
    }

    fn handle_request(
        id: &RequestId,
        snapshot: SessionSnapshot,
        client: &Client,
        params: WorkspaceDiagnosticParams,
    ) {
        let params = answered_whole(params);
        match check_workspace(&snapshot, client, params.clone()) {
            WorkspaceCheck::Checked(report) => client.respond(id, Ok(report)),
            WorkspaceCheck::NotReady => {
                tracing::debug!("Holding `by/checkWorkspace` open until the workspace is ready");
                client.queue_action(Action::SuspendWorkspaceDiagnostics(Box::new(
                    SuspendedWorkspaceDiagnosticRequest {
                        id: id.clone(),
                        kind: SuspendedWorkspaceRequestKind::CheckWorkspace,
                        params: json!(&params),
                        revision: snapshot.revision(),
                    },
                )));
            }
            WorkspaceCheck::Disabled => {
                client.respond::<WorkspaceDiagnosticReport>(id, Err(not_checked()));
            }
        }
    }
}

impl RetriableRequestHandler for CheckWorkspaceRequestHandler {
    // an edit that lands mid-check cancels it; the client asked for the answer, not for the
    // chance of one, so the check starts again on the edited workspace rather than telling the
    // client to ask again
    const RETRY_ON_CANCELLATION: bool = true;
}

/// `params` without the tokens that would have the check report anything but its answer
///
/// a check that an edit cancels starts again, so anything sent under the client's tokens before
/// the cancellation would be sent a second time: a second first batch of partial results, with
/// reports for the same files from before the edit, and a second start of progress on a token
/// whose progress has ended. so the report comes whole, in the response, and progress, where
/// the client supports it, under a token the server creates for each attempt
fn answered_whole(mut params: WorkspaceDiagnosticParams) -> WorkspaceDiagnosticParams {
    params.partial_result_params.partial_result_token = None;
    params.work_done_progress_params.work_done_token = None;
    params
}

/// an empty report would say the workspace has no diagnostics, which nobody checked
fn not_checked() -> Error {
    Error::new(
        anyhow!("the workspace is not checked: the diagnostic mode is not `workspace`"),
        ErrorCode::RequestFailed,
    )
}

/// which of the two requests that wait on the workspace a held one is
#[derive(Debug, Clone, Copy)]
pub(crate) enum SuspendedWorkspaceRequestKind {
    /// `workspace/diagnostic`, held until something differs from what the client has
    LongPoll,

    /// `by/checkWorkspace`, held until the workspace can be checked
    CheckWorkspace,
}

impl SuspendedWorkspaceRequestKind {
    /// the method the request is run again as
    pub(crate) fn method(self) -> String {
        match self {
            Self::LongPoll => WorkspaceDiagnosticRequest::METHOD.to_string(),
            Self::CheckWorkspace => CheckWorkspaceRequest::METHOD.to_string(),
        }
    }

    /// answers the held request `id` when the server shuts down before it could be run again
    pub(crate) fn answer_at_shutdown(self, id: &RequestId, client: &Client) {
        match self {
            // an empty report tells the client nothing changed, which is so
            Self::LongPoll => client.respond(id, Ok(WorkspaceDiagnosticReport::default())),
            // `by/checkWorkspace` promises the workspace's diagnostics, and an empty report would
            // say it has none. it was never checked, so it is not answered as if it was
            Self::CheckWorkspace => client.respond::<WorkspaceDiagnosticReport>(
                id,
                Err(Error::new(
                    anyhow!("the server shut down before the workspace could be checked"),
                    ErrorCode::RequestFailed,
                )),
            ),
        }
    }
}
