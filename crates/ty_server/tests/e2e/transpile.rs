//! The `by/transpile` and `by/transpileForBuild` requests, exercised through the
//! server rather than through the library they call.
//!
//! That distinction is the whole reason these exist. The transpiler builds a
//! database of its own whenever it lowers rewritten source, and a request handler
//! runs with the project database already attached — so the nested one is a second
//! attachment on the same thread, which salsa refuses with "Cannot change database
//! mid-query". Nothing about it reproduces from the command line, where nothing is
//! attached, and the `crates/ty` tests that cover the same operations therefore
//! passed throughout. Only a request does it.

use anyhow::Result;
use by_stage::record::BuildRecord;
use by_transforms::config::Config;
use lsp_types::{LspRequestMethod, MessageDirection, Request, TextDocumentIdentifier, Uri};
use ruff_db::system::{SystemPath, SystemPathBuf};

use crate::{TestServer, TestServerBuilder};

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TranspileParams {
    text_document: TextDocumentIdentifier,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    reverse: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TranspileResponse {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

enum Transpile {}

impl Request for Transpile {
    type Params = TranspileParams;
    type Result = Option<TranspileResponse>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/transpile");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

fn params(uri: &Uri, source: Option<&str>) -> TranspileParams {
    TranspileParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        reverse: false,
        source: source.map(str::to_owned),
    }
}

/// The fragment path: `source` is supplied, so the whole transpile runs over text
/// rather than the project, and it builds an in-memory database to check that text
/// against. That database is queried while the project's is attached.
#[test]
fn transpiling_a_fragment_answers_rather_than_panicking() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let main = SystemPath::new("src/main.by");
    let content = "def f(a: int?) -> int:\n    return a ?? 0\n";

    let mut server = TestServerBuilder::new()?
        .with_workspace(workspace_root, None)?
        .with_file(main, content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(main, content, 1);
    let uri = server.file_uri(main);

    let response = server
        .send_request_await::<Transpile>(params(&uri, Some(content)))
        .expect("the server answered nothing at all");

    assert!(
        response.error.is_none(),
        "transpile failed: {:?}",
        response.error
    );
    let generated = response.source.expect("no generated source");
    assert!(
        generated.contains("def f("),
        "unexpected output:\n{generated}"
    );
    Ok(())
}

/// The whole-document path, which goes through the project database — and then
/// still builds one of its own, because a pre-pass rewrote the source it lowers.
/// `enum class` is such a pre-pass.
#[test]
fn transpiling_a_document_answers_rather_than_panicking() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let main = SystemPath::new("src/main.by");
    let content = "enum class Colour:\n    case Red\n    case Green\n\nlet chosen: Colour = Red\n";

    let mut server = TestServerBuilder::new()?
        .with_workspace(workspace_root, None)?
        .with_file(main, content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(main, content, 1);
    let uri = server.file_uri(main);

    let response = server
        .send_request_await::<Transpile>(params(&uri, None))
        .expect("the server answered nothing at all");

    assert!(
        response.error.is_none(),
        "transpile failed: {:?}",
        response.error
    );
    let generated = response.source.expect("no generated source");
    assert!(
        generated.contains("Colour"),
        "unexpected output:\n{generated}"
    );
    Ok(())
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TranspileForBuildParams {
    text_documents: Vec<TextDocumentIdentifier>,
    build_directory: std::path::PathBuf,
}

enum TranspileForBuild {}

impl Request for TranspileForBuild {
    type Params = TranspileForBuildParams;
    // read as json rather than as the server's own type, so the shape a client depends on is what
    // is asserted
    type Result = serde_json::Value;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/transpileForBuild");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// A tree shaped the way a build leaves one for `project/main.by` and `project/other.by`: the
/// record naming this `by`, and a map holding an entry for each module — with digests that
/// describe nothing, so every file the server answers for comes back changed and moves its entry.
///
/// Laid out by hand because the build lives in the `by` binary, which this crate's tests cannot
/// run; everything the server reads out of the tree is here.
fn stale_build(server: &TestServer, build: &SystemPath) -> Result<SystemPathBuf> {
    let project = server.file_path("project");
    let build = server.file_path(build);
    std::fs::create_dir_all(build.as_std_path())?;
    let record = BuildRecord::new(
        project.as_std_path(),
        &[project.as_std_path().to_path_buf()],
        None,
        false,
        &Config::default(),
    );
    std::fs::write(
        build.join("_by_build.json").as_std_path(),
        serde_json::to_string_pretty(&record)?,
    )?;
    // the keys are paths inside python string literals, written the way `by build` writes
    // them: joined with the separator this system uses, and with a backslash escaped rather
    // than left to start an escape of its own. an unescaped windows path is a bad `\U`
    // escape, and a map that does not parse names no file, so every file is refused as one
    // the build was not made of
    let literal = |path: &SystemPath| path.as_str().replace('\\', "\\\\");
    let entry = |module: &str| {
        let generated = literal(&build.join(format!("{module}.py")));
        let source = literal(&project.join(format!("{module}.by")));
        (
            format!("    \"{generated}\": (\"{source}\", [None]),\n"),
            format!("    \"{generated}\": {{\"by\": \"sha256:00\", \"py\": \"sha256:00\"}},\n"),
        )
    };
    let (main_map, main_digests) = entry("main");
    let (other_map, other_digests) = entry("other");
    std::fs::write(
        build.join("_by_sourcemap.py").as_std_path(),
        format!(
            "SOURCEMAP = {{\n{main_map}{other_map}}}\n\nDIGESTS = {{\n{main_digests}{other_digests}}}\n"
        ),
    )?;
    Ok(build)
}

fn restage(server: &mut TestServer, build: &SystemPath, modules: &[&str]) -> serde_json::Value {
    let params = TranspileForBuildParams {
        text_documents: modules
            .iter()
            .map(|module| TextDocumentIdentifier {
                uri: server.file_uri(format!("project/{module}.by")),
            })
            .collect(),
        build_directory: build.as_std_path().to_path_buf(),
    };
    server.send_request_await::<TranspileForBuild>(params)
}

/// **One edit, one map.** Every file of the edit is named in one request, and the answer carries
/// one `_by_sourcemap.py` with each file's entry moved in it. A request per file could only answer
/// with the tree's map plus that one file, so a client writing each answer in turn kept the last
/// file's line table and lost the others'.
///
/// And the answer is the tree the edit describes: once it is written, asking again finds every
/// file already in place and the map already the one on disk.
#[test]
fn an_edit_to_two_files_is_answered_with_one_map_describing_both() -> Result<()> {
    let main = "def go() -> int:\n    return 42\n";
    let other = "def other() -> str:\n    return \"six\"\n";
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file("project/main.by", main)?
        .with_file("project/other.by", other)?
        .build()
        .wait_until_workspaces_are_initialized();
    let build = stale_build(&server, SystemPath::new("build"))?;

    let answer = restage(&mut server, &build, &["main", "other"]);

    let files = answer["files"]
        .as_array()
        .unwrap_or_else(|| panic!("not a re-stage of the set: {answer}"));
    assert_eq!(files.len(), 2, "{answer}");
    let map = answer["sourcemap"]
        .as_str()
        .unwrap_or_else(|| panic!("no map: {answer}"));
    for (file, module) in files.iter().zip(["main", "other"]) {
        assert!(
            file["generated"].as_str().unwrap().ends_with(
                SystemPath::new("build")
                    .join(format!("{module}.py"))
                    .as_str()
            ),
            "{file}"
        );
        assert_eq!(file["changed"].as_bool(), Some(true), "{file}");
        assert!(
            map.contains(file["pyDigest"].as_str().unwrap())
                && map.contains(file["byDigest"].as_str().unwrap()),
            "the one map carries {module}'s entry:\n{map}"
        );
    }
    assert!(!map.contains("sha256:00"), "no entry is left stale:\n{map}");

    for file in files {
        std::fs::write(
            file["generated"].as_str().unwrap(),
            file["content"].as_str().unwrap(),
        )?;
    }
    std::fs::write(build.join("_by_sourcemap.py").as_std_path(), map)?;

    let again = restage(&mut server, &build, &["main", "other"]);
    for file in again["files"].as_array().unwrap() {
        assert_eq!(file["changed"].as_bool(), Some(false), "{again}");
    }
    assert!(again["sourcemap"].is_null(), "{again}");
    Ok(())
}

/// One file that does not check refuses the set by name, and hands back no bytes for the other: a
/// tree holding half of an edit describes a program that never existed.
#[test]
fn one_file_of_an_edit_that_does_not_check_refuses_the_set() -> Result<()> {
    let main = "def go() -> int:\n    return 42\n";
    let other = "def other() -> int:\n    return \"not an int\"\n";
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file("project/main.by", main)?
        .with_file("project/other.by", other)?
        .build()
        .wait_until_workspaces_are_initialized();
    let build = stale_build(&server, SystemPath::new("build"))?;
    // open, as the editor that edited it holds it. a closed one refuses the same way — see
    // `a_file_edited_then_closed_that_does_not_check_refuses_the_set`
    server.open_text_document(SystemPath::new("project/other.by"), other, 1);

    let answer = restage(&mut server, &build, &["main", "other"]);

    assert_other_refused_for_not_checking(&answer);
    Ok(())
}

/// The refusal a file that does not check gets, asserted for `other.by`.
fn assert_other_refused_for_not_checking(answer: &serde_json::Value) {
    assert!(answer.get("files").is_none(), "{answer}");
    let refusals = answer["refusals"]
        .as_array()
        .unwrap_or_else(|| panic!("not a refusal: {answer}"));
    assert_eq!(refusals.len(), 1, "{answer}");
    assert!(
        refusals[0]["file"].as_str().unwrap().ends_with("other.by"),
        "{answer}"
    );
    assert!(
        refusals[0]["refused"]
            .as_str()
            .unwrap()
            .contains("does not check"),
        "{answer}"
    );
    assert!(
        refusals[0]["diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| diagnostics.iter().any(|d| d
                .as_str()
                .is_some_and(|d| d.contains("invalid-return-type")))),
        "{answer}"
    );
}

/// **A file the editor has closed is checked all the same.** The edit is saved and its tab closed
/// before the reload is pressed, and this server's default mode reports diagnostics only for open
/// files — but the gate is whether the file checks, not whether an editor is holding it, so the
/// type error must refuse exactly as it does while the file is open.
#[test]
fn a_file_edited_then_closed_that_does_not_check_refuses_the_set() -> Result<()> {
    use lsp_types::{TextDocumentContentChangeEvent, TextDocumentContentChangeWholeDocument};

    let main = "def go() -> int:\n    return 42\n";
    let other = "def other() -> int:\n    return 6\n";
    let broken = "def other() -> int:\n    return \"not an int\"\n";
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file("project/main.by", main)?
        .with_file("project/other.by", other)?
        .build()
        .wait_until_workspaces_are_initialized();
    let build = stale_build(&server, SystemPath::new("build"))?;

    server.open_text_document(SystemPath::new("project/other.by"), other, 1);
    server.change_text_document(
        SystemPath::new("project/other.by"),
        vec![
            TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                TextDocumentContentChangeWholeDocument {
                    text: broken.to_string(),
                },
            ),
        ],
        2,
    );
    server.write_file("project/other.by", broken)?;
    server.close_text_document(SystemPath::new("project/other.by"));

    let answer = restage(&mut server, &build, &["main", "other"]);

    assert_other_refused_for_not_checking(&answer);
    Ok(())
}

/// And a file no editor ever opened is checked too: the request names the files it re-stages, and
/// those are the files it checks.
#[test]
fn a_file_never_opened_that_does_not_check_refuses_the_set() -> Result<()> {
    let main = "def go() -> int:\n    return 42\n";
    let other = "def other() -> int:\n    return \"not an int\"\n";
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file("project/main.by", main)?
        .with_file("project/other.by", other)?
        .build()
        .wait_until_workspaces_are_initialized();
    let build = stale_build(&server, SystemPath::new("build"))?;

    let answer = restage(&mut server, &build, &["main", "other"]);

    assert_other_refused_for_not_checking(&answer);
    Ok(())
}

/// **An edit landing while a reload is answered is not a panic.** Typing and then pressing Reload
/// sends the request first and the keystrokes' `didChange` after it, and the change arrives while
/// the transpile is still reading the database it writes to. Salsa cancels the reader by unwinding
/// it; the request is asked again against the edit, and answered about the text the editor now
/// holds rather than refused as a transpiler panic.
///
/// The race is made certain rather than hoped for: the tree's `_by_sourcemap.py` is a pipe, so the
/// server — which reads it after taking its snapshot and before transpiling anything — waits on it
/// until the test has sent the edit.
#[cfg(unix)]
#[test]
fn an_edit_arriving_while_a_reload_is_answered_is_answered_against_the_edit() -> Result<()> {
    use std::io::Write as _;
    use std::time::Duration;

    use lsp_types::{TextDocumentContentChangeEvent, TextDocumentContentChangeWholeDocument};

    let before = "def go() -> int:\n    return 42\n";
    let after = "def go() -> int:\n    return 43\n";
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file("project/main.by", before)?
        .with_file("project/other.by", "def other() -> int:\n    return 6\n")?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("project/main.by"), before, 1);
    let build = stale_build(&server, SystemPath::new("build"))?;

    let pipe = build.join("_by_sourcemap.py").into_std_path_buf();
    let map = std::fs::read_to_string(&pipe)?;
    std::fs::remove_file(&pipe)?;
    let made = std::process::Command::new("mkfifo").arg(&pipe).status()?;
    assert!(made.success(), "mkfifo failed: {made}");

    let id = server.send_request::<TranspileForBuild>(TranspileForBuildParams {
        text_documents: vec![TextDocumentIdentifier {
            uri: server.file_uri("project/main.by"),
        }],
        build_directory: build.as_std_path().to_path_buf(),
    });
    server.change_text_document(
        SystemPath::new("project/main.by"),
        vec![
            TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                TextDocumentContentChangeWholeDocument {
                    text: after.to_string(),
                },
            ),
        ],
        2,
    );
    // Long enough for the server to take the change in and begin writing it, which it cannot
    // finish while the request holds the revision it replaces. The server has nothing to show that
    // it has begun, so this is a wait; if it is ever too short, the answer is about `before` and
    // the assertion below says so rather than passing.
    std::thread::sleep(Duration::from_millis(500));

    // Opening a pipe to write waits for its reader, so once this returns the server is holding the
    // pipe open. The path then becomes an ordinary file holding the same map, before the pipe is
    // written — so the server's read is fed exactly once, and whatever reads the map after it,
    // which is only ever an attempt made after the edit, reads the file.
    let fed = {
        let (pipe, staged) = (
            pipe.clone(),
            build.join("_by_sourcemap.py.next").into_std_path_buf(),
        );
        std::fs::write(&staged, &map)?;
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let fed = std::fs::OpenOptions::new()
                .write(true)
                .open(&pipe)
                .and_then(|mut writer| {
                    std::fs::rename(&staged, &pipe)?;
                    writer.write_all(map.as_bytes())
                });
            let _ = sender.send(fed);
        });
        receiver.recv_timeout(Duration::from_secs(60))
    };
    let Ok(fed) = fed else {
        // let the feeder go, which is waiting for a reader that never came
        let _ = std::fs::read(&pipe);
        panic!("the server never read the tree's map");
    };
    fed?;

    let answer = server
        .try_await_response::<TranspileForBuild>(&id, Some(Duration::from_secs(60)))
        .unwrap_or_else(|error| panic!("the reload was not answered: {error}"));
    let files = answer["files"]
        .as_array()
        .unwrap_or_else(|| panic!("the reload was refused: {answer}"));
    let content = files[0]["content"].as_str().unwrap();
    // The request's first attempt holds the revision from before the edit, and nothing can show it
    // the edit — the change's write waits for that attempt to let go. So an answer about the edit is
    // an answer from a second attempt, made against the new revision.
    assert!(
        content.contains("return 43"),
        "the answer is about the text before the edit, so the edit never reached the \
         request while it was running:\n{content}"
    );
    Ok(())
}

/// A tree nothing says is a build is refused for the whole set, not file by file.
#[test]
fn a_directory_that_is_not_a_build_refuses_the_set() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file("project/main.by", "x: int = 1\n")?
        .build()
        .wait_until_workspaces_are_initialized();
    let nothing = server.file_path("nothing");
    std::fs::create_dir_all(nothing.as_std_path())?;

    let answer = restage(&mut server, &nothing, &["main"]);

    let refusals = answer["refusals"].as_array().unwrap();
    assert_eq!(refusals.len(), 1, "{answer}");
    assert!(refusals[0]["file"].is_null(), "{answer}");
    assert!(
        refusals[0]["refused"]
            .as_str()
            .unwrap()
            .contains("_by_build.json"),
        "{answer}"
    );
    Ok(())
}
