use anyhow::Result;
use ruff_db::system::SystemPath;

use crate::TestServerBuilder;
use crate::notebook::NotebookBuilder;

#[test]
fn folding_range_basic_functionality() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = r#"class MyClass:
    def __init__(self):
        self.value = 1

    def method(self):
        return self.value
"#;

    let mut server = TestServerBuilder::new()?
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let ranges = server.folding_range_request(&server.file_uri(foo));

    insta::assert_json_snapshot!(ranges);

    Ok(())
}

/// a file with a range of every kind
const KINDS_SOURCE: &str = r#""""Module docs.

More module docs.
"""
import os
import sys

# region Helpers
# a block of
# comments


def process(items: list[int]) -> int:
    """Process the items.

    Returns the total.
    """
    return sum(items)


class Shape:
    def area(self) -> float:
        return 0.0


# endregion

TEXT = """
long
"""

NUMBERS = [
    1,
    2,
]
"#;

/// a client that lists basedpython's own kinds, shows `collapsedText` and folds within a line
/// is told every range's kind, what it folds to, and gets a range for every annotation, on one
/// line or not
#[test]
fn folding_range_kinds_a_client_asks_for() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");

    let mut server = TestServerBuilder::new()?
        .with_folding_range_capabilities(
            false,
            true,
            &[
                "comment",
                "imports",
                "region",
                "docstring",
                "function",
                "class",
                "string",
                "collection",
                "annotation",
            ],
        )
        .with_workspace(workspace_root, None)?
        .with_file(foo, KINDS_SOURCE)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, KINDS_SOURCE, 1);

    let ranges = server.folding_range_request(&server.file_uri(foo));

    insta::assert_json_snapshot!(ranges);

    Ok(())
}

/// a client that folds whole lines, lists only the kinds LSP defines and does not show
/// `collapsedText` gets the ranges it can draw: a docstring as a comment, the others of
/// basedpython's own kinds with no kind, no range on a single line, and no `collapsedText`
#[test]
fn folding_range_kinds_a_client_does_not_ask_for() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");

    let mut server = TestServerBuilder::new()?
        .with_folding_range_capabilities(true, false, &["comment", "imports", "region"])
        .with_workspace(workspace_root, None)?
        .with_file(foo, KINDS_SOURCE)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, KINDS_SOURCE, 1);

    let ranges = server.folding_range_request(&server.file_uri(foo));

    insta::assert_json_snapshot!(ranges);

    Ok(())
}

#[test]
fn folding_range_multiline_block_headers() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = r#"def foo(
    x: int,
    y: str,
) -> None:
    pass

match value:
    case {
        "kind": kind,
        "payload": payload,
    }:
        handle_mapping()
"#;

    let mut server = TestServerBuilder::new()?
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let ranges = server.folding_range_request(&server.file_uri(foo));

    insta::assert_json_snapshot!(ranges);

    Ok(())
}

#[test]
fn folding_range_notebook_cells_are_filtered_to_the_requested_cell() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .build()
        .wait_until_workspaces_are_initialized();

    server.initialization_result().unwrap();

    let mut builder = NotebookBuilder::virtual_file("test.ipynb");
    let first_cell = builder.add_python_cell(
        r#"import os
import zipfile

out_dir = "output"
archive = "archive.zip"

with zipfile.ZipFile(archive, "r") as zip_ref:
    for file_info in zip_ref.infolist():
        out_path = os.path.join(out_dir, file_info.filename)
        if file_info.file_size == 0:
            pass
        else:
            if os.path.exists(out_path):
                pass
"#,
    );

    let second_cell = builder.add_python_cell(
        r#"x1 = X(
    p1="X",
)

x2 = X(
    p1="X",
    p2="X",
    p3="X",
    p4="X",
    p5="X",
    p6="X",
    p7="X",
)
"#,
    );

    builder.open(&mut server);
    server.collect_publish_diagnostic_notifications(2);

    let first_cell_ranges = server.folding_range_request(&first_cell);
    let second_cell_ranges = server.folding_range_request(&second_cell);

    insta::assert_json_snapshot!([first_cell_ranges, second_cell_ranges]);

    Ok(())
}
