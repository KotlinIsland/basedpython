//! `by/references` over the wire
//!
//! what each kind is, is `ty_ide`'s and its unit tests cover it. what these cover is the contract
//! a client reads: the references `textDocument/references` answers, in its order, each with its
//! kind

use anyhow::Result;
use lsp_types::{
    LspRequestMethod, MessageDirection, PartialResultParams, Position, ReferenceContext,
    ReferenceParams, ReferencesRequest, Request, TextDocumentIdentifier,
    TextDocumentPositionParams, WorkDoneProgressParams,
};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::TestServerBuilder;

/// the request as a client sends it, in json throughout
enum ClassifiedReferences {}

impl Request for ClassifiedReferences {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/references");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const SHAPES: &str = "\
class Shape: ...
";

const MAIN: &str = "\
from shapes import Shape

class Circle(Shape): ...

def first(shapes: list[Shape]) -> Shape:
    if isinstance(shapes[0], Shape):
        return shapes[0]
    return Shape()

Shape = Circle
";

#[test]
fn each_reference_comes_with_what_it_does() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/shapes.by"), SHAPES)?
        .with_file(SystemPath::new("src/main.by"), MAIN)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/shapes.by"), SHAPES, 1);
    let uri = server.file_uri(SystemPath::new("src/shapes.by"));
    let position = Position::new(0, 7);

    let classified = server
        .send_request_await::<ClassifiedReferences>(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": position,
            "context": { "includeDeclaration": true },
        }))
        .expect("references of a class");
    let classified = classified.as_array().expect("a list");
    let kinds: Vec<_> = classified
        .iter()
        .map(|reference| {
            format!(
                "{} {}",
                reference["range"]["start"]["line"],
                reference["kind"].as_str().unwrap_or_default()
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "0 declaration",
            "0 import",
            "2 superclass",
            "4 typeHint",
            "4 typeHint",
            "5 isinstance",
            "7 read",
            "9 write",
        ]
    );

    // the same places `textDocument/references` goes to, in its order
    let locations = server
        .send_request_await::<ReferencesRequest>(ReferenceParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration: true,
            },
        })
        .unwrap_or_default();
    let places: Vec<_> = classified
        .iter()
        .map(
            |reference| serde_json::json!({ "uri": reference["uri"], "range": reference["range"] }),
        )
        .collect();
    assert_eq!(
        places,
        serde_json::to_value(&locations)?
            .as_array()
            .cloned()
            .unwrap_or_default()
    );
    Ok(())
}
