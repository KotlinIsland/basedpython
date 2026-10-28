//! `by/gitMergeIntegration` over the wire: what an editor offers to set up, and setting it up

use std::process::Command;

use anyhow::Result;
use lsp_server::ErrorCode;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use serde_json::{Value, json};

use crate::{AwaitResponseError, TestServerBuilder};

enum GitMergeIntegration {}

impl Request for GitMergeIntegration {
    type Params = Value;
    type Result = Value;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/gitMergeIntegration");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// `git` in `directory`, with none of the configuration of the machine it runs on
fn git(directory: &SystemPath, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory.as_std_path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("failed to spawn git");
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn a_project_in_a_repository_is_answered_with_what_is_missing_until_it_is_set_up() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(SystemPath::new("project/main.by"), "")?
        .build()
        .wait_until_workspaces_are_initialized();
    let root = server.file_path("project");
    git(&root, &["init", "-q"]);

    let state = server.send_request_await::<GitMergeIntegration>(json!({}));
    // whatever git the machine has, as `git version` writes it
    let version = git(&root, &["version"]);
    assert_eq!(
        state,
        json!({
            "enabled": false,
            "repository": true,
            "gitVersion": version,
            "gitSupported": true,
            "attributes": server.file_uri("project/.gitattributes"),
            "missingAttributes": ["*.by", "*.byi", "*.py", "*.pyi", "api.lock"],
            "driver": null,
            "driverFound": false,
        })
    );

    let state = server.send_request_await::<GitMergeIntegration>(json!({ "enable": true }));
    assert_eq!(state["enabled"], true, "{state}");
    assert_eq!(state["missingAttributes"], json!([]));
    let driver = git(&root, &["config", "--get", "merge.by.driver"]);
    assert!(
        driver.ends_with("' merge-driver %O %A %B %L %P %S %X %Y"),
        "{driver}"
    );
    assert_eq!(state["driver"], json!(driver));
    assert_eq!(
        git(&root, &["check-attr", "merge", "--", "pkg/a.py"]),
        "pkg/a.py: merge: by"
    );
    Ok(())
}

#[test]
fn setting_up_a_project_outside_a_repository_fails() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .build()
        .wait_until_workspaces_are_initialized();
    let state = server.send_request_await::<GitMergeIntegration>(json!({}));
    assert_eq!(state["repository"], false);
    let id = server.send_request::<GitMergeIntegration>(json!({ "enable": true }));
    match server.try_await_response::<GitMergeIntegration>(&id, None) {
        Err(AwaitResponseError::RequestFailed(error)) => {
            assert_eq!(error.code, ErrorCode::RequestFailed as i32);
        }
        other => panic!("expected the request to fail, got {other:?}"),
    }
    Ok(())
}
