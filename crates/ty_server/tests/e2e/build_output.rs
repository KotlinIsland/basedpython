//! `by/buildOutput` over the wire
//!
//! the mapping itself is `by_stage::layout`'s; what these cover is that a client, sending a uri,
//! gets back the same layout `by build` writes — the directory, and the module-tree path inside it
//! rather than the directory-tree one — and gets it for paths that are not open documents

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request, Uri};
use ruff_db::system::{SystemPath, SystemPathBuf};

use crate::TestServerBuilder;

/// the request as a client sends it, in json throughout
enum BuildOutput {}

impl Request for BuildOutput {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/buildOutput");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

fn ask(server: &mut crate::TestServer, uri: &Uri) -> serde_json::Value {
    server
        .send_request_await::<BuildOutput>(serde_json::json!({ "uri": uri }))
        .expect("a path inside the project has a place in its build")
}

/// compared as a path, so that `/` and `\` are the same separator on windows
fn path_of(answer: &serde_json::Value, key: &str) -> Option<SystemPathBuf> {
    answer[key].as_str().map(SystemPathBuf::from)
}

#[test]
fn a_source_in_a_src_layout_is_built_into_its_module_path() -> Result<()> {
    let pyproject = SystemPath::new("pyproject.toml");
    let main = SystemPath::new("src/pkg/main.by");

    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new(""), None)?
        .with_file(
            pyproject,
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )?
        .with_file(SystemPath::new("src/pkg/__init__.py"), "")?
        .with_file(main, "def f() -> int:\n    return 1\n")?
        .build()
        .wait_until_workspaces_are_initialized();

    let root = server.file_path(SystemPath::new(""));
    let build = root.join("build");
    let generated = build.join("pkg").join("main.py");

    let uri = server.file_uri(main);
    let answer = ask(&mut server, &uri);
    assert_eq!(
        path_of(&answer, "projectRoot").as_deref(),
        Some(root.as_path())
    );
    assert_eq!(
        path_of(&answer, "buildDirectory").as_deref(),
        Some(build.as_path())
    );
    assert_eq!(
        path_of(&answer, "generated").as_deref(),
        Some(generated.as_path())
    );
    assert_eq!(path_of(&answer, "source"), None);

    // and back again, for a generated file that has never been written
    let uri = server.file_uri(SystemPath::new("build/pkg/main.py"));
    let back = ask(&mut server, &uri);
    assert_eq!(
        path_of(&back, "source").as_deref(),
        Some(server.file_path(main).as_path())
    );
    assert_eq!(path_of(&back, "generated"), None);
    Ok(())
}

#[test]
fn a_directory_is_placed_without_being_a_source_or_an_output() -> Result<()> {
    let main = SystemPath::new("main.by");

    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new(""), None)?
        .with_file(main, "x = 1\n")?
        .build()
        .wait_until_workspaces_are_initialized();

    let uri = server.file_uri(SystemPath::new(""));
    let answer = ask(&mut server, &uri);
    assert_eq!(
        path_of(&answer, "buildDirectory").as_deref(),
        Some(server.file_path(SystemPath::new("build")).as_path()),
    );
    assert_eq!(path_of(&answer, "generated"), None);
    assert_eq!(path_of(&answer, "source"), None);

    let uri = server.file_uri(main);
    let flat = ask(&mut server, &uri);
    assert_eq!(
        path_of(&flat, "generated").as_deref(),
        Some(server.file_path(SystemPath::new("build/main.py")).as_path()),
    );
    Ok(())
}
