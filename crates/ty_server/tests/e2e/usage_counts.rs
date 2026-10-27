//! `by/documentUsageCounts` over the wire
//!
//! what counts as a usage or an override is `ty_ide`'s and its unit tests cover it. what these
//! cover is the contract a client reads: each declaration with its ranges, only the counts asked
//! for, and each count the number of places `textDocument/references` or
//! `textDocument/implementation` goes to from the declaration's name, which is what a client asks
//! when the count is clicked

use std::time::Duration;

use anyhow::Result;
use lsp_server::ErrorCode;
use lsp_types::{
    Definition, ImplementationParams, ImplementationRequest, ImplementationResponse,
    LspRequestMethod, MessageDirection, PartialResultParams, Position, ReferenceContext,
    ReferenceParams, ReferencesRequest, Request, TextDocumentIdentifier,
    TextDocumentPositionParams, WorkDoneProgressParams,
};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::{AwaitResponseError, TestServer, TestServerBuilder};

/// the request as a client sends it, in json throughout
enum DocumentUsageCounts {}

impl Request for DocumentUsageCounts {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/documentUsageCounts");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const SHAPES: &str = "\
from abc import ABC, abstractmethod

class Shape(ABC):
    @abstractmethod
    def area(self) -> float: ...

    def describe(self) -> str:
        return 'a shape'

def total(shapes: list[Shape]) -> float:
    return sum(shape.area() for shape in shapes)
";

const MAIN: &str = "\
from shapes import Shape, total

class Square(Shape):
    def area(self) -> float:
        return 1.0

print(total([Square()]), Square().describe())
";

fn server() -> Result<TestServer> {
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/shapes.by"), SHAPES)?
        .with_file(SystemPath::new("src/main.by"), MAIN)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/shapes.by"), SHAPES, 1);
    Ok(server)
}

fn ask(server: &mut TestServer, counts: &[&str]) -> Vec<serde_json::Value> {
    let uri = server.file_uri(SystemPath::new("src/shapes.by"));
    server
        .send_request_await::<DocumentUsageCounts>(serde_json::json!({
            "textDocument": { "uri": uri },
            "counts": counts,
        }))
        .expect("an answer for a document")
        .as_array()
        .expect("a list of declarations")
        .clone()
}

fn position(declaration: &serde_json::Value) -> Position {
    serde_json::from_value(declaration["selectionRange"]["start"].clone()).expect("a position")
}

#[test]
fn each_declaration_comes_with_its_ranges_and_kind() -> Result<()> {
    let mut server = server()?;

    let declarations = ask(&mut server, &[]);
    let names: Vec<_> = declarations
        .iter()
        .map(|declaration| {
            format!(
                "{} {}",
                declaration["kind"].as_str().unwrap_or_default(),
                declaration["name"].as_str().unwrap_or_default()
            )
        })
        .collect();
    assert_eq!(
        names,
        [
            "class Shape",
            "method area",
            "method describe",
            "function total"
        ]
    );

    // a decorated method's range starts at its decorator, and its name is where it is asked about
    let area = &declarations[1];
    assert_eq!(
        area["range"]["start"],
        serde_json::json!({ "line": 3, "character": 4 })
    );
    assert_eq!(
        area["selectionRange"],
        serde_json::json!({
            "start": { "line": 4, "character": 8 },
            "end": { "line": 4, "character": 12 },
        }),
    );
    // nothing asked for, nothing counted
    assert_eq!(area.get("references"), None);
    assert_eq!(area.get("implementations"), None);
    Ok(())
}

#[test]
fn only_the_counts_asked_for_are_worked_out() -> Result<()> {
    let mut server = server()?;

    for declaration in ask(&mut server, &["references"]) {
        assert!(declaration["references"].is_number(), "{declaration:#}");
        assert_eq!(declaration.get("implementations"), None, "{declaration:#}");
    }
    for declaration in ask(&mut server, &["implementations"]) {
        assert_eq!(declaration.get("references"), None, "{declaration:#}");
        // a function is neither subclassed nor overridden
        assert_eq!(
            declaration["implementations"].is_number(),
            declaration["kind"] != "function",
            "{declaration:#}"
        );
    }
    Ok(())
}

#[test]
fn a_count_is_the_number_of_places_the_request_it_stands_for_goes_to() -> Result<()> {
    let mut server = server()?;
    let uri = server.file_uri(SystemPath::new("src/shapes.by"));

    let declarations = ask(&mut server, &["references", "implementations"]);
    let counts: Vec<_> = declarations
        .iter()
        .map(|declaration| {
            (
                declaration["name"].as_str().unwrap_or_default().to_string(),
                declaration["references"].as_u64(),
                declaration["implementations"].as_u64(),
                declaration["abstract"].as_bool(),
            )
        })
        .collect();
    assert_eq!(
        counts,
        [
            ("Shape".to_string(), Some(3), Some(1), Some(false)),
            ("area".to_string(), Some(1), Some(1), Some(true)),
            ("describe".to_string(), Some(1), Some(0), Some(false)),
            ("total".to_string(), Some(2), None, Some(false)),
        ]
    );

    for declaration in &declarations {
        let at = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: position(declaration),
        };
        let references = server
            .send_request_await::<ReferencesRequest>(ReferenceParams {
                text_document_position_params: at.clone(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: ReferenceContext {
                    include_declaration: false,
                },
            })
            .unwrap_or_default();
        assert_eq!(
            declaration["references"].as_u64(),
            Some(references.len() as u64),
            "{declaration:#}"
        );

        if declaration["kind"] == "function" {
            continue;
        }
        let implementations =
            match server.send_request_await::<ImplementationRequest>(ImplementationParams {
                text_document_position_params: at.clone(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            }) {
                Some(ImplementationResponse::Definition(Definition::Location(location))) => {
                    vec![(location.uri, location.range.start)]
                }
                Some(ImplementationResponse::Definition(Definition::LocationList(locations))) => {
                    locations
                        .into_iter()
                        .map(|location| (location.uri, location.range.start))
                        .collect()
                }
                Some(ImplementationResponse::DefinitionLinkList(links)) => links
                    .into_iter()
                    .map(|link| (link.target_uri, link.target_selection_range.start))
                    .collect(),
                None => Vec::new(),
            };
        // less the declaration itself, which is the first of its own implementations
        let others: Vec<_> = implementations
            .into_iter()
            .filter(|(target, start)| *target != uri || *start != at.position)
            .collect();
        assert_eq!(
            declaration["implementations"].as_u64(),
            Some(others.len() as u64),
            "{declaration:#}"
        );
        // and the implementors are those places, in that order
        let implementors: Vec<_> = declaration["implementors"]
            .as_array()
            .expect("implementors with implementations")
            .iter()
            .map(|implementor| {
                (
                    serde_json::from_value::<lsp_types::Uri>(implementor["uri"].clone())
                        .expect("a uri"),
                    serde_json::from_value::<Position>(
                        implementor["selectionRange"]["start"].clone(),
                    )
                    .expect("a position"),
                )
            })
            .collect();
        assert_eq!(implementors, others, "{declaration:#}");
    }
    Ok(())
}

/// each subclass of a class, and each member overriding a method, is named by its class, so that
/// a client can say which classes they are without asking where each count goes
#[test]
fn implementors_are_named_by_their_class() -> Result<()> {
    let mut server = server()?;

    let declarations = ask(&mut server, &["implementations"]);
    let implementors: Vec<_> = declarations
        .iter()
        .map(|declaration| {
            (
                declaration["name"].as_str().unwrap_or_default().to_string(),
                declaration.get("implementors").map(|implementors| {
                    implementors
                        .as_array()
                        .expect("a list")
                        .iter()
                        .map(|implementor| {
                            format!(
                                "{} {} {}:{}",
                                implementor["className"].as_str().unwrap_or_default(),
                                implementor["qualifiedName"].as_str().unwrap_or_default(),
                                implementor["selectionRange"]["start"]["line"],
                                implementor["range"]["start"]["line"],
                            )
                        })
                        .collect::<Vec<_>>()
                }),
            )
        })
        .collect();
    assert_eq!(
        implementors,
        [
            (
                "Shape".to_string(),
                Some(vec!["Square main.Square 2:2".to_string()])
            ),
            (
                "area".to_string(),
                Some(vec!["Square main.Square 3:3".to_string()])
            ),
            ("describe".to_string(), Some(vec![])),
            ("total".to_string(), None),
        ]
    );

    // asked for references only, no implementors
    for declaration in ask(&mut server, &["references"]) {
        assert_eq!(declaration.get("implementors"), None, "{declaration:#}");
    }
    Ok(())
}

/// a search of the workspace the client gives up on, most often because the pass that asked was
/// cancelled by a keystroke, is stopped where it is: it is answered once, as cancelled, and the
/// count asked next, which waits for the search lane, is answered
#[test]
fn a_count_the_client_cancels_while_it_runs_is_answered_once_and_the_next_is_answered() -> Result<()>
{
    const MODULES: usize = 200;

    let mut builder = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/shapes.by"), SHAPES)?
        .with_file(SystemPath::new("src/main.by"), MAIN)?;
    for module in 0..MODULES {
        builder = builder.with_file(
            SystemPath::new(&format!("src/user{module}.by")),
            format!(
                "from shapes import Shape, total\n\nclass Shape{module}(Shape):\n    def area(self) -> float:\n        return {module}.0\n\nprint(total([Shape{module}()]), Shape{module}().describe())\n"
            ),
        )?;
    }
    let mut server = builder.build().wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/shapes.by"), SHAPES, 1);
    let uri = server.file_uri(SystemPath::new("src/shapes.by"));
    let params = serde_json::json!({
        "textDocument": { "uri": uri },
        "counts": ["references", "implementations"],
    });

    let cancelled = server.send_request::<DocumentUsageCounts>(params.clone());
    server.cancel(&cancelled);
    match server.try_await_response::<DocumentUsageCounts>(&cancelled, None) {
        Err(AwaitResponseError::RequestFailed(error)) => {
            assert_eq!(error.code, ErrorCode::RequestCanceled as i32);
        }
        other => panic!("expected the cancellation, got {other:?}"),
    }

    let declarations = server
        .send_request_await::<DocumentUsageCounts>(params)
        .expect("an answer for a document");
    let shape = &declarations.as_array().expect("a list of declarations")[0];
    assert_eq!(shape["name"], "Shape");
    // `total` names it, and `main.by` and every module import it and subclass it
    assert_eq!(shape["references"].as_u64(), Some(2 * MODULES as u64 + 3));
    assert_eq!(shape["implementations"].as_u64(), Some(MODULES as u64 + 1));

    // and the cancelled count does not bring a second answer
    assert!(matches!(
        server.try_await_response::<DocumentUsageCounts>(
            &cancelled,
            Some(Duration::from_millis(500))
        ),
        Err(AwaitResponseError::Timeout)
    ));
    Ok(())
}
