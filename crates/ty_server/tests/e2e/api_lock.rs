//! a project's `api.lock` over the wire: `by/apiLock`

use anyhow::Result;
use lsp_server::ErrorCode;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use serde_json::{Value, json};

use crate::{AwaitResponseError, TestServerBuilder};

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
