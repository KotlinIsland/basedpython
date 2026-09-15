//! `by restage` — the slots of an edit's files in a build tree that already exists.
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
        .args(["build", "--out", "build"])
        .current_dir(dir)
        .status()
        .expect("`by build` should run");
    assert!(status.success(), "`by build` failed");
}

fn restage_all(dir: &Path, files: &[&str]) -> (bool, serde_json::Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["restage", "build"])
        .args(files)
        .current_dir(dir)
        .output()
        .expect("`by restage` should run");
    let json = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "`by restage {files:?}` printed something that is not json: {e}\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    (out.status.success(), json)
}

/// A set of one: the file's own slot, and the set's map beside it.
fn restage(dir: &Path, file: &str) -> (bool, Slot) {
    let (ok, answer) = restage_all(dir, &[file]);
    (ok, Slot(answer))
}

/// One file's answer, read through the set it came back in.
struct Slot(serde_json::Value);

impl Slot {
    /// the file's own field, or the set's `sourcemap`
    fn get(&self, field: &str) -> &serde_json::Value {
        match field {
            "sourcemap" => &self.0["sourcemap"],
            _ => &self.0["files"][0][field],
        }
    }

    /// the first reason the set was refused
    fn refused(&self) -> &str {
        self.0["refusals"][0]["refused"]
            .as_str()
            .unwrap_or_else(|| panic!("not a refusal: {}", self.0))
    }
}

impl std::fmt::Display for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A re-stage is handed no stamps, so it can only reproduce the build's if the
/// build wrote them down.
///
/// This is the reason `build:` stamps arrive as transpile config rather than
/// being discovered inside the pipeline. A re-stage that went looking for the
/// commit itself would answer with whatever `HEAD` is *now* — and drop a module
/// claiming one commit into a tree of modules claiming another, with nothing to
/// say they disagree.
#[test]
fn restaging_reproduces_the_stamps_the_build_settled() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pyproject.toml"),
        "[project]
name = \"p\"
version = \"0.1.0\"
requires-python = \">=3.13\"

# build stamps are experimental, so a project that writes a `build:` block asks
# for them by name
[tool.ty.experimental]
build-stamps = true
",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("main.by"),
        "build:\n    GIT_SHA: str\n\nprint(build.GIT_SHA)\n",
    )
    .unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["build", "--out", "build", "--stamp", "GIT_SHA=abc123"])
        .current_dir(dir.path())
        .status()
        .expect("`by build` should run");
    assert!(status.success(), "`by build` failed");

    let on_disk = std::fs::read_to_string(dir.path().join("build/main.py")).unwrap();
    assert!(
        on_disk.contains(r#"GIT_SHA: str = "abc123""#),
        "the build should have stamped the value:\n{on_disk}"
    );

    let (ok, answer) = restage(dir.path(), "main.by");
    assert!(ok, "an unedited file should re-stage: {answer}");
    assert_eq!(answer.get("content").as_str().unwrap(), on_disk);
    assert_eq!(answer.get("changed").as_bool(), Some(false));
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

    let on_disk = std::fs::read_to_string(dir.path().join("build/main.py")).unwrap();
    let (ok, answer) = restage(dir.path(), "main.by");

    assert!(ok, "an unedited file should re-stage: {answer}");
    assert_eq!(answer.get("content").as_str().unwrap(), on_disk);
    assert_eq!(answer.get("changed").as_bool(), Some(false));
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
    let generated = Path::new(answer.get("generated").as_str().unwrap());

    assert!(
        generated.is_absolute(),
        "{} is not absolute",
        generated.display()
    );
    assert!(
        generated.ends_with("build/main.py"),
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
    assert_eq!(answer.get("changed").as_bool(), Some(true));
    assert!(
        answer
            .get("content")
            .as_str()
            .unwrap()
            .contains("return 42")
    );

    let map = answer
        .get("sourcemap")
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
        let map = answer
            .get("sourcemap")
            .as_str()
            .expect("a transpiled file's re-stage rewrites the map")
            .to_owned();
        let content = answer
            .get("content")
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
        answer.get("content").as_str().unwrap(),
        "def h() -> int:\n    return 3\n"
    );
    assert_eq!(answer.get("changed").as_bool(), Some(true));
    assert!(answer.get("sourcemap").is_null(), "{answer}");
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
    assert!(answer.refused().contains("does not check"), "{answer}");
    let diagnostics = answer.0["refusals"][0]["diagnostics"].as_array().unwrap();
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
    std::fs::remove_file(dir.path().join("build/_by_build.json")).unwrap();

    let (ok, answer) = restage(dir.path(), "main.by");

    assert!(!ok);
    assert!(answer.refused().contains("_by_build.json"), "{answer}");
}

/// The tree records which `by` wrote it because only that `by` can promise the same bytes. A tree
/// from another build is refused rather than re-staged with a transpiler that may lower differently.
#[test]
fn a_tree_built_by_another_by_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());
    build(dir.path());

    let record = dir.path().join("build/_by_build.json");
    let mut parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record).unwrap()).unwrap();
    parsed["byVersion"] = serde_json::Value::String("0.0.0+somethingelse".to_owned());
    std::fs::write(&record, serde_json::to_string_pretty(&parsed).unwrap()).unwrap();

    let (ok, answer) = restage(dir.path(), "main.by");

    assert!(!ok);
    assert!(answer.refused().contains("rebuild"), "{answer}");
}

/// A project with two `.by` modules, so one edit can touch both.
fn write_two_module_project(dir: &Path) {
    write_project(dir);
    std::fs::write(dir.join("other.by"), "def other() -> int:\n    return 5\n").unwrap();
}

/// Put an answer's bytes and its map into the tree, the way a client does.
fn write_answer(dir: &Path, answer: &serde_json::Value) {
    for file in answer["files"].as_array().unwrap() {
        std::fs::write(
            file["generated"].as_str().unwrap(),
            file["content"].as_str().unwrap(),
        )
        .unwrap();
    }
    if let Some(map) = answer["sourcemap"].as_str() {
        std::fs::write(dir.join("build/_by_sourcemap.py"), map).unwrap();
    }
}

/// The keys of both tables of a map, in the order the map holds them.
fn map_keys(map: &str) -> Vec<String> {
    map.lines()
        .filter_map(|line| line.strip_prefix("    \""))
        .map(|line| line.split('"').next().unwrap().to_owned())
        .collect()
}

/// **Why the set is one request.** Two files edited together share one `_by_sourcemap.py`, and
/// the map that comes back has to carry both of their entries moved. Answered one file at a time,
/// each answer's map held the tree's map plus that file alone, so writing them in turn kept only
/// the last file's line table — and every other breakpoint was then re-armed against a table
/// describing code the tree no longer held.
#[test]
fn an_edit_to_two_modules_comes_back_with_one_map_describing_both() {
    let dir = tempfile::tempdir().unwrap();
    write_two_module_project(dir.path());
    build(dir.path());
    let before = std::fs::read_to_string(dir.path().join("build/_by_sourcemap.py")).unwrap();

    std::fs::write(
        dir.path().join("main.by"),
        "def go() -> int:\n    return 42\nprint(go())\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("other.by"),
        "\n\ndef other() -> int:\n    return 6\n",
    )
    .unwrap();
    let (ok, answer) = restage_all(dir.path(), &["main.by", "other.by"]);
    assert!(ok, "two edited files that check should re-stage: {answer}");

    let files = answer["files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "{answer}");
    let map = answer["sourcemap"].as_str().expect("both entries moved");

    for file in files {
        assert_eq!(file["changed"].as_bool(), Some(true), "{file}");
        let py = file["pyDigest"].as_str().unwrap();
        let by = file["byDigest"].as_str().unwrap();
        assert!(
            !before.contains(py) && map.contains(py) && map.contains(by),
            "the one map carries this file's new digests:\n{map}\n{file}"
        );
    }
    // and nothing else about the map moved: both tables hold the same keys, in the same order,
    // as the map the build wrote
    assert_eq!(map_keys(&before), map_keys(map));
}

/// Writing what came back makes the tree what the edit describes: asked again, every file is
/// already there and the map already is the one on disk.
#[test]
fn writing_a_sets_answer_leaves_nothing_left_to_restage() {
    let dir = tempfile::tempdir().unwrap();
    write_two_module_project(dir.path());
    build(dir.path());

    std::fs::write(
        dir.path().join("main.by"),
        "def go() -> int:\n    return 42\nprint(go())\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("other.by"),
        "def other() -> int:\n    return 6\n",
    )
    .unwrap();
    let (ok, answer) = restage_all(dir.path(), &["main.by", "other.by"]);
    assert!(ok, "{answer}");
    write_answer(dir.path(), &answer);

    let (ok, again) = restage_all(dir.path(), &["main.by", "other.by"]);
    assert!(ok, "{again}");
    for file in again["files"].as_array().unwrap() {
        assert_eq!(file["changed"].as_bool(), Some(false), "{file}");
    }
    assert!(again["sourcemap"].is_null(), "{again}");
}

/// One file of the set that does not check refuses the set, and names that file — the other
/// file's bytes are not handed back to be written on their own.
#[test]
fn one_file_that_does_not_check_refuses_the_whole_set_by_name() {
    let dir = tempfile::tempdir().unwrap();
    write_two_module_project(dir.path());
    build(dir.path());

    std::fs::write(
        dir.path().join("main.by"),
        "def go() -> int:\n    return 42\nprint(go())\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("other.by"),
        "def other() -> int:\n    return \"not an int\"\n",
    )
    .unwrap();
    let (ok, answer) = restage_all(dir.path(), &["main.by", "other.by"]);

    assert!(!ok, "{answer}");
    assert!(answer.get("files").is_none(), "{answer}");
    let refusals = answer["refusals"].as_array().unwrap();
    assert_eq!(
        refusals.len(),
        1,
        "only the file that does not check: {answer}"
    );
    assert!(
        refusals[0]["file"].as_str().unwrap().ends_with("other.by"),
        "{answer}"
    );
    assert!(
        !refusals[0]["diagnostics"].as_array().unwrap().is_empty(),
        "{answer}"
    );
}

/// A `.by` and a hand-written `.py` edited together: the copied file comes back as its own bytes,
/// and the map moves only for the file it describes.
#[test]
fn a_set_holding_a_copied_file_moves_the_map_only_for_the_transpiled_one() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());
    build(dir.path());

    std::fs::write(
        dir.path().join("main.by"),
        "def go() -> int:\n    return 42\nprint(go())\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("helper.py"),
        "def h() -> int:\n    return 3\n",
    )
    .unwrap();
    let (ok, answer) = restage_all(dir.path(), &["helper.py", "main.by"]);

    assert!(ok, "{answer}");
    let files = answer["files"].as_array().unwrap();
    assert!(
        files[0]["source"].as_str().unwrap().ends_with("helper.py"),
        "in the order asked"
    );
    assert_eq!(files[0]["content"], "def h() -> int:\n    return 3\n");
    assert!(
        answer["sourcemap"]
            .as_str()
            .unwrap()
            .contains(files[1]["pyDigest"].as_str().unwrap())
    );
}

/// The same file named twice is one slot: two answers for it would be two writes to one file.
#[test]
fn a_file_named_twice_is_answered_once() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());
    build(dir.path());

    let (ok, answer) = restage_all(dir.path(), &["main.by", "./main.by"]);
    assert!(ok, "{answer}");
    assert_eq!(answer["files"].as_array().unwrap().len(), 1, "{answer}");
}
