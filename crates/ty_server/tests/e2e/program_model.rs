//! `by/entryPoint`, `by/testItems` and `by/runModules`: what a module runs as, which tests a file
//! holds, and which file a module name runs — the model an editor's run configurations are built
//! from, answered by the server so the editor never re-derives it from text.

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request, Uri};
use ruff_db::system::SystemPath;
use serde_json::{Value, json};

use crate::TestServerBuilder;

enum EntryPoint {}

impl Request for EntryPoint {
    type Params = Value;
    type Result = Option<Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/entryPoint");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

enum TestItems {}

impl Request for TestItems {
    type Params = Value;
    type Result = Option<Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/testItems");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

enum RunModules {}

impl Request for RunModules {
    type Params = Value;
    type Result = Option<Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/runModules");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// A generic `main` is `main`, a `main(` inside a docstring is not a call, and every parameter
/// comes back as the generated parser registers it — for a file the editor never opened.
#[test]
fn entry_point_is_the_transpilers_reading_of_main() -> Result<()> {
    let root = SystemPath::new("project");
    let main = SystemPath::new("project/main.by");
    let content = r#""""
main()
"""

def main[T](out_dir: Path, /, count: int = 1, *, mode: "fast" | "slow" = "fast", verbose: bool = False):
    """Does the thing."""
"#;

    let mut server = TestServerBuilder::new()?
        .with_workspace(root, None)?
        .with_file(main, content)?
        .build()
        .wait_until_workspaces_are_initialized();

    let uri = server.file_uri(main);
    let response = server
        .send_request_await::<EntryPoint>(json!({ "uri": uri }))
        .expect("the server answered nothing at all");

    assert_eq!(response["guards"], json!([]));
    let main = &response["main"];
    assert_eq!(main["entryPoint"], true, "{response:#}");
    assert_eq!(main["moduleInvokesMain"], false);
    assert_eq!(main["blockedBy"], Value::Null);
    assert_eq!(main["docstring"], "Does the thing.");
    assert_eq!(
        main["nameRange"]["start"],
        json!({ "line": 4, "character": 4 })
    );
    assert_eq!(
        main["parameters"],
        json!([
            {
                "name": "out_dir", "kind": "positional", "required": true,
                "annotation": "Path", "default": null,
                "cli": {
                    "converter": "Path", "choices": null,
                    "flags": ["--out-dir", "--out_dir"], "negativeFlags": [],
                },
            },
            {
                "name": "count", "kind": "any", "required": false,
                "annotation": "int", "default": "1",
                "cli": { "converter": "int", "choices": null, "flags": ["--count"], "negativeFlags": [] },
            },
            {
                "name": "mode", "kind": "keyword", "required": false,
                "annotation": "\"fast\" | \"slow\"", "default": "\"fast\"",
                "cli": {
                    "converter": "str", "choices": ["fast", "slow"],
                    "flags": ["--mode"], "negativeFlags": [],
                },
            },
            {
                "name": "verbose", "kind": "keyword", "required": false,
                "annotation": "bool", "default": "False",
                "cli": {
                    "converter": null, "choices": null,
                    "flags": ["--verbose"], "negativeFlags": ["--no-verbose"],
                },
            },
        ])
    );
    Ok(())
}

/// A hand-written guard is reported as an entry point of its own, and it is what stops `main`
/// being one.
#[test]
fn a_hand_written_guard_keeps_its_own_entry_point() -> Result<()> {
    let root = SystemPath::new("project");
    let main = SystemPath::new("project/main.by");
    let content =
        "def main(name: str):\n    pass\n\nif __name__ == \"__main__\":\n    main(\"x\")\n";

    let mut server = TestServerBuilder::new()?
        .with_workspace(root, None)?
        .with_file(main, content)?
        .build()
        .wait_until_workspaces_are_initialized();

    let uri = server.file_uri(main);
    let response = server
        .send_request_await::<EntryPoint>(json!({ "uri": uri }))
        .expect("the server answered nothing at all");

    assert_eq!(
        response["guards"],
        json!([{ "start": { "line": 3, "character": 0 }, "end": { "line": 3, "character": 25 } }])
    );
    assert_eq!(response["main"]["entryPoint"], false);
    assert_eq!(response["main"]["moduleInvokesMain"], true);
    Ok(())
}

/// A test nested in a function is not one, however its enclosing class came before it, and the
/// answer follows the buffer rather than the disk.
#[test]
fn test_items_are_what_pytest_would_collect() -> Result<()> {
    let root = SystemPath::new("project");
    let tests = SystemPath::new("project/tests/test_math.by");
    let helper = SystemPath::new("project/main.by");
    let on_disk = "def test_old():\n    pass\n";
    let buffer = r#"class TestA:
    def test_one(self):
        pass

    class TestNested:
        def test_deep(self):
            pass

def outer():
    def test_inner():
        pass

def test_top():
    pass
"#;

    let mut server = TestServerBuilder::new()?
        .with_workspace(root, None)?
        .with_file(tests, on_disk)?
        .with_file(helper, "def test_not_collected():\n    pass\n")?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(tests, buffer, 1);

    let uri = server.file_uri(tests);
    let one = server
        .send_request_await::<TestItems>(json!({ "uri": uri }))
        .expect("the server answered nothing at all");
    let ids = |items: &Value| -> Vec<String> {
        fn walk(items: &Value, out: &mut Vec<String>) {
            for item in items.as_array().into_iter().flatten() {
                out.push(item["id"].as_str().unwrap_or_default().to_owned());
                walk(&item["children"], out);
            }
        }
        let mut out = Vec::new();
        walk(items, &mut out);
        out
    };
    assert_eq!(
        ids(&one["files"][0]["tests"]),
        [
            "TestA",
            "TestA::test_one",
            "TestA::TestNested",
            "TestA::TestNested::test_deep",
            "test_top"
        ]
    );
    assert_eq!(
        one["files"][0]["tests"][1]["selectionRange"]["start"],
        json!({ "line": 12, "character": 4 })
    );

    let all = server
        .send_request_await::<TestItems>(json!({}))
        .expect("the server answered nothing at all");
    let files = all["files"].as_array().expect("files");
    assert_eq!(files.len(), 1, "only the test module holds tests: {all:#}");
    assert_eq!(files[0]["uri"], json!(uri));
    Ok(())
}

/// A name resolves the way the project resolves it: the configured `run.main` when none is given, a
/// package as its `__main__`, and a file another holds the name of runs under no name at all.
#[test]
fn run_modules_resolve_as_by_run_does() -> Result<()> {
    let root = SystemPath::new("project");
    let shadowed = SystemPath::new("project/main.by");
    let src_main = SystemPath::new("project/src/main.by");
    let app = SystemPath::new("project/src/app/__main__.by");

    let mut server = TestServerBuilder::new()?
        .with_workspace(root, None)?
        .with_file(
            SystemPath::new("project/ty.toml"),
            "[run]\nmain = \"app\"\n",
        )?
        .with_file(shadowed, "def main():\n    pass\n")?
        .with_file(src_main, "def main():\n    pass\n")?
        .with_file(SystemPath::new("project/src/app/__init__.by"), "")?
        .with_file(app, "def main():\n    pass\n")?
        .with_file(
            SystemPath::new("project/src/twin.by"),
            "def main():\n    pass\n",
        )?
        .with_file(SystemPath::new("project/src/twin.py"), "print(1)\n")?
        .build()
        .wait_until_workspaces_are_initialized();

    let response = server
        .send_request_await::<RunModules>(json!({ "module": "main" }))
        .expect("the server answered nothing at all");
    let project = &response["projects"][0];

    assert_eq!(
        project["main"],
        json!({ "module": "app", "uri": server.file_uri(app) })
    );
    let requested: Uri = serde_json::from_value(project["requested"]["uri"].clone())?;
    let modules = project["modules"].as_array().expect("modules");
    let named_main: Vec<&Value> = modules.iter().filter(|m| m["module"] == "main").collect();
    assert_eq!(named_main.len(), 1, "one file runs as `main`: {project:#}");
    assert_eq!(named_main[0]["uri"], json!(requested));
    // the resolver gives the name to the file in the root it searches first, and the other one runs
    // under none. (`by run` itself refuses to stage two files that build to one module, so neither
    // pair runs until one is renamed; what is pinned here is that one name never means two files.)
    assert_eq!(requested, server.file_uri(src_main));
    let twins: Vec<&Value> = modules.iter().filter(|m| m["module"] == "twin").collect();
    assert_eq!(
        twins,
        [&json!({ "module": "twin", "uri": server.file_uri("project/src/twin.py") })],
        "one of `twin.by` and `twin.py` holds the name: {project:#}"
    );
    assert!(
        modules.iter().any(|m| m["module"] == "app.__main__"),
        "{project:#}"
    );
    assert!(
        !modules.iter().any(|m| m["module"] == "app"),
        "a package's `__init__` is not what `by run app` runs: {project:#}"
    );
    Ok(())
}
