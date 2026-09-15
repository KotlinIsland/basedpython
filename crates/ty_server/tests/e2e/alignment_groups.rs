//! `by/alignmentGroups` over the wire
//!
//! which lines make a group is `ty_ide::alignment_groups`'s; what these cover is that a client
//! gets that grouping decided in display columns at the tab size it sent, and gets those columns
//! back beside positions that stay in the negotiated encoding

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::TestServerBuilder;

/// the request as a client sends it, in json throughout
enum AlignmentGroups {}

impl Request for AlignmentGroups {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/alignmentGroups");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// `名前` is four columns wide as drawn and two utf-16 code units long, and `x:\tint` reaches
/// column seven at a tab size of four and column eleven at eight
const SOURCE: &str = "\
名前 = [1]
abcd = [2]

x:\tint = 1
abcdefg = 2
";

fn groups(tab_size: u32) -> Result<serde_json::Value> {
    let main = SystemPath::new("src/main.py");

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(main, SOURCE)?
        .enable_inlay_hints(true)
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(main, SOURCE, 1);

    let uri = server.file_uri(main);
    Ok(server
        .send_request_await::<AlignmentGroups>(serde_json::json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 5, "character": 0 },
            },
            "tabSize": tab_size,
        }))
        .expect("a document with inlay hints enabled is answered"))
}

#[test]
fn columns_are_display_columns_at_the_clients_tab_size() -> Result<()> {
    insta::assert_json_snapshot!(groups(4)?, @r#"
    [
      {
        "members": [
          {
            "gapEnd": {
              "character": 3,
              "line": 0
            },
            "gapEndColumn": 5,
            "gapStart": {
              "character": 2,
              "line": 0
            },
            "gapStartColumn": 4
          },
          {
            "gapEnd": {
              "character": 5,
              "line": 1
            },
            "gapEndColumn": 5,
            "gapStart": {
              "character": 4,
              "line": 1
            },
            "gapStartColumn": 4
          }
        ]
      },
      {
        "members": [
          {
            "gapEnd": {
              "character": 7,
              "line": 3
            },
            "gapEndColumn": 8,
            "gapStart": {
              "character": 6,
              "line": 3
            },
            "gapStartColumn": 7
          },
          {
            "gapEnd": {
              "character": 8,
              "line": 4
            },
            "gapEndColumn": 8,
            "gapStart": {
              "character": 7,
              "line": 4
            },
            "gapStartColumn": 7
          }
        ]
      }
    ]
    "#);
    Ok(())
}

/// the same lines at a tab size of eight: the tabbed `=` moves out of its neighbour's column and
/// that pair stops being a group, while the pair with no tab in it is untouched
#[test]
fn a_different_tab_size_moves_a_tabbed_column() -> Result<()> {
    let answer = groups(8)?;
    let groups = answer.as_array().expect("an array of groups");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["members"][0]["gapStart"]["line"], 0);
    Ok(())
}

#[test]
fn a_request_without_a_tab_size_is_refused() -> Result<()> {
    let main = SystemPath::new("src/main.py");

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(main, SOURCE)?
        .enable_inlay_hints(true)
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(main, SOURCE, 1);

    let uri = server.file_uri(main);
    let id = server.send_request::<AlignmentGroups>(serde_json::json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 5, "character": 0 },
        },
    }));
    match server.try_await_response::<AlignmentGroups>(&id, None) {
        Err(crate::AwaitResponseError::RequestFailed(error)) => {
            assert_eq!(error.code, lsp_server::ErrorCode::InvalidParams as i32);
        }
        other => panic!("expected the request to be refused, got {other:?}"),
    }
    Ok(())
}
