//! `by/dataFlowAt` over the wire
//!
//! the unit tests in the handler cover which findings the analysis reaches. what they cannot cover
//! is the request itself: they build a `DataFlowParams` from typed values, and the contract with a
//! client is json. a serde attribute that makes the typed value unreachable from json is invisible
//! from inside the crate and total from outside it, so the params here are written as a client
//! would send them and never constructed

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::TestServerBuilder;

/// the request as a client sends it, in json throughout
///
/// deliberately not the server's own `DataFlowParams`: that type is private to `ty_server`, and
/// borrowing it would make this test agree with the server by construction rather than by the
/// json actually lining up
enum DataFlowAt {}

impl Request for DataFlowAt {
    type Params = serde_json::Value;
    type Result = Option<Vec<serde_json::Value>>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/dataFlowAt");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// a module whose branch nothing in the source can decide
const CONTENT: &str = "\
def compute() -> int:
    return 0

limit = compute()
if limit > 100:
    over = 1
";

#[test]
fn an_observation_sent_as_json_settles_a_branch() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(workspace_root, None)?
        .with_file(foo, CONTENT)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, CONTENT, 1);

    let findings = server
        .send_request_await::<DataFlowAt>(serde_json::json!({
            "textDocument": { "uri": server.file_uri(foo) },
            // stopped on the `if`, which has not run yet
            "line": 5,
            "observations": [
                { "name": "limit", "observed": "isInt", "text": "5" }
            ],
        }))
        .expect("the server answers a file it is checking");

    let labels: Vec<_> = findings
        .iter()
        .map(|finding| {
            (
                finding["kind"].as_str().unwrap_or_default().to_string(),
                finding["label"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();

    assert!(
        labels.contains(&("condition".to_string(), "= false".to_string())),
        "the branch should have been settled, and the server answered {labels:?}"
    );

    Ok(())
}

#[test]
fn the_same_request_with_nothing_observed_settles_nothing() -> Result<()> {
    // the control. if this ever answers, the server is reporting ordinary static analysis as
    // though a debugger had produced it
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(workspace_root, None)?
        .with_file(foo, CONTENT)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, CONTENT, 1);

    let findings = server
        .send_request_await::<DataFlowAt>(serde_json::json!({
            "textDocument": { "uri": server.file_uri(foo) },
            "line": 5,
            "observations": [],
        }))
        .expect("the server answers a file it is checking");

    assert!(findings.is_empty(), "the server answered {findings:?}");

    Ok(())
}

/// the function a user reported the value half missing from
///
/// a `.by` file rather than a `.py` one, because that is the only kind a debug session asks about
/// and it is load-bearing here: basedpython gives a float literal a literal type and python does
/// not, so `discount` at the return is `float` in a `.py` file and `0.0` in this one
const PRICE: &str = "\
def price(qty: int, member: bool):
    discount = 0.0
    if qty >= 10:
        discount = 0.1
    if member:
        discount += 0.05
    return discount
";

#[test]
fn the_value_a_name_will_hold_crosses_the_wire_with_its_own_kind() -> Result<()> {
    // the value finding carries a string where a condition carries a bool, so it is the one shape
    // in the reply that nothing else exercises. the client reads `kind` to decide how to draw it,
    // which is exactly the field a serde attribute can break with no test inside the crate noticing
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.by");

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(workspace_root, None)?
        .with_file(foo, PRICE)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, PRICE, 1);

    let findings = server
        .send_request_await::<DataFlowAt>(serde_json::json!({
            "textDocument": { "uri": server.file_uri(foo) },
            // the first statement of the body — the stop that used to answer nothing at all
            "line": 2,
            "observations": [
                { "name": "qty", "observed": "isInt", "text": "3" },
                { "name": "member", "observed": "isBool", "value": false },
            ],
        }))
        .expect("the server answers a file it is checking");

    let value = findings
        .iter()
        .find(|finding| finding["kind"] == "value")
        .unwrap_or_else(|| {
            panic!("neither `if` runs, so `return discount` finds line 2's 0.0: {findings:?}")
        });

    assert_eq!(value["value"], "0.0", "the whole finding was {value:?}");
    assert_eq!(
        value["label"], "discount = 0.0",
        "the label names the name, because a client draws it in the margin and not at the read"
    );
    assert!(
        value["taken"].is_null(),
        "a value is not a branch, and a client keying off `taken` must see nothing: {value:?}"
    );

    Ok(())
}
