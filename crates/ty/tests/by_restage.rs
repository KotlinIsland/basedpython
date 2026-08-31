//! `by restage` — one file's slot in a build tree that already exists.
//!
//! Driven through the real binary rather than the library, because the property that matters is
//! about two commands agreeing: what `by build` wrote into the tree, and what `by restage` says
//! should be there now. A test that called one function twice could not tell them apart.

use std::path::Path;
use std::process::Command;

/// A project with one `.by`, one hand-written `.py` beside it, and nothing else.
fn write_project(dir: &Path) {
    std::fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname = \"p\"\nversion = \"0.1.0\"\nrequires-python = \">=3.13\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("main.by"),
        "def go() -> int:\n    return 1\nprint(go())\n",
    )
    .unwrap();
    std::fs::write(dir.join("helper.py"), "def h() -> int:\n    return 2\n").unwrap();
}

fn build(dir: &Path) {
    let status = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["build", "--out", "out"])
        .current_dir(dir)
        .status()
        .expect("`by build` should run");
    assert!(status.success(), "`by build` failed");
}

fn restage(dir: &Path, file: &str) -> (bool, serde_json::Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["restage", "out", file])
        .current_dir(dir)
        .output()
        .expect("`by restage` should run");
    let json = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "`by restage {file}` printed something that is not json: {e}\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    (out.status.success(), json)
}

/// **The property the whole design rests on.**
///
/// A re-stage has to produce the bytes the build itself would have written, or the debugger is
/// handed a module body whose line table describes a different file — and it would refuse it as a
/// changed module body, which is the loud failure. The quiet one is worse: a map beside the
/// generated file describing the file it used to be.
///
/// Byte-for-byte against what is on disk, and `changed: false` says so in the answer, which is what
/// lets a caller skip a file rather than replace it with what it already is.
#[test]
fn restaging_a_file_nobody_edited_reproduces_the_build_exactly() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());
    build(dir.path());

    let on_disk = std::fs::read_to_string(dir.path().join("out/main.py")).unwrap();
    let (ok, answer) = restage(dir.path(), "main.by");

    assert!(ok, "an unedited file should re-stage: {answer}");
    assert_eq!(answer["content"].as_str().unwrap(), on_disk);
    assert_eq!(answer["changed"].as_bool(), Some(false));
}

/// The path a caller writes to, and the key `_by_sourcemap.py` uses, have to be the same path — a
/// caller resolving a relative one against its own working directory would write the file somewhere
/// the map says nothing about.
#[test]
fn the_generated_path_is_absolute_even_for_a_relative_build_directory() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());
    build(dir.path());

    let (_, answer) = restage(dir.path(), "main.by");
    let generated = Path::new(answer["generated"].as_str().unwrap());

    assert!(
        generated.is_absolute(),
        "{} is not absolute",
        generated.display()
    );
    assert!(
        generated.ends_with("out/main.py"),
        "{}",
        generated.display()
    );
}

/// An edit produces new bytes **and** a rewritten map: the digests beside that entry are over the
/// bytes that just changed, and a map left describing the file it used to be is the one outcome the
/// digests exist to prevent.
#[test]
fn an_edited_source_comes_back_changed_with_a_rewritten_map() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());
    build(dir.path());

    std::fs::write(
        dir.path().join("main.by"),
        "def go() -> int:\n    return 42\nprint(go())\n",
    )
    .unwrap();
    let (ok, answer) = restage(dir.path(), "main.by");

    assert!(ok, "an edited file that checks should re-stage: {answer}");
    assert_eq!(answer["changed"].as_bool(), Some(true));
    assert!(answer["content"].as_str().unwrap().contains("return 42"));

    let map = answer["sourcemap"]
        .as_str()
        .expect("a transpiled file's re-stage rewrites the map");
    assert!(map.contains("SOURCEMAP"), "{map}");
    assert!(map.contains("DIGESTS"), "{map}");
}

/// An editor saves what the user typed, and what the user typed may not end in a newline — which is
/// exactly the file a re-stage exists to serve, since the debugger is about to be handed this map
/// and asked which generated line the caret's `.by` line became.
///
/// The line table used to be built one entry per `\n`, so the last line of such a file had no entry
/// and the lookup answered with its neighbour's, or with nothing. Here the two spellings of one
/// program must produce the same table, differing only in the bytes of the source.
#[test]
fn a_source_saved_without_a_trailing_newline_re_stages_a_whole_line_table() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());
    build(dir.path());

    let body = "def go() -> int:\n    return 42\nprint(go())";
    let table_of = |source: &str| {
        std::fs::write(dir.path().join("main.by"), source).unwrap();
        let (ok, answer) = restage(dir.path(), "main.by");
        assert!(ok, "an edited file that checks should re-stage: {answer}");
        let map = answer["sourcemap"]
            .as_str()
            .expect("a transpiled file's re-stage rewrites the map")
            .to_owned();
        let content = answer["content"]
            .as_str()
            .expect("bytes to write")
            .to_owned();
        let (_, rest) = map
            .split_once('[')
            .unwrap_or_else(|| panic!("no line table:\n{map}"));
        let (list, _) = rest
            .split_once(']')
            .unwrap_or_else(|| panic!("no line table:\n{map}"));
        (list.to_owned(), content)
    };

    let (without, content) = table_of(body);
    let (with_newline, _) = table_of(&format!("{body}\n"));

    assert_eq!(
        without, with_newline,
        "the terminator closes the last line, it does not add one"
    );
    assert_eq!(
        without.split(", ").count(),
        content.lines().count(),
        "one entry per generated line:\n[{without}]\n{content}"
    );
}

/// A hand-written `.py` is in the tree because it was **copied**, not transpiled — so its slot is
/// its own bytes and the map says nothing about it. Answering with a map entry for one would be
/// inventing a file the transpiler never produced.
#[test]
fn a_hand_written_python_is_its_own_bytes_and_has_no_map_entry() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());
    build(dir.path());

    std::fs::write(
        dir.path().join("helper.py"),
        "def h() -> int:\n    return 3\n",
    )
    .unwrap();
    let (ok, answer) = restage(dir.path(), "helper.py");

    assert!(ok, "a copied python file should re-stage: {answer}");
    assert_eq!(
        answer["content"].as_str().unwrap(),
        "def h() -> int:\n    return 3\n"
    );
    assert_eq!(answer["changed"].as_bool(), Some(true));
    assert!(answer["sourcemap"].is_null(), "{answer}");
}

/// A file that does not check must not reach a running program: the transpiler would emit for a
/// source the checker rejected, and a refusal costs a restart while a wrong answer costs a session.
/// The diagnostics come with it, because "it does not check" alone is not something a user can act
/// on.
#[test]
fn a_source_that_does_not_check_is_refused_with_the_reasons() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());
    build(dir.path());

    std::fs::write(
        dir.path().join("main.by"),
        "def go() -> int:\n    return \"not an int\"\nprint(go())\n",
    )
    .unwrap();
    let (ok, answer) = restage(dir.path(), "main.by");

    assert!(!ok, "a refusal exits non-zero so a script can read it");
    assert!(
        answer["refused"]
            .as_str()
            .unwrap()
            .contains("does not check"),
        "{answer}"
    );
    let diagnostics = answer["diagnostics"].as_array().unwrap();
    assert!(
        !diagnostics.is_empty(),
        "a refusal for the checker carries what it said"
    );
}

/// bpd finds a build by the map in it, and `by` finds what wrote a build by the record in it. A
/// directory with neither is not a build, and guessing a configuration for it would emit bytes no
/// build would have written.
#[test]
fn a_directory_that_is_not_a_build_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());
    build(dir.path());
    std::fs::remove_file(dir.path().join("out/_by_build.json")).unwrap();

    let (ok, answer) = restage(dir.path(), "main.by");

    assert!(!ok);
    assert!(
        answer["refused"]
            .as_str()
            .unwrap()
            .contains("_by_build.json"),
        "{answer}"
    );
}

/// The tree records which `by` wrote it because only that `by` can promise the same bytes. A tree
/// from another build is refused rather than re-staged with a transpiler that may lower differently.
#[test]
fn a_tree_built_by_another_by_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());
    build(dir.path());

    let record = dir.path().join("out/_by_build.json");
    let mut parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record).unwrap()).unwrap();
    parsed["byVersion"] = serde_json::Value::String("0.0.0+somethingelse".to_owned());
    std::fs::write(&record, serde_json::to_string_pretty(&parsed).unwrap()).unwrap();

    let (ok, answer) = restage(dir.path(), "main.by");

    assert!(!ok);
    assert!(
        answer["refused"].as_str().unwrap().contains("rebuild"),
        "{answer}"
    );
}
