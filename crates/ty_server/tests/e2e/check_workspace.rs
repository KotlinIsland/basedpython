//! `by/checkWorkspace` over the wire
//!
//! the report is `workspace/diagnostic`'s, from the same check, and the pull diagnostics tests
//! cover what is in it. what these cover is when it comes back: straight away, whatever it says —
//! which is the difference between the two, since `workspace/diagnostic` holds on to a request
//! whose answer the client already has — and how it comes back

use std::time::Duration;

use anyhow::Result;
use lsp_server::ErrorCode;
use lsp_types::{
    LspRequestMethod, MessageDirection, PartialResultParams, PreviousResultId,
    ProgressNotification, ProgressToken, Request, WorkDoneProgressParams,
    WorkspaceDiagnosticParams, WorkspaceDiagnosticReport, WorkspaceDocumentDiagnosticReport,
};
use ruff_db::system::SystemPath;
use ty_server::{ClientOptions, DiagnosticMode};

use crate::pull_diagnostics::{
    assert_workspace_diagnostics_suspends_for_long_polling, send_workspace_diagnostic_request,
    shutdown_and_await_workspace_diagnostic,
};
use crate::{AwaitResponseError, TestServer, TestServerBuilder};

/// the request as a client sends it
enum CheckWorkspace {}

impl Request for CheckWorkspace {
    type Params = WorkspaceDiagnosticParams;
    type Result = WorkspaceDiagnosticReport;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/checkWorkspace");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

fn params(previous_result_ids: Vec<PreviousResultId>) -> WorkspaceDiagnosticParams {
    WorkspaceDiagnosticParams {
        identifier: None,
        previous_result_ids,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

/// asks, and waits no longer than a request that is answered straight away takes
fn check(
    server: &mut TestServer,
    previous_result_ids: Vec<PreviousResultId>,
) -> Result<WorkspaceDiagnosticReport, AwaitResponseError> {
    let id = server.send_request::<CheckWorkspace>(params(previous_result_ids));
    server.try_await_response::<CheckWorkspace>(&id, Some(Duration::from_secs(10)))
}

fn workspace_server(files: &[(&str, &str)], mode: DiagnosticMode) -> Result<TestServer> {
    let mut builder = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_initialization_options(&ClientOptions::default().with_diagnostic_mode(mode));
    for (path, content) in files {
        builder = builder.with_file(SystemPath::new(path), *content)?;
    }
    Ok(builder.build().wait_until_workspaces_are_initialized())
}

const CLEAN: &str = "def hello() -> str:\n    return \"world\"\n";
const BROKEN: &str = "def hello() -> str:\n    return 42\n";

fn result_ids(report: &WorkspaceDiagnosticReport) -> Vec<PreviousResultId> {
    report
        .items
        .iter()
        .filter_map(|item| match item {
            WorkspaceDocumentDiagnosticReport::WorkspaceFullDocumentDiagnosticReport(full) => {
                Some(PreviousResultId {
                    uri: full.uri.clone(),
                    value: full.full_document_diagnostic_report.result_id.clone()?,
                })
            }
            WorkspaceDocumentDiagnosticReport::WorkspaceUnchangedDocumentDiagnosticReport(_) => {
                None
            }
        })
        .collect()
}

/// a project with nothing wrong in it is an answer too: an empty report, not a request held open
/// until something goes wrong
#[test]
fn a_project_with_no_diagnostics_is_answered() -> Result<()> {
    let mut server = workspace_server(&[("src/clean.py", CLEAN)], DiagnosticMode::Workspace)?;

    // the long poll holds on to exactly this request
    let long_poll = send_workspace_diagnostic_request(&mut server);
    assert_workspace_diagnostics_suspends_for_long_polling(&mut server, &long_poll);

    let report = check(&mut server, Vec::new()).expect("answered straight away");
    assert!(report.items.is_empty(), "{report:#?}");

    // and the long poll still has nothing to say until the server goes away
    let long_poll_report = shutdown_and_await_workspace_diagnostic(server, &long_poll);
    assert!(long_poll_report.items.is_empty(), "{long_poll_report:#?}");

    Ok(())
}

/// a check that finds what the client already has says so, file by file, rather than waiting for
/// a change
#[test]
fn an_unchanged_workspace_is_answered_with_unchanged_reports() -> Result<()> {
    let mut server = workspace_server(&[("src/broken.py", BROKEN)], DiagnosticMode::Workspace)?;

    let first = check(&mut server, Vec::new()).expect("answered");
    let ids = result_ids(&first);
    assert_eq!(ids.len(), 1, "{first:#?}");

    let second = check(&mut server, ids).expect("answered although nothing changed");
    assert!(
        matches!(
            second.items.as_slice(),
            [WorkspaceDocumentDiagnosticReport::WorkspaceUnchangedDocumentDiagnosticReport(_)]
        ),
        "{second:#?}"
    );

    Ok(())
}

/// in the mode that checks open files only, the workspace is not checked, and an empty report
/// would say it has no diagnostics
#[test]
fn a_workspace_that_is_not_checked_is_refused() -> Result<()> {
    let mut server = workspace_server(&[("src/broken.py", BROKEN)], DiagnosticMode::OpenFilesOnly)?;

    match check(&mut server, Vec::new()) {
        Err(AwaitResponseError::RequestFailed(error)) => {
            assert_eq!(error.code, ErrorCode::RequestFailed as i32, "{error:?}");
        }
        other => panic!("expected the request to be refused, got {other:?}"),
    }

    Ok(())
}

/// the report comes whole, in the response, even when the params ask for partial results and
/// progress under the client's tokens: a check that an edit interrupts starts again, and nothing
/// sent under those tokens can be taken back
#[test]
fn the_report_comes_whole_in_the_response() -> Result<()> {
    // the server streams partial results in batches of two in tests, so three files make at
    // least one batch
    let mut server = workspace_server(
        &[
            ("src/a.py", BROKEN),
            ("src/b.py", BROKEN),
            ("src/c.py", BROKEN),
        ],
        DiagnosticMode::Workspace,
    )?;

    let id = server.send_request::<CheckWorkspace>(WorkspaceDiagnosticParams {
        work_done_progress_params: WorkDoneProgressParams {
            work_done_token: Some(ProgressToken::String("work-done".to_string())),
        },
        partial_result_params: PartialResultParams {
            partial_result_token: Some(ProgressToken::String("partial-result".to_string())),
        },
        ..params(Vec::new())
    });
    let report = server
        .try_await_response::<CheckWorkspace>(&id, Some(Duration::from_secs(10)))
        .expect("answered");
    assert_eq!(result_ids(&report).len(), 3, "{report:#?}");

    if let Ok(progress) =
        server.try_await_notification::<ProgressNotification>(Some(Duration::from_secs(1)))
    {
        panic!("expected no progress under the client's tokens, got {progress:#?}");
    }

    Ok(())
}

/// a workspace whose script is still having its environment set up is not checked yet, because
/// the script would be checked against an environment it does not have. the check is held open
/// until the environment is there, like a long poll, and these cover what happens to it
/// meanwhile
///
/// `uv` here is a stand-in that holds the setup until the test releases it
#[cfg(unix)]
mod a_script_environment_being_set_up {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    use anyhow::{Result, bail};
    use lsp_server::{ErrorCode, RequestId};
    use lsp_types::{Uri, WorkspaceDiagnosticReport, WorkspaceDiagnosticRequest};
    use ruff_db::system::{SystemPath, SystemPathBuf};
    use ty_project::UseUv;
    use ty_server::{ClientOptions, DiagnosticMode};

    use super::{CheckWorkspace, params};
    use crate::pull_diagnostics::{
        assert_workspace_diagnostics_suspends_for_long_polling, send_workspace_diagnostic_request,
    };
    use crate::{AwaitResponseError, TestServer, TestServerBuilder};

    /// run beside the script, it says it has started, waits to be released, and fails, which
    /// ends the setup with an environment that has an error rather than one that is pending
    const UV: &str = "#!/bin/sh
touch started
i=0
while [ ! -e release ] && [ \"$i\" -lt 3000 ]; do
    sleep 0.01
    i=$((i + 1))
done
exit 1
";

    /// the stand-in `uv`'s two signals, as files beside the script
    struct Setup {
        started: SystemPathBuf,
        release: SystemPathBuf,
    }

    impl Setup {
        fn wait_until_started(&self) -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !self.started.as_std_path().exists() {
                if Instant::now() > deadline {
                    bail!("the script's environment setup never started");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        }

        fn release(&self) {
            // an error here leaves the setup to the stand-in's own time limit
            let _ = fs::write(self.release.as_std_path(), "");
        }
    }

    impl Drop for Setup {
        // the server waits for the setup when it exits
        fn drop(&mut self) {
            self.release();
        }
    }

    fn server() -> Result<(TestServer, Setup)> {
        let builder = TestServerBuilder::new()?
            .with_workspace(SystemPath::new("src"), None)?
            .with_initialization_options(
                &ClientOptions::default().with_diagnostic_mode(DiagnosticMode::Workspace),
            )
            .with_use_uv(UseUv::Scripts)
            .with_file("src/main.py", "missing\n")?
            .with_file(
                "src/script.py",
                "# /// script\n# dependencies = []\n# ///\n",
            )?
            .with_file("bin/uv", UV)?;
        let uv = builder.file_path("bin/uv");
        fs::set_permissions(uv.as_std_path(), fs::Permissions::from_mode(0o755))?;
        let setup = Setup {
            started: builder.file_path("src/started"),
            release: builder.file_path("src/release"),
        };
        let server = builder
            .with_env_var("UV", uv.as_str())
            .build()
            .wait_until_workspaces_are_initialized();
        setup.wait_until_started()?;
        Ok((server, setup))
    }

    fn answer<R: lsp_types::Request<Result = WorkspaceDiagnosticReport>>(
        server: &mut TestServer,
        id: &RequestId,
    ) -> WorkspaceDiagnosticReport {
        server
            .try_await_response::<R>(id, Some(Duration::from_secs(10)))
            .unwrap_or_else(|err| panic!("request {id} was not answered: {err}"))
    }

    fn reports_on(report: &WorkspaceDiagnosticReport, uri: &Uri) -> bool {
        report.items.iter().any(|item| match item {
            lsp_types::WorkspaceDocumentDiagnosticReport::WorkspaceFullDocumentDiagnosticReport(
                full,
            ) => &full.uri == uri,
            lsp_types::WorkspaceDocumentDiagnosticReport::WorkspaceUnchangedDocumentDiagnosticReport(
                unchanged,
            ) => &unchanged.uri == uri,
        })
    }

    fn assert_refused(server: &mut TestServer, id: &RequestId, code: ErrorCode) {
        match server.try_await_response::<CheckWorkspace>(id, Some(Duration::from_secs(10))) {
            Err(AwaitResponseError::RequestFailed(error)) => {
                assert_eq!(error.code, code as i32, "{error:?}");
            }
            other => panic!("expected the check to be refused with {code:?}, got {other:?}"),
        }
    }

    #[track_caller]
    fn assert_held(server: &mut TestServer, id: &RequestId) {
        match server.try_await_response::<CheckWorkspace>(id, Some(Duration::from_secs(2))) {
            Err(AwaitResponseError::Timeout) => {}
            other => panic!("expected the check to be held open, got {other:?}"),
        }
    }

    fn shutdown(server: &mut TestServer) -> RequestId {
        server.send_request::<lsp_types::ShutdownRequest>(())
    }

    fn exit(server: &mut TestServer, shutdown: &RequestId) {
        server.await_response::<lsp_types::ShutdownRequest>(shutdown);
        server.send_notification::<lsp_types::ExitNotification>(());
    }

    /// the check is answered once the environment is there, with the whole workspace checked
    #[test]
    fn a_check_is_answered_once_the_environment_is_there() -> Result<()> {
        let (mut server, setup) = server()?;

        let check = server.send_request::<CheckWorkspace>(params(Vec::new()));
        assert_held(&mut server, &check);

        setup.release();
        let report = answer::<CheckWorkspace>(&mut server, &check);
        assert!(
            reports_on(&report, &server.file_uri("src/main.py")),
            "{report:#?}"
        );

        Ok(())
    }

    /// a long poll waits for the environment too, and holding the one does not drop the other
    #[test]
    fn a_held_check_and_a_held_long_poll_are_both_answered() -> Result<()> {
        let (mut server, setup) = server()?;

        let long_poll = send_workspace_diagnostic_request(&mut server);
        assert_workspace_diagnostics_suspends_for_long_polling(&mut server, &long_poll);
        let check = server.send_request::<CheckWorkspace>(params(Vec::new()));
        assert_held(&mut server, &check);

        setup.release();
        let main = server.file_uri("src/main.py");
        let report = answer::<WorkspaceDiagnosticRequest>(&mut server, &long_poll);
        assert!(reports_on(&report, &main), "{report:#?}");
        let report = answer::<CheckWorkspace>(&mut server, &check);
        assert!(reports_on(&report, &main), "{report:#?}");

        Ok(())
    }

    /// at shutdown the long poll is answered with an empty report, since nothing changed, but the
    /// check is refused: an empty report would say the workspace has no diagnostics, and it was
    /// never checked
    #[test]
    fn a_check_held_at_shutdown_is_refused() -> Result<()> {
        let (mut server, _setup) = server()?;

        let long_poll = send_workspace_diagnostic_request(&mut server);
        assert_workspace_diagnostics_suspends_for_long_polling(&mut server, &long_poll);
        let check = server.send_request::<CheckWorkspace>(params(Vec::new()));
        assert_held(&mut server, &check);

        let shutdown = shutdown(&mut server);
        let report = answer::<WorkspaceDiagnosticRequest>(&mut server, &long_poll);
        assert!(report.items.is_empty(), "{report:#?}");
        assert_refused(&mut server, &check, ErrorCode::RequestFailed);
        exit(&mut server, &shutdown);

        Ok(())
    }

    /// a check that has not been held yet when the `shutdown` request comes, because it is still
    /// finding out that the workspace is not ready, is refused as a held one is rather than held
    /// by a server that will not run it again
    #[test]
    fn a_check_still_running_at_shutdown_is_refused() -> Result<()> {
        let (mut server, _setup) = server()?;

        let check = server.send_request::<CheckWorkspace>(params(Vec::new()));
        let shutdown = shutdown(&mut server);
        assert_refused(&mut server, &check, ErrorCode::RequestFailed);
        exit(&mut server, &shutdown);

        Ok(())
    }

    /// a held check the client cancels is answered as cancelled, once, and is not run again when
    /// the environment is there
    #[test]
    fn a_held_check_can_be_cancelled() -> Result<()> {
        let (mut server, setup) = server()?;

        let check = server.send_request::<CheckWorkspace>(params(Vec::new()));
        assert_held(&mut server, &check);
        server.cancel(&check);
        assert_refused(&mut server, &check, ErrorCode::RequestCanceled);

        // a second check is answered once the environment is there, by which time the cancelled
        // one would have been run again too. the test server's drop asserts that no other
        // response came
        setup.release();
        let second = server.send_request::<CheckWorkspace>(params(Vec::new()));
        answer::<CheckWorkspace>(&mut server, &second);

        Ok(())
    }
}
