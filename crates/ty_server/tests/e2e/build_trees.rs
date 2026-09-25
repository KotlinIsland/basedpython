//! a tree `by build` wrote is not part of the project it was built from
//!
//! the output is a copy of the project (a `.py` for every `.by`, every hand-written `.py` carried
//! over), so checking it with the workspace would check everything twice and report every
//! diagnostic a second time, against a file nobody edits. it is recognised by the manifest the
//! build leaves at its top, wherever the build wrote it, and not by its name

use anyhow::Result;
use lsp_types::{WorkspaceDiagnosticReport, WorkspaceDocumentDiagnosticReport};
use ruff_db::system::SystemPath;
use ty_server::{ClientOptions, DiagnosticMode};

use crate::{TestServer, TestServerBuilder};

const ERROR: &str = "x: int = \"\"\n";
const MANIFEST: &str = "# written by `by build`; delete it and stale output stays\napp.py\n";

/// the paths, relative to the workspace, of the files the report has a diagnostic for
fn reported_paths(server: &TestServer, report: &WorkspaceDiagnosticReport) -> Vec<String> {
    let root = server.file_path("project");
    let mut paths: Vec<String> = report
        .items
        .iter()
        .filter_map(|item| match item {
            WorkspaceDocumentDiagnosticReport::WorkspaceFullDocumentDiagnosticReport(full)
                if !full.full_document_diagnostic_report.items.is_empty() =>
            {
                let path = full.uri.to_file_path().ok()?;
                let path = SystemPath::from_std_path(&path)?.to_path_buf();
                Some(path.strip_prefix(&root).ok()?.as_str().replace('\\', "/"))
            }
            _ => None,
        })
        .collect();
    paths.sort();
    paths
}

fn workspace_server(builder: TestServerBuilder) -> Result<TestServer> {
    Ok(builder
        .with_workspace(SystemPath::new("project"), None)?
        .with_initialization_options(
            &ClientOptions::default().with_diagnostic_mode(DiagnosticMode::Workspace),
        )
        .build()
        .wait_until_workspaces_are_initialized())
}

/// the default output, `build/`, holds a copy of the module with the error. only the source is
/// reported
#[test]
fn the_workspace_does_not_include_the_build_output() -> Result<()> {
    let mut server = workspace_server(
        TestServerBuilder::new()?
            .with_file("project/app.py", ERROR)?
            .with_file("project/build/.by-manifest", MANIFEST)?
            .with_file("project/build/app.py", ERROR)?,
    )?;

    let report = server.workspace_diagnostic_request(None, None);
    assert_eq!(reported_paths(&server, &report), ["app.py"]);

    Ok(())
}

/// a build told to write elsewhere is left out wherever it wrote, and a directory of the author's
/// own called `build`, with no manifest in it, stays part of the project
#[test]
fn the_output_is_recognised_by_its_manifest_not_its_name() -> Result<()> {
    let mut server = workspace_server(
        TestServerBuilder::new()?
            .with_file("project/app.py", ERROR)?
            .with_file("project/build/tool.py", ERROR)?
            .with_file("project/elsewhere/.by-manifest", MANIFEST)?
            .with_file("project/elsewhere/app.py", ERROR)?,
    )?;

    let report = server.workspace_diagnostic_request(None, None);
    assert_eq!(
        reported_paths(&server, &report),
        ["app.py", "build/tool.py"]
    );

    Ok(())
}

/// pulls the workspace's diagnostics each time the server says something changed, until `done`
fn reported_after_change(server: &mut TestServer, done: impl Fn(&[String]) -> bool) -> Vec<String> {
    loop {
        server.await_diagnostic_refresh();
        let report = server.workspace_diagnostic_request(None, None);
        let paths = reported_paths(server, &report);
        if done(&paths) {
            return paths;
        }
    }
}

/// a build run while the server watches: the tree it writes does not join the project, and a
/// directory that becomes an output when a manifest appears in it leaves the project
///
/// these read real file system events, like the other watching tests
#[test]
fn a_build_written_while_the_server_watches_does_not_join_the_project() -> Result<()> {
    let mut server = workspace_server(
        TestServerBuilder::new()?
            .with_file("project/app.py", ERROR)?
            .with_file("project/elsewhere/app.py", ERROR)?
            .enable_pull_diagnostics(true)
            .enable_workspace_diagnostic_refresh(true)
            .watch_file_system(),
    )?;
    let report = server.workspace_diagnostic_request(None, None);
    assert_eq!(
        reported_paths(&server, &report),
        ["app.py", "elsewhere/app.py"],
        "a directory with no manifest is part of the project"
    );

    // the build marks its output before the first file lands in it
    server.write_file("project/build/.by-manifest", MANIFEST)?;
    server.write_file("project/build/app.py", ERROR)?;
    // a write the server has to see after the build's, so that once it is reported the build's
    // writes have been taken in too
    server.write_file("project/later.py", ERROR)?;
    let paths = reported_after_change(&mut server, |paths| {
        paths.iter().any(|path| path == "later.py")
    });
    assert_eq!(paths, ["app.py", "elsewhere/app.py", "later.py"]);

    // a directory already in the project leaves it when a build claims it
    server.write_file("project/elsewhere/.by-manifest", MANIFEST)?;
    let paths = reported_after_change(&mut server, |paths| {
        !paths.iter().any(|path| path == "elsewhere/app.py")
    });
    assert_eq!(paths, ["app.py", "later.py"]);

    Ok(())
}
