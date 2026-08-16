//! Tests for the `workspace/executeCommand` handlers that rewrite a document.
//!
//! Each of these commands answers by asking the client to apply a workspace edit, so a test drives
//! them in three steps: send the command, take the `workspace/applyEdit` request the server sends
//! back, and tell the server the edit was applied.

use anyhow::Result;
use insta::assert_json_snapshot;
use lsp_types::{
    ApplyWorkspaceEditRequest, ApplyWorkspaceEditResult, ExecuteCommandParams,
    ExecuteCommandRequest, WorkspaceEdit,
};

use crate::{TestServer, TestServerBuilder};

/// Runs `command` over `path` and returns the edit the server asks the client to apply.
fn execute_command(server: &mut TestServer, command: &str, path: &str) -> WorkspaceEdit {
    let uri = server.file_uri(path);
    let request_id = server.send_request::<ExecuteCommandRequest>(ExecuteCommandParams {
        command: command.to_string(),
        arguments: Some(vec![serde_json::json!({ "uri": uri, "version": 1 })]),
        work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
    });

    let (edit_request_id, params) = server.await_request::<ApplyWorkspaceEditRequest>();
    server.respond(
        edit_request_id,
        ApplyWorkspaceEditResult {
            applied: true,
            failure_reason: None,
            failed_change: None,
        },
    );
    server.await_response::<ExecuteCommandRequest>(&request_id);

    params.edit
}

/// A module whose imports are both out of order and partly unused: `os` is imported and never
/// used, and the two that are used are the wrong way round. Sorting and removal are both visible
/// here, so a command that does only one of them cannot pass for doing both.
const UNSORTED_WITH_UNUSED: &str =
    "import sys\nimport os\nimport abc\n\nprint(sys.argv, abc.ABC)\n";

/// `organizeImports` is isort's job and only that: all three imports are sorted, and the unused
/// `os` stays.
#[test]
fn organize_imports_sorts_but_keeps_unused() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(".")?
        .enable_apply_edit(true)
        .build();

    server.open_text_document("test.py", UNSORTED_WITH_UNUSED, 1);

    assert_json_snapshot!(execute_command(
        &mut server,
        "ruff.applyOrganizeImports",
        "test.py"
    ));

    Ok(())
}

/// `optimizeImports` is what an editor's *Optimize Imports* means: the unused `os` is gone, and
/// what remains is sorted.
#[test]
fn optimize_imports_sorts_and_removes_unused() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(".")?
        .enable_apply_edit(true)
        .build();

    server.open_text_document("test.py", UNSORTED_WITH_UNUSED, 1);

    assert_json_snapshot!(execute_command(
        &mut server,
        "ruff.applyOptimizeImports",
        "test.py"
    ));

    Ok(())
}

/// Removing an import from `__init__.py` can break what a package deliberately re-exports, so F401
/// marks that fix unsafe and it is skipped unless the user opts into unsafe fixes. The import
/// ordering still applies.
#[test]
fn optimize_imports_keeps_unused_in_init() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(".")?
        .enable_apply_edit(true)
        .build();

    server.open_text_document("__init__.py", "import sys\nimport os\n", 1);

    assert_json_snapshot!(execute_command(
        &mut server,
        "ruff.applyOptimizeImports",
        "__init__.py"
    ));

    Ok(())
}

/// The composite runs the import pass and the formatter against one buffer, so a file that needs
/// both comes back as a single edit rather than one per operation.
#[test]
fn format_and_optimize_imports_is_a_single_edit() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(".")?
        .enable_apply_edit(true)
        .build();

    server.open_text_document(
        "test.py",
        "import sys\nimport os\n\ndef f( a,b ):\n    return  a+b\n\nprint(sys.argv)\n",
        1,
    );

    let edit = execute_command(&mut server, "ruff.applyFormatAndOptimizeImports", "test.py");

    let changes = edit.changes.as_ref().expect("Expected document changes");
    let edits = changes.values().next().expect("Expected one document");
    assert_eq!(
        edits.len(),
        1,
        "The composite should produce one edit, not one per operation"
    );

    assert_json_snapshot!(edit);

    Ok(())
}

/// The formatter runs over what the import pass left behind, not over the original source. Removing
/// the unused import here deletes a line, and the result is still formatted — which would not hold
/// if the two passes were computed against the same starting text and merged.
#[test]
fn format_and_optimize_imports_formats_the_fixed_source() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(".")?
        .enable_apply_edit(true)
        .build();

    server.open_text_document("test.py", "import os\nx = {  'a' : 1 }\n", 1);

    assert_json_snapshot!(execute_command(
        &mut server,
        "ruff.applyFormatAndOptimizeImports",
        "test.py"
    ));

    Ok(())
}

/// A file that is already sorted and already formatted has nothing to apply, so the server never
/// asks the client to edit anything.
#[test]
fn format_and_optimize_imports_is_quiet_when_there_is_nothing_to_do() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(".")?
        .enable_apply_edit(true)
        .build();

    server.open_text_document("test.py", "import os\n\nprint(os.name)\n", 1);

    let uri = server.file_uri("test.py");
    let request_id = server.send_request::<ExecuteCommandRequest>(ExecuteCommandParams {
        command: "ruff.applyFormatAndOptimizeImports".to_string(),
        arguments: Some(vec![serde_json::json!({ "uri": uri, "version": 1 })]),
        work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
    });
    server.await_response::<ExecuteCommandRequest>(&request_id);

    Ok(())
}
