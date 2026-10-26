//! `by/copiedImports` and `by/pastedImports` over the wire, and the quick fix a client may add
//! without asking
//!
//! which names copied code needs and which a paste is missing is `ty_ide`'s and its unit tests cover
//! it. what these cover is the contract a client reads: the copied text and where it came from for
//! the copy, the imports out as names, modules and members, the same imports back in for the paste with
//! the edits that add the missing ones — and the `unambiguousImport` mark in a quick fix's `data`,
//! on the one import that resolves a name and on nothing else

use anyhow::Result;
use lsp_types::{
    CodeActionRequest, DocumentDiagnosticReport, LspRequestMethod, MessageDirection, Position,
    Range, Request,
};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::{TestServer, TestServerBuilder, text_hash};

/// `by/copiedImports`, in json throughout
enum CopiedImports {}

impl Request for CopiedImports {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/copiedImports");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// `by/pastedImports`, in json throughout
enum PastedImports {}

impl Request for PastedImports {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/pastedImports");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const SHAPES: &str = "\
import math
from pathlib import Path

SCALE = 2

def area(r: float) -> float:
    return math.pi * r * r * SCALE
";

const MAIN: &str = "\
from pathlib import PurePath

def f() -> float:
    return math.pi * SCALE
";

fn server() -> Result<TestServer> {
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/shapes.py"), SHAPES)?
        .with_file(SystemPath::new("src/main.py"), MAIN)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/shapes.py"), SHAPES, 1);
    server.open_text_document(SystemPath::new("src/main.py"), MAIN, 1);
    Ok(server)
}

#[test]
fn a_copy_answers_its_imports_and_a_paste_the_ones_it_is_missing() -> Result<()> {
    let mut server = server()?;

    // the body of `area`, as it was copied from where it starts
    let copied = server
        .send_request_await::<CopiedImports>(serde_json::json!({
            "textDocument": { "uri": server.file_uri(SystemPath::new("src/shapes.py")) },
            "copies": [{
                "text": "return math.pi * r * r * SCALE",
                "position": { "line": 6, "character": 4 },
            }],
            "textHash": text_hash(SHAPES),
        }))
        .expect("a python document is answered");
    assert_eq!(
        copied,
        serde_json::json!([
            { "name": "math", "module": "math" },
            { "name": "SCALE", "module": "shapes", "member": "SCALE" },
        ]),
    );

    // pasted as the body of `f`, which has neither
    let pasted = server
        .send_request_await::<PastedImports>(serde_json::json!({
            "textDocument": { "uri": server.file_uri(SystemPath::new("src/main.py")) },
            "range": { "start": { "line": 3, "character": 4 }, "end": { "line": 3, "character": 26 } },
            "imports": copied,
            "textHash": text_hash(MAIN),
        }))
        .expect("a python document is answered");
    assert_eq!(pasted["imports"], copied, "{pasted:#}");
    let added: Vec<_> = pasted["edits"]
        .as_array()
        .expect("a list of edits")
        .iter()
        .map(|edit| edit["newText"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        added,
        // `math` joins the standard library's imports, above `pathlib`'s, and `shapes`, which is
        // the project's own, starts a section of its own below them
        ["import math\n", "\nfrom shapes import SCALE\n"],
        "{pasted:#}"
    );
    Ok(())
}

#[test]
fn only_the_one_import_that_resolves_a_name_is_marked() -> Result<()> {
    let main = "\
x = defaultdict(list)
y = deprecated
";
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("ty.toml"), "")?
        .with_file(SystemPath::new("src/main.py"), main)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/main.py"), main, 1);

    let diagnostics = match server.document_diagnostic_request(SystemPath::new("src/main.py"), None)
    {
        DocumentDiagnosticReport::RelatedFullDocumentDiagnosticReport(report) => {
            report.full_document_diagnostic_report.items
        }
        DocumentDiagnosticReport::RelatedUnchangedDocumentDiagnosticReport(_) => {
            panic!("Expected full diagnostic report")
        }
    };
    let id = server.send_request::<CodeActionRequest>(lsp_types::CodeActionParams {
        text_document: lsp_types::TextDocumentIdentifier {
            uri: server.file_uri(SystemPath::new("src/main.py")),
        },
        range: Range::new(Position::new(0, 0), Position::new(2, 0)),
        context: lsp_types::CodeActionContext {
            diagnostics,
            only: Some(vec![lsp_types::CodeActionKind::QuickFix]),
            trigger_kind: None,
        },
        work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        partial_result_params: lsp_types::PartialResultParams::default(),
    });
    let actions = server
        .await_response::<CodeActionRequest>(&id)
        .expect("quick fixes");

    let actions = serde_json::to_value(actions)?;
    let marked: Vec<_> = actions
        .as_array()
        .expect("a list of actions")
        .iter()
        .filter(|action| action["data"] == serde_json::json!({ "unambiguousImport": true }))
        .map(|action| action["title"].as_str().unwrap_or_default())
        .collect();
    // `deprecated` is in both `warnings` and `typing_extensions`, so it is not marked
    assert_eq!(marked, ["import collections.defaultdict"]);
    Ok(())
}
