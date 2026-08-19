//! `by/injections` over the wire
//!
//! the unit tests in `ty_ide` cover which fragments the analysis finds. what they cannot cover is
//! the request itself: they work in `TextRange` over one in-memory file, and the contract with a
//! client is json, in lsp line/character positions, for a document the client opened. an off-by-one
//! in that conversion puts an editor's injected fragment one character out of place, which is
//! invisible from inside the crate and total from outside it
//!
//! so the params here are written as a client would send them and never constructed from the
//! server's own types

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::TestServerBuilder;

/// the request as a client sends it, in json throughout
enum Injections {}

impl Request for Injections {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/injections");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// one fragment, flattened to what a client actually reads off it
#[derive(Debug, PartialEq, Eq)]
struct Fragment {
    language: String,
    origin: String,
    /// each range as (line, start character, end character), which is enough while no fragment
    /// here spans a line
    ranges: Vec<(u64, u64, u64)>,
}

fn fragments(answer: &serde_json::Value) -> Vec<Fragment> {
    answer["injections"]
        .as_array()
        .expect("the response to carry a list of injections")
        .iter()
        .map(|injection| Fragment {
            language: injection["language"].as_str().unwrap_or_default().to_string(),
            origin: injection["origin"].as_str().unwrap_or_default().to_string(),
            ranges: injection["ranges"]
                .as_array()
                .expect("a fragment to cover something")
                .iter()
                .map(|range| {
                    (
                        range["start"]["line"].as_u64().unwrap_or_default(),
                        range["start"]["character"].as_u64().unwrap_or_default(),
                        range["end"]["character"].as_u64().unwrap_or_default(),
                    )
                })
                .collect(),
        })
        .collect()
}

/// ask about a document the server has open, and flatten the answer
fn injections_in(content: &str) -> Result<Vec<Fragment>> {
    let workspace_root = SystemPath::new("src");
    let main = SystemPath::new("src/main.by");

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(ClientOptions::default())
        .with_workspace(workspace_root, None)?
        .with_file(main, content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(main, content, 1);

    let answer = server
        .send_request_await::<Injections>(serde_json::json!({
            "textDocument": { "uri": server.file_uri(main) },
        }))
        .expect("the server answers a file it is checking");

    Ok(fragments(&answer))
}

#[test]
fn a_marked_statement_reaches_the_client_as_a_fragment() -> Result<()> {
    let found = injections_in(
        "\
# language=javascript
script = \"const x = 1\"
",
    )?;

    assert_eq!(
        found,
        vec![Fragment {
            language: "javascript".to_string(),
            origin: "comment".to_string(),
            // line 1, inside the quotes
            ranges: vec![(1, 10, 21)],
        }]
    );

    Ok(())
}

#[test]
fn a_language_the_server_knows_nothing_about_is_carried_through_untouched() -> Result<()> {
    // the point of the whole contract: `by` has no idea what elvish is, and says so anyway
    let found = injections_in(
        "\
# language=elvish
spell = \"mellon\"
",
    )?;

    assert_eq!(
        found,
        vec![Fragment {
            language: "elvish".to_string(),
            origin: "comment".to_string(),
            ranges: vec![(1, 9, 15)],
        }]
    );

    Ok(())
}

#[test]
fn the_language_a_parameter_declares_travels_to_the_call_that_supplies_the_string() -> Result<()> {
    let found = injections_in(
        "\
from typing import Annotated

def f1(s: Annotated[str, \"language=basedpython\"]): ...

def f2(s: str):
    f1(s)

f2(\"None\")
",
    )?;

    assert_eq!(
        found,
        vec![Fragment {
            language: "basedpython".to_string(),
            origin: "propagated".to_string(),
            ranges: vec![(7, 4, 8)],
        }]
    );

    Ok(())
}

#[test]
fn a_fragment_written_as_two_literals_arrives_as_two_ranges() -> Result<()> {
    let found = injections_in(
        "\
# language=sql
query = \"SELECT *\" \" FROM t\"
",
    )?;

    assert_eq!(
        found,
        vec![Fragment {
            language: "sql".to_string(),
            origin: "comment".to_string(),
            ranges: vec![(1, 9, 17), (1, 20, 27)],
        }]
    );

    Ok(())
}

#[test]
fn a_file_with_nothing_marked_answers_with_an_empty_list() -> Result<()> {
    // an answer, not a miss: a client that cannot tell "no fragments" from "no server" has to
    // guess whether to leave its existing injections in place
    let found = injections_in(
        "\
script = \"const x = 1\"
",
    )?;

    assert_eq!(found, Vec::new());

    Ok(())
}
