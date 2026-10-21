//! `textDocument/onTypeFormatting` over the wire: the `:` trigger the server advertises, and the
//! edit that moves a clause out to the statement it continues, in a client's line/character
//! positions

use anyhow::Result;
use lsp_types::{
    DocumentOnTypeFormattingParams, DocumentOnTypeFormattingRequest, FormattingOptions, Position,
    TextDocumentIdentifier,
};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::{TestServer, TestServerBuilder};

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

fn typed(server: &mut TestServer, line: u32, character: u32, ch: &str) -> serde_json::Value {
    let uri = server.file_uri(SystemPath::new("src/main.by"));
    let answer = server.send_request_await::<DocumentOnTypeFormattingRequest>(
        DocumentOnTypeFormattingParams::new(
            TextDocumentIdentifier::new(uri),
            Position::new(line, character),
            ch.to_string(),
            FormattingOptions::new(4, true, None, None, None),
        ),
    );
    serde_json::to_value(answer).expect("an answer serialises")
}

#[test]
fn the_colon_is_the_trigger() -> Result<()> {
    let server = server_with("x = 1\n")?;
    let provider = &server
        .initialization_result()
        .expect("initialized")
        .capabilities;
    let provider = serde_json::to_value(&provider.document_on_type_formatting_provider)?;
    assert_eq!(
        provider,
        serde_json::json!({ "firstTriggerCharacter": ":" })
    );
    Ok(())
}

#[test]
fn an_else_typed_in_the_body_is_moved_out_to_its_if() -> Result<()> {
    let mut server = server_with("def f(x):\n    if x:\n        a = 1\n        else:\n")?;
    // the caret is after the `:` just typed
    let edits = typed(&mut server, 3, 13, ":");
    assert_eq!(
        edits,
        serde_json::json!([{
            "range": {
                "start": { "line": 3, "character": 0 },
                "end": { "line": 3, "character": 8 },
            },
            "newText": "    ",
        }])
    );
    Ok(())
}

#[test]
fn a_colon_that_ends_nothing_changes_nothing() -> Result<()> {
    let mut server = server_with("if x:\n    d = {1: 2}\n")?;
    assert_eq!(typed(&mut server, 1, 11, ":"), serde_json::Value::Null);
    Ok(())
}
