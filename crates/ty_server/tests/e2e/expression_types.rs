//! `by/expressionTypes` over the wire
//!
//! which expressions a range is in, and their types, is `ty_ide`'s and its unit tests cover it.
//! what these cover is the contract a client reads: a range in, and out the expressions that
//! contain it, innermost first, each with its range and its type as the document spells it, on one
//! line and written for a reader

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::{TestServer, TestServerBuilder};

/// the request as a client sends it, in json throughout
enum ExpressionTypes {}

impl Request for ExpressionTypes {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/expressionTypes");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const MAIN: &str = "\
def size(text: str) -> int:
    return len(text) + 1
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

fn ask(server: &mut TestServer, start: u32, end: u32) -> Option<serde_json::Value> {
    let uri = server.file_uri(SystemPath::new("src/main.by"));
    server.send_request_await::<ExpressionTypes>(serde_json::json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": 1, "character": start },
            "end": { "line": 1, "character": end },
        },
    }))
}

fn found(start: u32, end: u32, ty: &str) -> serde_json::Value {
    serde_json::json!({
        "range": {
            "start": { "line": 1, "character": start },
            "end": { "line": 1, "character": end },
        },
        "type": ty,
        "pretty": { "language": "python", "value": ty },
    })
}

#[test]
fn a_caret_answers_every_expression_around_it_innermost_first() -> Result<()> {
    let mut server = server()?;

    assert_eq!(
        ask(&mut server, 17, 17),
        Some(serde_json::json!([
            found(15, 19, "str"),
            found(11, 20, "int"),
            found(11, 24, "int"),
        ]))
    );
    Ok(())
}

#[test]
fn a_selection_starts_from_the_expression_it_spans() -> Result<()> {
    let mut server = server()?;

    assert_eq!(
        ask(&mut server, 11, 20),
        Some(serde_json::json!([
            found(11, 20, "int"),
            found(11, 24, "int")
        ]))
    );
    Ok(())
}

#[test]
fn outside_an_expression_answers_nothing() -> Result<()> {
    let mut server = server()?;

    assert_eq!(ask(&mut server, 0, 0), Some(serde_json::json!([])));
    Ok(())
}

#[test]
fn an_overloaded_function_is_also_written_one_signature_after_another() -> Result<()> {
    let source = "print(1)\n";
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/main.py"), source)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/main.py"), source, 1);
    let uri = server.file_uri(SystemPath::new("src/main.py"));

    let answer = server
        .send_request_await::<ExpressionTypes>(serde_json::json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 0, "character": 2 },
                "end": { "line": 0, "character": 2 },
            },
        }))
        .expect("language services are on");

    assert!(
        answer[0]["type"]
            .as_str()
            .is_some_and(|ty| ty.starts_with("Overload[(*values: object, sep: ")),
        "{answer}"
    );
    assert_eq!(answer[0]["pretty"]["language"], "python");
    insta::assert_snapshot!(answer[0]["pretty"]["value"].as_str().unwrap_or_default(), @r#"
    def print(
        *values: object,
        sep: str | None = " ",
        end: str | None = "\n",
        file: SupportsWrite[str] | None = None,
        flush: Literal[False] = False
    )
    def print(
        *values: object,
        sep: str | None = " ",
        end: str | None = "\n",
        file: SupportsWriteAndFlush[str] | None = None,
        flush: bool
    )
    "#);
    Ok(())
}
