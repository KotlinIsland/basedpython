//! `by/declaration` over the wire
//!
//! what a name declares is `ty_ide`'s and its unit tests cover it. what these cover is the
//! contract a client reads: a position in, and out the declaration's kind, name and qualified
//! name with where it is, or `null` where there is no name

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::{TestServer, TestServerBuilder};

/// the request as a client sends it, in json throughout
enum Declaration {}

impl Request for Declaration {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/declaration");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const BASE: &str = "\
class Circle:
    def __init__(self, radius: float) -> None:
        self.radius = radius

    def area(self) -> float:
        return self.radius
";

const MAIN: &str = "\
from shapes.base import Circle

circle = Circle(2)
size = circle.area()

def scaled() -> float:
    factor = 2
    return factor * circle.radius
";

fn server() -> Result<TestServer> {
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/shapes/__init__.by"), "")?
        .with_file(SystemPath::new("src/shapes/base.by"), BASE)?
        .with_file(SystemPath::new("src/main.by"), MAIN)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/main.by"), MAIN, 1);
    Ok(server)
}

fn ask(server: &mut TestServer, line: u32, character: u32) -> Option<serde_json::Value> {
    let uri = server.file_uri(SystemPath::new("src/main.by"));
    server.send_request_await::<Declaration>(serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character },
    }))
}

fn range(line: u32, start: u32, end: u32) -> serde_json::Value {
    serde_json::json!({
        "start": { "line": line, "character": start },
        "end": { "line": line, "character": end },
    })
}

#[test]
fn a_method_used_elsewhere_is_named_by_its_module_and_class() -> Result<()> {
    let mut server = server()?;
    let base = server.file_uri(SystemPath::new("src/shapes/base.by"));

    let answer = ask(&mut server, 3, 16).expect("a declaration");
    assert_eq!(answer["kind"], "method");
    assert_eq!(answer["name"], "area");
    assert_eq!(answer["qualifiedName"], "shapes.base.Circle.area");
    assert_eq!(answer["originSelectionRange"], range(3, 14, 18));
    assert_eq!(answer["uri"], serde_json::json!(base));
    assert_eq!(answer["selectionRange"], range(4, 8, 12));
    Ok(())
}

#[test]
fn a_called_class_is_the_class() -> Result<()> {
    let mut server = server()?;

    let answer = ask(&mut server, 2, 10).expect("a declaration");
    assert_eq!(answer["kind"], "class");
    assert_eq!(answer["qualifiedName"], "shapes.base.Circle");
    Ok(())
}

#[test]
fn an_attribute_assigned_in_a_method_belongs_to_the_class() -> Result<()> {
    let mut server = server()?;

    let answer = ask(&mut server, 7, 30).expect("a declaration");
    assert_eq!(answer["kind"], "attribute");
    assert_eq!(answer["qualifiedName"], "shapes.base.Circle.radius");
    Ok(())
}

#[test]
fn a_local_has_no_qualified_name() -> Result<()> {
    let mut server = server()?;

    let answer = ask(&mut server, 7, 12).expect("a declaration");
    assert_eq!(answer["kind"], "variable");
    assert_eq!(answer["name"], "factor");
    assert_eq!(answer["qualifiedName"], serde_json::Value::Null);

    let module_level = ask(&mut server, 3, 1).expect("a declaration");
    assert_eq!(module_level["qualifiedName"], "main.size");
    Ok(())
}

#[test]
fn no_name_answers_null() -> Result<()> {
    let mut server = server()?;

    assert_eq!(ask(&mut server, 1, 0), None);
    Ok(())
}
