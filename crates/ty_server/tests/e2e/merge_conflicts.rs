//! `by/resolveMergeConflicts` and `by/diffIgnoredRanges` over the wire: texts in, no document

use anyhow::Result;
use lsp_server::ErrorCode;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use serde_json::{Value, json};

use crate::{AwaitResponseError, TestServerBuilder};

enum ResolveMergeConflicts {}

impl Request for ResolveMergeConflicts {
    type Params = Value;
    type Result = Value;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/resolveMergeConflicts");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

enum DiffIgnoredRanges {}

impl Request for DiffIgnoredRanges {
    type Params = Value;
    type Result = Value;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/diffIgnoredRanges");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

#[test]
fn each_conflict_is_answered_with_its_resolution_or_null() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(SystemPath::new("project/main.by"), "")?
        .build()
        .wait_until_workspaces_are_initialized();
    let uri = server.file_uri("project/main.by");
    let answer = server.send_request_await::<ResolveMergeConflicts>(json!({
        "base": "import os\nLIMIT = 1\n",
        "ours": "import os\nimport sys\nLIMIT = 2\n",
        "theirs": "import os\nimport json\nLIMIT = 3\n",
        "conflicts": [
            {"base": {"start": 1, "end": 1}, "ours": {"start": 1, "end": 2}, "theirs": {"start": 1, "end": 2}},
            {"base": {"start": 1, "end": 2}, "ours": {"start": 2, "end": 3}, "theirs": {"start": 2, "end": 3}},
        ],
        "uri": uri,
    }));
    assert_eq!(answer, json!(["import json\nimport sys\n", null]));
    Ok(())
}

#[test]
fn lines_that_end_before_they_start_are_invalid_params() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .build()
        .wait_until_workspaces_are_initialized();
    let id = server.send_request::<ResolveMergeConflicts>(json!({
        "base": "import os\n",
        "ours": "import os\n",
        "theirs": "import os\n",
        "conflicts": [
            {"base": {"start": 1, "end": 0}, "ours": {"start": 0, "end": 1}, "theirs": {"start": 0, "end": 1}},
        ],
    }));
    match server.try_await_response::<ResolveMergeConflicts>(&id, None) {
        Err(AwaitResponseError::RequestFailed(error)) => {
            assert_eq!(error.code, ErrorCode::InvalidParams as i32);
        }
        other => panic!("expected invalid params, got {other:?}"),
    }
    Ok(())
}

#[test]
fn imports_and_formatting_are_answered_apart() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .build()
        .wait_until_workspaces_are_initialized();
    let answer = server
        .send_request_await::<DiffIgnoredRanges>(json!({ "text": "import os\nx = [1 ,2]\n" }));
    let range = |line: u32, start: u32, end: u32| json!({"start": {"line": line, "character": start}, "end": {"line": line, "character": end}});
    assert_eq!(
        answer,
        json!({
            "imports": [range(0, 0, 9)],
            "formatting": [range(0, 6, 7), range(1, 1, 2), range(1, 3, 4), range(1, 6, 7)],
        })
    );
    Ok(())
}
