//! `by/completeStatement` over the wire
//!
//! the unit tests in `ty_ide` cover what completing a statement writes. what they cannot cover is
//! the contract a client reads: the edits in the document as it was asked about, and the caret in
//! the document as the edits leave it, both in line/character positions — a caret one line out
//! puts the user's next keystroke in the wrong suite

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::{TestServer, TestServerBuilder, text_hash};

/// the request as a client sends it, in json throughout
enum CompleteStatement {}

impl Request for CompleteStatement {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/completeStatement");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

fn server_with(content: &str) -> Result<TestServer> {
    let main = SystemPath::new("src/main.by");
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(main, content)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(main, content, 1);
    Ok(server)
}

fn complete(
    server: &mut TestServer,
    content: &str,
    line: u32,
    character: u32,
    options: &serde_json::Value,
) -> serde_json::Value {
    let uri = server.file_uri(SystemPath::new("src/main.by"));
    server
        .send_request_await::<CompleteStatement>(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "options": options,
            "textHash": text_hash(content),
        }))
        .expect("the server completes a statement in a file it has open")
}

#[test]
fn a_header_is_completed_and_the_caret_goes_into_its_suite() -> Result<()> {
    let content = "def f():\n    if g(x\n";
    let mut server = server_with(content)?;
    let completed = complete(
        &mut server,
        content,
        1,
        10,
        &serde_json::json!({ "tabSize": 4, "insertSpaces": true }),
    );
    assert_eq!(
        completed,
        serde_json::json!({
            "edits": [{
                "range": {
                    "start": { "line": 1, "character": 10 },
                    "end": { "line": 1, "character": 10 },
                },
                "newText": "):\n        ",
            }],
            "caret": { "line": 2, "character": 8 },
        })
    );
    Ok(())
}

#[test]
fn a_suite_started_in_a_tab_indented_file_is_indented_with_a_tab() -> Result<()> {
    let content = "while x\n";
    let mut server = server_with(content)?;
    let completed = complete(
        &mut server,
        content,
        0,
        7,
        &serde_json::json!({ "tabSize": 8, "insertSpaces": false }),
    );
    assert_eq!(completed["edits"][0]["newText"], ":\n\t");
    assert_eq!(
        completed["caret"],
        serde_json::json!({ "line": 1, "character": 1 })
    );
    Ok(())
}

#[test]
fn a_clause_moved_out_is_one_edit_and_the_completion_another() -> Result<()> {
    let content = "if x:\n    a()\n    else\n";
    let mut server = server_with(content)?;
    let completed = complete(
        &mut server,
        content,
        2,
        8,
        &serde_json::json!({ "tabSize": 4, "insertSpaces": true }),
    );
    assert_eq!(
        completed,
        serde_json::json!({
            "edits": [
                {
                    "range": {
                        "start": { "line": 2, "character": 0 },
                        "end": { "line": 2, "character": 4 },
                    },
                    "newText": "",
                },
                {
                    "range": {
                        "start": { "line": 2, "character": 8 },
                        "end": { "line": 2, "character": 8 },
                    },
                    "newText": ":\n    ",
                },
            ],
            // in the edited text: `else` has moved four characters left
            "caret": { "line": 3, "character": 4 },
        })
    );
    Ok(())
}
