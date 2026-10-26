//! `by/docstringStub` over the wire
//!
//! what a stub holds is `ty_ide`'s and its unit tests cover it. what these cover is the contract
//! a client reads: a position in an empty docstring and an optional format in, and out the edit
//! that replaces the literal and where the caret goes once it is applied

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::{TestServer, TestServerBuilder};

/// the request as a client sends it, in json throughout
enum DocstringStub {}

impl Request for DocstringStub {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/docstringStub");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const MAIN: &str = "\
class Cart:
    def add(self, item: int) -> bool:
        \"\"\"\"\"\"
        return True
";

fn server() -> Result<TestServer> {
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/main.by"), MAIN)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/main.by"), MAIN, 1);
    Ok(server)
}

fn ask(
    server: &mut TestServer,
    line: u32,
    character: u32,
    format: Option<&str>,
) -> Option<serde_json::Value> {
    let uri = server.file_uri(SystemPath::new("src/main.by"));
    let mut params = serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character },
    });
    if let Some(format) = format {
        params["format"] = serde_json::json!(format);
    }
    server.send_request_await::<DocstringStub>(params)
}

#[test]
fn an_empty_docstring_is_answered_with_its_stub_and_the_caret() -> Result<()> {
    let mut server = server()?;

    assert_eq!(
        ask(&mut server, 2, 11, Some("google")),
        Some(serde_json::json!({
            "range": {
                "start": { "line": 2, "character": 8 },
                "end": { "line": 2, "character": 14 },
            },
            "newText": "\"\"\"\n        \n\n        Args:\n            item:\n\n        Returns:\n        \"\"\"",
            "caret": { "line": 3, "character": 8 },
        }))
    );

    Ok(())
}

#[test]
fn anywhere_else_is_answered_with_null() -> Result<()> {
    let mut server = server()?;

    assert_eq!(ask(&mut server, 3, 10, None), None);

    Ok(())
}
