//! `by/prepareChangeSignature` and `by/changeSignature` over the wire
//!
//! how calls are rewritten, and when a change is refused, is `ty_ide`'s and its unit tests cover
//! it. what these cover is the contract a client reads: a function's parameters, and the
//! workspace edit or refusal a change answers

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use serde_json::json;

use crate::{TestServer, TestServerBuilder};

/// `by/prepareChangeSignature`, in json throughout
enum PrepareChangeSignature {}

impl Request for PrepareChangeSignature {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/prepareChangeSignature");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// `by/changeSignature`, in json throughout
enum ChangeSignature {}

impl Request for ChangeSignature {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/changeSignature");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const MAIN: &str = "\
def area(width: float, height: float = 1) -> float:
    return width * height


print(area(2, 3))
";

fn server() -> Result<TestServer> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/main.py"), MAIN)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/main.py"), MAIN, 1);
    Ok(server)
}

#[test]
fn a_call_names_the_function_and_its_parameters() -> Result<()> {
    let mut server = server()?;
    let uri = server.file_uri(SystemPath::new("src/main.py"));
    let answer = server.send_request_await::<PrepareChangeSignature>(json!({
        "textDocument": { "uri": uri },
        "position": { "line": 4, "character": 7 },
    }));
    assert_eq!(
        answer,
        Some(json!({
            "name": "area",
            "uri": uri,
            "range": { "start": { "line": 0, "character": 4 }, "end": { "line": 0, "character": 8 } },
            "parameters": [
                { "name": "width", "kind": "positionalOrKeyword", "default": null, "annotation": "float" },
                { "name": "height", "kind": "positionalOrKeyword", "default": "1", "annotation": "float" },
            ],
            "returns": "float",
        }))
    );
    Ok(())
}

#[test]
fn a_change_answers_the_edit_to_the_definition_and_every_call() -> Result<()> {
    let mut server = server()?;
    let uri = server.file_uri(SystemPath::new("src/main.py"));
    let answer = server
        .send_request_await::<ChangeSignature>(json!({
            "textDocument": { "uri": uri },
            "position": { "line": 0, "character": 5 },
            "parameters": [
                { "name": "height", "oldName": "height", "kind": "positionalOrKeyword" },
                { "name": "width", "oldName": "width", "kind": "positionalOrKeyword" },
                { "name": "scale", "kind": "keywordOnly", "default": "1.0" },
            ],
        }))
        .expect("an answer");
    assert_eq!(
        answer,
        json!({
            "edit": { "changes": { uri.as_str(): [
                {
                    "range": { "start": { "line": 0, "character": 8 }, "end": { "line": 0, "character": 41 } },
                    "newText": "(height: float, width: float, *, scale=1.0)",
                },
                {
                    "range": { "start": { "line": 4, "character": 10 }, "end": { "line": 4, "character": 16 } },
                    "newText": "(3, 2)",
                },
            ] } },
            "warnings": [],
        })
    );

    let refused = server.send_request_await::<ChangeSignature>(json!({
        "textDocument": { "uri": uri },
        "position": { "line": 0, "character": 5 },
        "parameters": [{ "name": "width", "oldName": "width", "kind": "positionalOrKeyword" }],
    }));
    assert_eq!(
        refused,
        Some(json!({
            "refused": "the body still reads `height`, on line 2",
            "warnings": [],
        }))
    );
    Ok(())
}

/// `name` renames the function, a parameter's `annotation` replaces its own and `null` removes it,
/// and `returns` does the same for the return annotation; each absent leaves what is written
#[test]
fn a_change_renames_the_function_and_rewrites_its_annotations() -> Result<()> {
    let mut server = server()?;
    let uri = server.file_uri(SystemPath::new("src/main.py"));
    let answer = server
        .send_request_await::<ChangeSignature>(json!({
            "textDocument": { "uri": uri },
            "position": { "line": 0, "character": 5 },
            "name": "surface",
            "parameters": [
                { "name": "width", "oldName": "width", "kind": "positionalOrKeyword", "annotation": null },
                { "name": "height", "oldName": "height", "kind": "positionalOrKeyword", "default": "1", "annotation": "int" },
            ],
            "returns": null,
        }))
        .expect("an answer");
    assert_eq!(
        answer,
        json!({
            "edit": { "changes": { uri.as_str(): [
                {
                    "range": { "start": { "line": 0, "character": 4 }, "end": { "line": 0, "character": 8 } },
                    "newText": "surface",
                },
                {
                    "range": { "start": { "line": 0, "character": 8 }, "end": { "line": 0, "character": 41 } },
                    "newText": "(width, height: int = 1)",
                },
                {
                    "range": { "start": { "line": 0, "character": 41 }, "end": { "line": 0, "character": 50 } },
                    "newText": "",
                },
                {
                    "range": { "start": { "line": 4, "character": 6 }, "end": { "line": 4, "character": 10 } },
                    "newText": "surface",
                },
            ] } },
            "warnings": [],
        })
    );
    Ok(())
}

/// a refusal answers why, and no edit
#[test]
fn a_change_that_would_alter_what_the_body_reads_is_refused() -> Result<()> {
    const SCALE: &str = "\
FACTOR = 2


def scale(x):
    return x * FACTOR


print(scale(3))
";
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/scale.py"), SCALE)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/scale.py"), SCALE, 1);
    let answer = server.send_request_await::<ChangeSignature>(json!({
        "textDocument": { "uri": server.file_uri(SystemPath::new("src/scale.py")) },
        "position": { "line": 3, "character": 5 },
        "parameters": [{ "name": "FACTOR", "oldName": "x", "kind": "positionalOrKeyword" }],
    }));
    assert_eq!(
        answer,
        Some(json!({
            "refused": "the body reads `FACTOR` from outside the function",
            "warnings": [],
        }))
    );
    Ok(())
}
