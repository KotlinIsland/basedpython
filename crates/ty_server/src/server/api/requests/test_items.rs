//! `by/testItems` — the tests pytest would collect, without running pytest.
//!
//! Collecting tests the way pytest does — `pytest --collect-only` — imports every test module, and
//! importing a module runs whatever it does at the top level. That is a fine price for a user who
//! asked to collect; it is not one an editor should pay on a user's behalf just to draw a gutter
//! icon or a tree. The checker already models pytest's collection rules statically
//! ([`ty_python_semantic::collected_pytest_tests`]) — a test nested in a function is not one, a
//! `Test` class with an `__init__` holds none, a `unittest.TestCase` method is one whatever its
//! class is called — so this answers from that model and executes nothing.
//!
//! A standard `textDocument/documentSymbol` answer is not enough to derive this from: it lists
//! declarations, and which of them pytest collects depends on inheritance, constructors, fixtures
//! and `__test__` flags that only the checker can resolve.

use lsp_types::{LspRequestMethod, MessageDirection, Range, Request, Uri};
use ruff_db::files::File;
use ruff_text_size::TextRange;
use ty_project::{Db as _, ProjectDatabase};
use ty_python_semantic::{CollectedTest, Db as _, collected_pytest_tests};

use crate::PositionEncoding;
use crate::document::ToRangeExt;
use crate::server::api::program_files::project_file;
use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;
use crate::system::file_to_uri;

pub(crate) enum TestItemsRequest {}

impl Request for TestItemsRequest {
    type Params = TestItemsParams;
    type Result = Option<TestItemsResponse>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/testItems");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// Where to look.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TestItemsParams {
    /// One file — its buffer when it is open. Absent for every file of every project.
    #[serde(default)]
    uri: Option<Uri>,
}

/// The tests found, per file.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TestItemsResponse {
    /// For a request naming one file, that file, with an empty list when it holds no tests. For a
    /// whole-project request, only the files that hold some.
    files: Vec<TestFile>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TestFile {
    uri: Uri,
    /// The module-level classes and functions, in source order.
    tests: Vec<TestItem>,
}

/// A test, or a class tests are collected under.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TestItem {
    /// The name pytest reports it under.
    name: String,
    /// `class` or `function`.
    kind: String,
    /// The `::`-separated names after the file in the item's pytest node id: `TestA::test_b`.
    id: String,
    /// The whole declaration.
    range: Range,
    /// The name.
    selection_range: Range,
    /// Collected as a `unittest.TestCase` method, into which pytest injects no fixtures.
    unittest: bool,
    /// The classes and tests collected under a class, in source order; empty for a function.
    children: Vec<TestItem>,
}

pub(crate) struct TestItemsHandler;

impl RequestHandler for TestItemsHandler {
    type RequestType = TestItemsRequest;
}

impl BackgroundRequestHandler for TestItemsHandler {
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        params: TestItemsParams,
    ) -> crate::server::Result<Option<TestItemsResponse>> {
        let encoding = snapshot.position_encoding();
        if let Some(uri) = params.uri {
            let Some((db, file)) = project_file(snapshot, &uri) else {
                return Ok(None);
            };
            let tests = test_items(db, file, encoding);
            return Ok(Some(TestItemsResponse {
                files: vec![TestFile { uri, tests }],
            }));
        }

        let mut files = Vec::new();
        for db in snapshot.projects() {
            for file in &db.project().files(db) {
                let tests = test_items(db, file, encoding);
                if tests.is_empty() {
                    continue;
                }
                let Some(uri) = file_to_uri(db, file) else {
                    continue;
                };
                files.push(TestFile { uri, tests });
            }
        }
        files.sort_by(|a, b| a.uri.as_str().cmp(b.uri.as_str()));
        Ok(Some(TestItemsResponse { files }))
    }
}

impl RetriableRequestHandler for TestItemsHandler {}

/// The tests in `file`, arranged under the classes they are collected in.
fn test_items(db: &ProjectDatabase, file: File, encoding: PositionEncoding) -> Vec<TestItem> {
    let range = |range: TextRange| {
        range
            .to_lsp_range(db, file, encoding)
            .map(|range| range.local_range())
    };
    let mut roots: Vec<TestItem> = Vec::new();
    for test in collected_pytest_tests(db, db.program_file(file)) {
        let CollectedTest {
            name,
            classes,
            full_range,
            focus_range,
            is_unittest,
        } = test;
        let mut siblings = &mut roots;
        let mut id = String::new();
        let mut placed = true;
        for class in classes {
            if !id.is_empty() {
                id.push_str("::");
            }
            id.push_str(&class.name);
            let (Some(class_range), Some(class_selection)) =
                (range(class.full_range), range(class.focus_range))
            else {
                placed = false;
                break;
            };
            let index = match siblings.iter().position(|item| {
                item.kind == CLASS
                    && item.name == class.name
                    && item.selection_range == class_selection
            }) {
                Some(index) => index,
                None => {
                    siblings.push(TestItem {
                        name: class.name,
                        kind: CLASS.to_owned(),
                        id: id.clone(),
                        range: class_range,
                        selection_range: class_selection,
                        unittest: false,
                        children: Vec::new(),
                    });
                    siblings.len() - 1
                }
            };
            siblings = &mut siblings[index].children;
        }
        let (true, Some(test_range), Some(test_selection)) =
            (placed, range(full_range), range(focus_range))
        else {
            continue;
        };
        if !id.is_empty() {
            id.push_str("::");
        }
        id.push_str(&name);
        siblings.push(TestItem {
            name,
            kind: FUNCTION.to_owned(),
            id,
            range: test_range,
            selection_range: test_selection,
            unittest: is_unittest,
            children: Vec::new(),
        });
    }
    roots
}

const CLASS: &str = "class";
const FUNCTION: &str = "function";

#[cfg(test)]
mod tests {
    use super::*;

    /// Both forms a client sends: one file, and the whole project.
    #[test]
    fn the_params_a_client_sends_parse() {
        let one: TestItemsParams = serde_json::from_str(r#"{"uri":"file:///p/test_a.by"}"#)
            .expect("a client names a file");
        assert_eq!(one.uri.expect("a uri").as_str(), "file:///p/test_a.by");
        let all: TestItemsParams = serde_json::from_str("{}").expect("a client names nothing");
        assert!(all.uri.is_none());
    }
}
