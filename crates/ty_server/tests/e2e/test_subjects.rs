//! `by/testSubjects`, `by/subjectTests` and `by/moduleName` over the wire
//!
//! which tests call what is `ty_ide`'s and its unit tests cover it. what these cover is the contract
//! a client reads: a position in, the tests or the code as items it can show and go to, `null` kept
//! apart from an empty list, where a new test goes and the import it needs, and the dotted name of a
//! file or a package in a src-layout project, which is the search path's and not the directory's

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use serde_json::{Value, json};
use ty_server::ClientOptions;

use crate::{TestServer, TestServerBuilder};

enum TestSubjects {}

impl Request for TestSubjects {
    type Params = Value;
    type Result = Option<Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/testSubjects");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

enum SubjectTests {}

impl Request for SubjectTests {
    type Params = Value;
    type Result = Option<Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/subjectTests");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

enum ModuleName {}

impl Request for ModuleName {
    type Params = Value;
    type Result = Option<String>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/moduleName");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const CIRCLE: &str = "\
class Circle:
    def __init__(self, radius: float) -> None:
        self.radius = radius

    def area(self) -> float:
        return 3.14 * self.radius ** 2
";

const TEST_CIRCLE: &str = "\
from shapes.circle import Circle

def test_area() -> None:
    assert Circle(1.0).area() > 3

class TestCircle:
    def test_radius(self) -> None:
        assert Circle(2.0).radius == 2.0
";

/// a src-layout project with the code and its test open, as an editor asking from either has them:
/// what the answers read from every other file is the files on disk
fn server() -> Result<TestServer> {
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(SystemPath::new("project/src/shapes/__init__.by"), "")?
        .with_file(SystemPath::new("project/src/shapes/circle.by"), CIRCLE)?
        .with_file(SystemPath::new("project/tests/test_circle.by"), TEST_CIRCLE)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("project/src/shapes/circle.by"), CIRCLE, 1);
    server.open_text_document(
        SystemPath::new("project/tests/test_circle.by"),
        TEST_CIRCLE,
        1,
    );
    Ok(server)
}

fn at(server: &TestServer, path: &str, line: u32, character: u32) -> Value {
    json!({
        "textDocument": { "uri": server.file_uri(SystemPath::new(path)) },
        "position": { "line": line, "character": character },
    })
}

#[test]
fn a_test_answers_what_it_calls_as_items_to_go_to() -> Result<()> {
    let mut server = server()?;
    let circle = server.file_uri(SystemPath::new("project/src/shapes/circle.by"));

    // in `test_area`'s body
    let params = at(&server, "project/tests/test_circle.by", 3, 12);
    let subjects = server
        .send_request_await::<TestSubjects>(params)
        .expect("tests are collected from the document");

    let names: Vec<_> = subjects
        .as_array()
        .expect("a list of subjects")
        .iter()
        .map(|subject| subject["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(names, ["Circle", "Circle.area"], "{subjects:#}");
    let area = &subjects[1];
    assert_eq!(area["kind"], 6, "a method");
    assert_eq!(area["module"], "shapes.circle");
    assert_eq!(area["uri"], circle.as_str());
    assert_eq!(
        area["selectionRange"],
        json!({ "start": { "line": 4, "character": 8 }, "end": { "line": 4, "character": 12 } }),
    );
    assert_eq!(area["range"]["start"]["line"], 4);
    Ok(())
}

#[test]
fn a_document_no_test_is_collected_from_has_no_test_subjects() -> Result<()> {
    let mut server = server()?;
    let params = at(&server, "project/src/shapes/circle.by", 4, 9);
    assert_eq!(server.send_request_await::<TestSubjects>(params), None);
    Ok(())
}

#[test]
fn a_class_answers_the_tests_that_call_it_and_where_another_goes() -> Result<()> {
    let mut server = server()?;
    let tests = server.file_uri(SystemPath::new("project/tests/test_circle.by"));

    // on `class Circle`
    let params = at(&server, "project/src/shapes/circle.by", 0, 7);
    let answer = server
        .send_request_await::<SubjectTests>(params)
        .expect("the document is the project's");

    let names: Vec<_> = answer["tests"]
        .as_array()
        .expect("a list of tests")
        .iter()
        .map(|test| test["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        names,
        ["test_area", "TestCircle::test_radius"],
        "{answer:#}"
    );
    assert_eq!(answer["tests"][0]["module"], "tests.test_circle");
    assert_eq!(answer["tests"][0]["uri"], tests.as_str());
    assert_eq!(
        answer["tests"][0]["selectionRange"],
        json!({ "start": { "line": 2, "character": 4 }, "end": { "line": 2, "character": 13 } }),
    );

    // the test module exists and imports `Circle` already, so a new test needs nothing more
    assert_eq!(
        answer["newTest"],
        json!({
            "uri": tests.as_str(),
            "exists": true,
            "module": "shapes.circle",
            "import": "Circle",
            "testName": "test_circle",
            "importEdits": [],
        }),
    );
    Ok(())
}

#[test]
fn a_new_test_in_a_test_module_that_does_not_import_the_code_comes_with_the_import() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(SystemPath::new("project/src/shapes/__init__.by"), "")?
        .with_file(SystemPath::new("project/src/shapes/circle.by"), CIRCLE)?
        .with_file(
            SystemPath::new("project/tests/test_circle.by"),
            "import math\n\ndef test_pi() -> None:\n    assert math.pi > 3\n",
        )?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("project/src/shapes/circle.by"), CIRCLE, 1);

    // in `area`'s body
    let params = at(&server, "project/src/shapes/circle.by", 5, 10);
    let answer = server
        .send_request_await::<SubjectTests>(params)
        .expect("the document is the project's");

    assert_eq!(answer["tests"], json!([]));
    assert_eq!(answer["newTest"]["testName"], "test_circle_area");
    assert_eq!(answer["newTest"]["import"], "Circle");
    assert_eq!(
        answer["newTest"]["importEdits"],
        json!([{
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
            "newText": "from shapes.circle import Circle\n",
        }]),
        "{answer:#}",
    );
    Ok(())
}

#[test]
fn a_module_and_a_package_are_named_from_their_search_path() -> Result<()> {
    let mut server = server()?;
    let mut name = |path: &str| {
        let uri = server.file_uri(SystemPath::new(path));
        server.send_request_await::<ModuleName>(json!({ "uri": uri }))
    };

    assert_eq!(
        name("project/src/shapes/circle.by").as_deref(),
        Some("shapes.circle")
    );
    assert_eq!(
        name("project/src/shapes/__init__.by").as_deref(),
        Some("shapes")
    );
    assert_eq!(name("project/src/shapes").as_deref(), Some("shapes"));
    assert_eq!(
        name("project/tests/test_circle.by").as_deref(),
        Some("tests.test_circle")
    );
    // the project root is the top of the module tree, not a module in it
    assert_eq!(name("project"), None);
    Ok(())
}
