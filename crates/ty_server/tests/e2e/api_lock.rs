//! a project's `api.lock` over the wire: `by/apiLock`, the diagnostic on a stale lockfile and
//! the quick fix that regenerates it

use anyhow::Result;
use lsp_server::ErrorCode;
use lsp_types::{
    CodeActionRequest, Diagnostic, DocumentDiagnosticReport, LanguageKind, LspRequestMethod,
    MessageDirection, Position, PublishDiagnosticsNotification, PublishDiagnosticsParams, Range,
    Request, TextDocumentContentChangeEvent, TextDocumentContentChangeWholeDocument,
};
use ruff_db::system::SystemPath;
use serde_json::{Value, json};

use crate::{AwaitResponseError, TestServer, TestServerBuilder};

enum ApiLock {}

impl Request for ApiLock {
    type Params = Value;
    type Result = Value;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/apiLock");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// a module whose public api depends on the python it is read for
const VERSIONED: &str = "\
import sys

if sys.version_info >= (3, 11):
    X: int = 1
else:
    X: str = \"\"
";

#[test]
fn a_project_without_a_lockfile_is_answered_with_the_lockfile_it_would_have() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(SystemPath::new("project/main.py"), "X: int = 1\n")?
        .build()
        .wait_until_workspaces_are_initialized();
    let answer = server.send_request_await::<ApiLock>(json!({}));
    assert_eq!(answer["uri"], json!(server.file_uri("project/api.lock")));
    assert_eq!(answer["python"], "default");
    assert_eq!(answer["exists"], false);
    assert_eq!(answer["staleLine"], Value::Null);
    let text = answer["text"].as_str().unwrap_or_default();
    assert!(text.starts_with("#api-lock:v=1\n"), "{text}");
    assert!(
        text.contains("\n#python:default\n#modules:1\nmain.X:v=builtins.int\n"),
        "{text}"
    );
    Ok(())
}

/// a file is answered with the lockfile of the project the server analyses it in, the innermost
/// where projects nest, as the merge requests read that project's configuration for it
#[test]
fn a_file_of_a_nested_project_is_answered_with_that_projects_lockfile() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("outer"), None)?
        .with_workspace(SystemPath::new("outer/inner"), None)?
        .with_file(
            SystemPath::new("outer/pyproject.toml"),
            "[project]\nname = \"outer\"\n",
        )?
        .with_file(
            SystemPath::new("outer/inner/pyproject.toml"),
            "[project]\nname = \"inner\"\n",
        )?
        .with_file(SystemPath::new("outer/main.py"), "")?
        .with_file(SystemPath::new("outer/inner/main.py"), "")?
        .build()
        .wait_until_workspaces_are_initialized();
    for (file, lockfile) in [
        ("outer/inner/main.py", "outer/inner/api.lock"),
        ("outer/main.py", "outer/api.lock"),
    ] {
        let answer = server.send_request_await::<ApiLock>(json!({ "uri": server.file_uri(file) }));
        assert_eq!(answer["uri"], json!(server.file_uri(lockfile)), "{file}");
    }
    Ok(())
}

/// the lockfile names the python it is for, and only the `by` that wrote it differs, which does
/// not make it stale
#[test]
fn the_lockfile_is_generated_for_the_python_its_header_names() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(SystemPath::new("project/main.py"), VERSIONED)?
        .with_file(
            SystemPath::new("project/api.lock"),
            "#api-lock:v=1\n#tool:by=0.0.0\n#python:3.10\n#modules:1\nmain.X:v=builtins.str\nmain.sys:m=sys\n",
        )?
        .build()
        .wait_until_workspaces_are_initialized();
    let uri = server.file_uri("project/main.py");
    let answer = server.send_request_await::<ApiLock>(json!({ "uri": uri }));
    let text = answer["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("\n#python:3.10\n#modules:1\nmain.X:v=builtins.str\n"),
        "{text}"
    );
    assert_eq!(answer["python"], "3.10");
    assert_eq!(answer["exists"], true);
    assert_eq!(answer["staleLine"], Value::Null, "{text}");
    Ok(())
}

#[test]
fn a_python_the_lockfile_names_that_by_cannot_read_fails_the_request() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(SystemPath::new("project/main.py"), "X = 1\n")?
        .with_file(
            SystemPath::new("project/api.lock"),
            "#api-lock:v=1\n#python:three\n",
        )?
        .build()
        .wait_until_workspaces_are_initialized();
    let id = server.send_request::<ApiLock>(json!({}));
    match server.try_await_response::<ApiLock>(&id, None) {
        Err(AwaitResponseError::RequestFailed(error)) => {
            assert_eq!(error.code, ErrorCode::RequestFailed as i32);
        }
        other => panic!("expected the request to fail, got {other:?}"),
    }
    Ok(())
}

/// a lockfile that is there but is not utf-8 is not one the project lacks
#[test]
fn a_lockfile_that_cannot_be_read_fails_the_request() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(SystemPath::new("project/main.py"), "X = 1\n")?
        .build()
        .wait_until_workspaces_are_initialized();
    std::fs::write(
        server.file_path("project/api.lock").as_std_path(),
        b"#api-lock:v=1\n#python:3.10\n\xff\n",
    )?;
    let id = server.send_request::<ApiLock>(json!({}));
    match server.try_await_response::<ApiLock>(&id, None) {
        Err(AwaitResponseError::RequestFailed(error)) => {
            assert_eq!(error.code, ErrorCode::RequestFailed as i32);
            assert!(
                error.message.contains("cannot be read"),
                "{}",
                error.message
            );
        }
        other => panic!("expected the request to fail, got {other:?}"),
    }
    Ok(())
}

/// the python a lockfile names is what it is checked for, so one `by` cannot read is reported
/// where it is named
#[test]
fn a_python_the_lockfile_names_that_by_cannot_read_is_reported_on_its_header() -> Result<()> {
    let lock = "#api-lock:v=1\n#python:three\n";
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(SystemPath::new("project/main.py"), "X = 1\n")?
        .with_file(SystemPath::new("project/api.lock"), lock)?
        .enable_pull_diagnostics(true)
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document_as("project/api.lock", lock, 1, LanguageKind::new("plaintext"));
    let reported = diagnostics(&mut server, "project/api.lock");
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert_eq!(
        reported[0].range,
        Range::new(Position::new(1, 0), Position::new(1, 13))
    );
    assert_eq!(
        reported[0].code,
        Some(lsp_types::Code::String("unreadable-api-lock".to_string()))
    );
    Ok(())
}

fn diagnostics(server: &mut TestServer, path: &str) -> Vec<Diagnostic> {
    match server.document_diagnostic_request(path, None) {
        DocumentDiagnosticReport::RelatedFullDocumentDiagnosticReport(report) => {
            report.full_document_diagnostic_report.items
        }
        DocumentDiagnosticReport::RelatedUnchangedDocumentDiagnosticReport(_) => {
            panic!("asked without a previous result, the report is full")
        }
    }
}

fn replace(server: &mut TestServer, path: &str, text: &str, version: i32) {
    server.change_text_document(
        path,
        vec![
            TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                TextDocumentContentChangeWholeDocument {
                    text: text.to_string(),
                },
            ),
        ],
        version,
    );
}

/// an unsaved edit to a module the lockfile describes makes it stale, at the record that
/// changed, and the quick fix rewrites it as the server generates it
#[test]
fn an_unsaved_edit_makes_the_lockfile_stale_until_it_is_regenerated() -> Result<()> {
    let lock =
        "#api-lock:v=1\n#tool:by=0.0.0\n#python:default\n#modules:1\nmain.X:v=builtins.int\n";
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(SystemPath::new("project/main.py"), "X: int = 1\n")?
        .with_file(SystemPath::new("project/api.lock"), lock)?
        .enable_pull_diagnostics(true)
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document_as("project/api.lock", lock, 1, LanguageKind::new("plaintext"));
    assert_eq!(diagnostics(&mut server, "project/api.lock"), vec![]);

    server.open_text_document("project/main.py", "X: int = 1\n", 1);
    replace(&mut server, "project/main.py", "X: str = \"\"\n", 2);
    let stale = diagnostics(&mut server, "project/api.lock");
    assert_eq!(stale.len(), 1, "{stale:?}");
    assert_eq!(
        stale[0].range,
        Range::new(Position::new(4, 0), Position::new(4, 21))
    );
    assert_eq!(
        stale[0].code,
        Some(lsp_types::Code::String("stale-api-lock".to_string()))
    );

    let actions = server.send_request_await::<CodeActionRequest>(lsp_types::CodeActionParams {
        text_document: lsp_types::TextDocumentIdentifier {
            uri: server.file_uri("project/api.lock"),
        },
        range: stale[0].range,
        context: lsp_types::CodeActionContext {
            diagnostics: stale,
            only: Some(vec![lsp_types::CodeActionKind::QuickFix]),
            trigger_kind: None,
        },
        work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        partial_result_params: lsp_types::PartialResultParams::default(),
    });
    let actions = serde_json::to_value(actions)?;
    assert_eq!(actions[0]["title"], "Regenerate api.lock");
    let edits = &actions[0]["edit"]["changes"][server.file_uri("project/api.lock").as_str()];
    assert_eq!(
        edits[0]["range"],
        json!({"start": {"line": 0, "character": 0}, "end": {"line": 5, "character": 0}})
    );
    let regenerated = edits[0]["newText"].as_str().unwrap_or_default().to_string();
    assert!(
        regenerated.ends_with("\nmain.X:v=builtins.str\n"),
        "{regenerated}"
    );

    replace(&mut server, "project/api.lock", &regenerated, 2);
    assert_eq!(diagnostics(&mut server, "project/api.lock"), vec![]);
    Ok(())
}

/// a client that does not act on `interFileDependencies` is asked to pull every document's
/// diagnostics again after a run of edits, and then finds the lockfile stale. that the run asks
/// once is `DiagnosticRefresh`'s to show: how far apart the edits arrive is not this test's to say
#[test]
fn a_run_of_edits_asks_the_client_to_pull_the_lockfile_diagnostics_again() -> Result<()> {
    let lock =
        "#api-lock:v=1\n#tool:by=0.0.0\n#python:default\n#modules:1\nmain.X:v=builtins.int\n";
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(SystemPath::new("project/main.py"), "X: int = 1\n")?
        .with_file(SystemPath::new("project/api.lock"), lock)?
        .enable_pull_diagnostics(true)
        .enable_workspace_diagnostic_refresh(true)
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document_as("project/api.lock", lock, 1, LanguageKind::new("plaintext"));
    server.open_text_document("project/main.py", "X: int = 1\n", 1);
    assert_eq!(diagnostics(&mut server, "project/api.lock"), vec![]);

    for (version, text) in [
        "X: s = 1\n",
        "X: st = 1\n",
        "X: str = 1\n",
        "X: str = \"\"\n",
    ]
    .into_iter()
    .enumerate()
    {
        replace(
            &mut server,
            "project/main.py",
            text,
            2 + i32::try_from(version)?,
        );
    }
    server.await_diagnostic_refresh();
    assert_eq!(diagnostics(&mut server, "project/api.lock").len(), 1);
    Ok(())
}

/// a client that does not pull diagnostics is published the lockfile's as it opens it, and again
/// after a run of edits to a module it describes
#[test]
fn a_client_that_does_not_pull_diagnostics_is_published_the_lockfile_diagnostics() -> Result<()> {
    let lock =
        "#api-lock:v=1\n#tool:by=0.0.0\n#python:default\n#modules:1\nmain.X:v=builtins.int\n";
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(SystemPath::new("project/main.py"), "X: int = 1\n")?
        .with_file(SystemPath::new("project/api.lock"), lock)?
        .enable_pull_diagnostics(false)
        .build()
        .wait_until_workspaces_are_initialized();
    let api_lock = server.file_uri("project/api.lock");
    server.open_text_document_as("project/api.lock", lock, 1, LanguageKind::new("plaintext"));
    let published = server.await_notification::<PublishDiagnosticsNotification>();
    assert_eq!(published.uri, api_lock);
    assert_eq!(published.diagnostics, vec![]);

    // the module is published as it is opened, and as it is edited
    server.open_text_document("project/main.py", "X: int = 1\n", 1);
    replace(&mut server, "project/main.py", "X: str = \"\"\n", 2);
    for _ in 0..2 {
        let published = server.await_notification::<PublishDiagnosticsNotification>();
        assert_ne!(published.uri, api_lock);
    }
    // and then every open document is published again, in no particular order
    let mut republished: Vec<_> = (0..2)
        .map(|_| server.await_notification::<PublishDiagnosticsNotification>())
        .collect();
    republished.retain(|published| published.uri == api_lock);
    let [
        PublishDiagnosticsParams {
            diagnostics: stale, ..
        },
    ] = republished.as_slice()
    else {
        panic!("the lockfile is published again once: {republished:?}");
    };
    assert_eq!(stale.len(), 1, "{stale:?}");
    assert_eq!(
        stale[0].range,
        Range::new(Position::new(4, 0), Position::new(4, 21))
    );
    assert_eq!(
        stale[0].code,
        Some(lsp_types::Code::String("stale-api-lock".to_string()))
    );
    Ok(())
}

/// closing a module with unsaved edits reads it from disk again, which changes what the lockfile
/// open beside it is reported as, and the client is asked to pull its diagnostics again
#[test]
fn closing_an_unsaved_module_asks_the_client_to_pull_the_lockfile_diagnostics_again() -> Result<()>
{
    let lock =
        "#api-lock:v=1\n#tool:by=0.0.0\n#python:default\n#modules:1\nmain.X:v=builtins.int\n";
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(SystemPath::new("project/main.py"), "X: int = 1\n")?
        .with_file(SystemPath::new("project/api.lock"), lock)?
        .enable_pull_diagnostics(true)
        .enable_workspace_diagnostic_refresh(true)
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document_as("project/api.lock", lock, 1, LanguageKind::new("plaintext"));
    server.open_text_document("project/main.py", "X: int = 1\n", 1);
    replace(&mut server, "project/main.py", "X: str = \"\"\n", 2);
    server.await_diagnostic_refresh();
    assert_eq!(diagnostics(&mut server, "project/api.lock").len(), 1);

    server.close_text_document("project/main.py");
    server.await_diagnostic_refresh();
    assert_eq!(diagnostics(&mut server, "project/api.lock"), vec![]);
    Ok(())
}
