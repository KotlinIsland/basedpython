//! `by/resolveMergeConflicts`, `by/reconcileMergeImports` and `by/diffIgnoredRanges` over the
//! wire: texts in, no document

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

enum ReconcileMergeImports {}

impl Request for ReconcileMergeImports {
    type Params = Value;
    type Result = Value;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/reconcileMergeImports");
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

/// a file in a project nested in another is merged as that project sorts its imports: its own
/// packages are first-party
#[test]
fn a_file_of_a_nested_project_is_resolved_as_that_project_sorts_imports() -> Result<()> {
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
        .with_file(SystemPath::new("outer/inner/mylib/__init__.py"), "")?
        .with_file(SystemPath::new("outer/inner/main.py"), "")?
        .build()
        .wait_until_workspaces_are_initialized();
    let uri = server.file_uri("outer/inner/main.py");
    let answer = server.send_request_await::<ResolveMergeConflicts>(json!({
        "base": "import os\n",
        "ours": "import os\nimport mylib\n",
        "theirs": "import os\nimport attrs\n",
        "conflicts": [
            {"base": {"start": 1, "end": 1}, "ours": {"start": 1, "end": 2}, "theirs": {"start": 1, "end": 2}},
        ],
        "uri": uri,
    }));
    assert_eq!(answer, json!(["import attrs\n\nimport mylib\n"]));
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

/// ours removed the last reader of `json` and its import, theirs added another reader, and a
/// line merge wrote the file without a conflict
#[test]
fn a_clean_merge_that_dropped_a_needed_import_is_answered_with_the_edit_adding_it() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(SystemPath::new("project/main.by"), "")?
        .build()
        .wait_until_workspaces_are_initialized();
    let uri = server.file_uri("project/main.by");
    let answer = server.send_request_await::<ReconcileMergeImports>(json!({
        "base": "import json\nimport os\n\nA = os\nB = json\n",
        "ours": "import os\n\nA = os\n",
        "theirs": "import json\nimport os\n\nA = os\nB = json\nC = json\n",
        "merged": "import os\n\nA = os\nC = json\n",
        "uri": uri,
    }));
    let range = json!({"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}});
    assert_eq!(
        answer,
        json!({"edits": [{"range": range, "newText": "import json\nimport os\n"}], "choice": null})
    );
    Ok(())
}

#[test]
fn a_name_imported_from_two_places_is_answered_with_the_choice() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .build()
        .wait_until_workspaces_are_initialized();
    let answer = server.send_request_await::<ReconcileMergeImports>(json!({
        "base": "from a import x\n\nY = x\n",
        "ours": "Y = x\n",
        "theirs": "from b import x\n\nY = x\n",
        "merged": "Y = x\n",
    }));
    assert_eq!(
        answer,
        json!({
            "edits": [],
            "choice": {
                "name": "x",
                "imports": [
                    {"version": "ours", "statements": []},
                    {"version": "base", "statements": ["from a import x"]},
                    {"version": "theirs", "statements": ["from b import x"]},
                ],
                "position": {"line": 0, "character": 0},
                "bindings": [],
            },
        })
    );
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
