//! `by/testSubjects` and `by/subjectTests`: which tests exercise which code
//!
//! what an editor's *go to test subject* and *go to test* ask, from a test to the code it calls
//! and from code to the tests that call it. a test exercises what it calls, as call hierarchy sees
//! a call, and which declarations are tests is pytest's collection as `by/testItems` reports it:
//! nothing goes by file names. `by/subjectTests` also says where a new test of the code would go,
//! for an editor's *create test*
//!
//! both answer about the project's files as they are, a document the client has not opened
//! included, and read no version: what a test calls is in other files too

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Range, Request, SymbolKind,
    TextDocumentIdentifier, TextEdit, Uri,
};
use ruff_text_size::{Ranged, TextRange};
use ty_ide::{SubjectTests, TestLink, subject_tests, test_subjects};
use ty_project::{Db as _, ProjectDatabase, SemanticDb as _};

use crate::PositionEncoding;
use crate::document::{PositionExt, ToRangeExt};
use crate::server::api::symbols::convert_symbol_kind;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;
use crate::system::file_to_uri;

pub(crate) enum TestSubjectsRequest {}

impl Request for TestSubjectsRequest {
    type Params = TestLinksParams;
    type Result = Option<Vec<TestLinkItem>>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/testSubjects");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

pub(crate) enum SubjectTestsRequest {}

impl Request for SubjectTestsRequest {
    type Params = TestLinksParams;
    type Result = Option<SubjectTestsResponse>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/subjectTests");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// where to start from: for `by/testSubjects` a test, a class tests are collected under or anywhere
/// in a module tests are collected from; for `by/subjectTests` a function, a method, a class, or
/// anywhere else in a module, which stands for everything the module declares
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TestLinksParams {
    text_document: TextDocumentIdentifier,
    position: Position,
}

/// a test, or code a test exercises
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TestLinkItem {
    /// for a test, the `::`-separated names after the file in its pytest node id,
    /// `TestTotal::test_total`; for code under test, its name in its module, with the classes it is
    /// declared in, `Circle.area`
    name: String,
    kind: SymbolKind,
    /// the module it is declared in
    #[serde(skip_serializing_if = "Option::is_none")]
    module: Option<String>,
    uri: Uri,
    /// the whole declaration
    range: Range,
    /// its name, which is where to go
    selection_range: Range,
}

impl TestLinkItem {
    fn from_ide(db: &ProjectDatabase, link: TestLink, encoding: PositionEncoding) -> Option<Self> {
        let range = |range: TextRange| {
            range
                .to_lsp_range(db, link.file, encoding)
                .map(|range| range.local_range())
        };
        Some(TestLinkItem {
            uri: file_to_uri(db, link.file)?,
            range: range(link.full_range)?,
            selection_range: range(link.focus_range)?,
            name: link.name,
            kind: convert_symbol_kind(link.kind),
            module: link.module,
        })
    }
}

/// the tests that exercise the code, and how to add one
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubjectTestsResponse {
    /// the collected tests that call the code, by file and then in source order
    tests: Vec<TestLinkItem>,
    /// where a new test of the code goes; `null` when its module has no name a test could import
    /// it by, or is a test module itself
    new_test: Option<NewTestItem>,
}

/// where a new test goes, and what it imports
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewTestItem {
    /// the test module: `test_<module>`, in the directory most of the project's test modules
    /// are in, or in `tests` under the project root when there are none; the one there already
    /// whatever it is written in, or a new one written in what the subject is
    uri: Uri,
    /// whether the test module exists already, so that a new test is added to it rather than
    /// created with it
    exists: bool,
    /// the module the code is in
    module: String,
    /// the name a test imports from `module`: the function's or the class's, or for a method its
    /// class's. absent for a module, which a test imports whole
    #[serde(skip_serializing_if = "Option::is_none")]
    import: Option<String>,
    /// a name for the test function, `test_` and the code's name in snake case
    test_name: String,
    /// for a test module that exists, the edits that import `import` into it, empty when it does
    /// already
    import_edits: Vec<TextEdit>,
}

impl NewTestItem {
    fn from_ide(
        db: &ProjectDatabase,
        new_test: ty_ide::NewTest,
        encoding: PositionEncoding,
    ) -> Option<Self> {
        let uri = Uri::from_file_path(new_test.path.as_std_path()).ok()?;
        let import_edits = match &new_test.import_edit {
            None => Vec::new(),
            Some(edit) => {
                let file = ruff_db::files::system_path_to_file(db, &new_test.path).ok()?;
                vec![TextEdit {
                    range: edit.range().to_lsp_range(db, file, encoding)?.local_range(),
                    new_text: edit.content().unwrap_or_default().to_string(),
                }]
            }
        };
        Some(NewTestItem {
            uri,
            exists: new_test.exists,
            module: new_test.module,
            import: new_test.import,
            test_name: new_test.test_name,
            import_edits,
        })
    }
}

pub(crate) struct TestSubjectsRequestHandler;

impl RequestHandler for TestSubjectsRequestHandler {
    type RequestType = TestSubjectsRequest;
}

impl BackgroundDocumentRequestHandler for TestSubjectsRequestHandler {
    const ANSWERS_CLOSED_DOCUMENTS: bool = true;

    fn document_uri(params: &TestLinksParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off, for a template, and when no test is collected from
    /// the document; otherwise what the tests at the position call, each once, in the order they
    /// first call it, and an empty list when that is nothing of the project's own
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: TestLinksParams,
    ) -> crate::server::Result<Option<Vec<TestLinkItem>>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
            || snapshot.is_django_template()
        {
            return Ok(None);
        }
        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(None);
        };
        let Some(offset) =
            params
                .position
                .to_text_size(db, file, snapshot.document().uri(), snapshot.encoding())
        else {
            return Ok(None);
        };
        Ok(
            test_subjects(db, db.program_file(file), offset).map(|subjects| {
                subjects
                    .into_iter()
                    .filter_map(|link| TestLinkItem::from_ide(db, link, snapshot.encoding()))
                    .collect()
            }),
        )
    }
}

impl RetriableRequestHandler for TestSubjectsRequestHandler {}

pub(crate) struct SubjectTestsRequestHandler;

impl RequestHandler for SubjectTestsRequestHandler {
    type RequestType = SubjectTestsRequest;
}

impl BackgroundDocumentRequestHandler for SubjectTestsRequestHandler {
    const ANSWERS_CLOSED_DOCUMENTS: bool = true;

    fn document_uri(params: &TestLinksParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off, for a template, and for a file outside the project,
    /// whose code no test of the project's is looked for
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: TestLinksParams,
    ) -> crate::server::Result<Option<SubjectTestsResponse>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
            || snapshot.is_django_template()
        {
            return Ok(None);
        }
        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(None);
        };
        if !db.project().files(db).contains(file) {
            return Ok(None);
        }
        let Some(offset) =
            params
                .position
                .to_text_size(db, file, snapshot.document().uri(), snapshot.encoding())
        else {
            return Ok(None);
        };
        let SubjectTests { tests, new_test } = subject_tests(db, db.program_file(file), offset);
        Ok(Some(SubjectTestsResponse {
            tests: tests
                .into_iter()
                .filter_map(|link| TestLinkItem::from_ide(db, link, snapshot.encoding()))
                .collect(),
            new_test: new_test
                .and_then(|new_test| NewTestItem::from_ide(db, new_test, snapshot.encoding())),
        }))
    }
}

impl RetriableRequestHandler for SubjectTestsRequestHandler {}
