//! `by/inlayHintCatalog`: every kind of inlay hint, with an example drawn, for an editor to list
//! them before any file is open

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use serde_json::{Value, json};

use crate::{AwaitResponseError, TestServerBuilder};

enum InlayHintCatalogRequest {}

impl Request for InlayHintCatalogRequest {
    type Params = Value;
    // read as json rather than as the server's own type, so the shape a client depends on is
    // what is asserted
    type Result = Value;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/inlayHintCatalog");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

fn kind<'a>(catalog: &'a Value, setting: &str) -> &'a Value {
    catalog["kinds"]
        .as_array()
        .expect("kinds")
        .iter()
        .find(|kind| kind["setting"] == setting)
        .unwrap_or_else(|| panic!("no {setting}"))
}

/// the hints of `kind`'s example that are tagged as its own, as a file's hints are
fn own_hints(kind: &Value) -> Vec<&Value> {
    kind["example"]["hints"]
        .as_array()
        .expect("hints")
        .iter()
        .filter(|hint| hint["data"]["kind"] == kind["setting"])
        .collect()
}

/// the kinds come grouped, each with its example drawn in the shape `textDocument/inlayHint`
/// answers in, whatever params a client sends and whatever the project
#[test]
fn every_kind_is_listed_with_its_example_drawn() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .build()
        .wait_until_workspaces_are_initialized();

    for params in [Value::Null, json!({})] {
        let catalog = server.send_request_await::<InlayHintCatalogRequest>(params);

        let groups: Vec<&str> = catalog["groups"]
            .as_array()
            .expect("groups")
            .iter()
            .map(|group| group["id"].as_str().expect("an id"))
            .collect();
        assert_eq!(
            groups,
            [
                "types",
                "generics",
                "arguments",
                "effects",
                "ui",
                "templates"
            ]
        );
        assert_eq!(catalog["groups"][0]["title"], "Types");

        // every kind's group is listed, and every source kind's example shows its own hint
        for kind in catalog["kinds"].as_array().expect("kinds") {
            let group = kind["group"].as_str().expect("a group");
            assert!(groups.contains(&group), "{kind}");
            match kind["fileKind"].as_str() {
                Some("source") => assert!(!own_hints(kind).is_empty(), "{}", kind["setting"]),
                Some("template") => {}
                other => panic!("a file kind of {other:?}"),
            }
            assert!(matches!(
                kind["attachesTo"].as_str(),
                Some("preceding" | "following")
            ));
        }

        let returns = kind(&catalog, "returnTypes");
        assert_eq!(returns["title"], "Return types");
        assert_eq!(returns["group"], "types");
        assert_eq!(returns["attachesTo"], "preceding");
        assert_eq!(returns["fileKind"], "source");
        assert_eq!(
            returns["example"]["source"],
            "def area(r: float):\n    return 3.14 * r * r\n"
        );
        assert_eq!(
            own_hints(returns),
            [&json!({
                "position": { "line": 0, "character": 18 },
                "label": [{ "value": "-> " }, { "value": "float" }],
                "kind": 1,
                "paddingLeft": true,
                "data": { "kind": "returnTypes" },
            })]
        );

        let arguments = kind(&catalog, "callArgumentNames");
        assert_eq!(arguments["attachesTo"], "following");
        assert_eq!(arguments["group"], "arguments");
        // the hints a parameter is shown as, as a file's are
        assert!(own_hints(arguments).iter().all(|hint| hint["kind"] == 2));

        // an example draws the other kinds it shows, each tagged as its own
        let variables = kind(&catalog, "variableTypes");
        assert!(
            variables["example"]["hints"]
                .as_array()
                .expect("hints")
                .iter()
                .any(|hint| hint["data"]["kind"] == "returnTypes")
        );

        let templates = kind(&catalog, "resolvedTemplates");
        assert_eq!(templates["fileKind"], "template");
        assert!(templates["example"].is_null());
    }

    Ok(())
}

/// the params are nothing or an empty object; anything else is a client asking for something
/// this request does not do
#[test]
fn params_other_than_none_are_refused() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .build()
        .wait_until_workspaces_are_initialized();

    let id = server.send_request::<InlayHintCatalogRequest>(json!({ "kinds": ["returnTypes"] }));
    let response = server.try_await_response::<InlayHintCatalogRequest>(&id, None);
    let Err(AwaitResponseError::RequestFailed(error)) = response else {
        anyhow::bail!("expected a request error, got {response:?}");
    };
    assert_eq!(error.code, lsp_server::ErrorCode::InvalidParams as i32);

    Ok(())
}
