//! checker/runtime divergence harness.
//!
//! every `.by` code block in the mdtests that the checker accepts
//! (no `# error:` or `# snapshot` assertions) must also transpile and *execute*
//! cleanly: the mdtest framework verifies ty's diagnostics, this test verifies
//! the runtime half of the contract. divergences of the form "checks clean but crashes at
//! runtime" (enum constants becoming members, transform composition leaks,
//! unsound lowerings) are exactly the bug class this catches.
//!
//! blocks carrying expected diagnostics are skipped — their runtime behaviour
//! is intentionally unspecified.
//!
//! a second leg holds `unused-import` to the same contract: each such block has its unused
//! imports removed by `by check --fix`, and must then check the same and do the same at
//! runtime — every soundness check on, so a check that reads an import is run. a removed
//! import whose binding the transpiled program still reads, because the lowering spelled a
//! name the checker never recorded a read of, shows up as a program that behaves
//! differently, or no longer checks. it sees only what this corpus exercises: an import read
//! only by a lowering, in a checker-clean block of a single file. a multi-file section is not
//! run, so a read across modules is out of its reach, as is any lowering no such block makes
//!
//! each block runs as a program of its own, where mdtest checks the blocks of a
//! section as one file, so a block that builds on an earlier one in its section
//! repeats what it needs of it

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use test_case::test_case;

fn mdtest_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../ty_python_semantic/resources/mdtest")
}

/// The interpreter the blocks run on: `$PYTHON` when it is set, which is the
/// interpreter the run was pinned to, and otherwise a CPython 3.13 provisioned
/// through uv, so the harness runs the same interpreter everywhere instead of
/// riding on whatever `python3` the host happens to ship. Every block is
/// transpiled for that interpreter's version, so what runs is the lowering for
/// it, and a section whose blocks need a newer one is listed in [`REQUIRES`].
/// Returns `None` (the test then skips) when uv or the interpreter can't be
/// obtained.
#[cfg(not(windows))]
fn python() -> Option<String> {
    if let Ok(p) = std::env::var("PYTHON") {
        return Some(p);
    }
    let find = || {
        let out = Command::new("uv")
            .args(["python", "find", "3.13"])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    if let Some(path) = find() {
        return Some(path);
    }
    // not discoverable yet — let uv download a managed build, then locate it
    Command::new("uv")
        .args(["python", "install", "3.13"])
        .output()
        .ok()?;
    find()
}

/// On windows the harness is skipped unless `PYTHON` is set explicitly: it drives
/// a python subprocess whose interpreter discovery and stdout encoding differ
/// from unix, and the checker/runtime contract it validates is platform
/// independent, so unix coverage suffices.
#[cfg(windows)]
fn python() -> Option<String> {
    std::env::var("PYTHON").ok()
}

/// `major.minor` of the interpreter the blocks will execute on, so the
/// transpile targets what it actually supports.
fn python_version(python: &str) -> Option<String> {
    let output = Command::new(python)
        .arg("-c")
        .arg("import sys; print(f'{sys.version_info[0]}.{sys.version_info[1]}')")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// a `by` fenced code block of an mdtest file
#[derive(Debug, PartialEq)]
struct Block {
    source: String,
    /// whether the block lives in a multi-file section (one that declares
    /// companion modules via a `` `name.py`: `` marker). those import
    /// section-local modules and cannot run standalone
    multi_file: bool,
    /// the text of the heading the block is under
    heading: String,
}

/// a checker-clean block the harness runs
struct Item {
    name: String,
    index: usize,
    source: String,
    /// the entry of [`REQUIRES`] that speaks for the block on this interpreter
    requires: Option<usize>,
    /// the entry of [`DIVERGES_BELOW`] that speaks for the block on this interpreter
    diverges_below: Option<usize>,
}

/// whether `error` is the transpiler refusing a block for needing a python newer than
/// `3.{minor}`, and no newer than `3.{floor}`
fn refuses_for_version(error: &str, minor: u8, floor: u8) -> bool {
    (minor + 1..=floor)
        .any(|needed| error.contains(&format!("requires python 3.{needed} or newer")))
}

/// extract the `by` fenced code blocks of a markdown file, in order
fn by_blocks(markdown: &str) -> Vec<Block> {
    let mut blocks: Vec<(String, usize)> = Vec::new();
    let mut multi_file_sections: Vec<usize> = Vec::new();
    let mut headings: Vec<String> = vec![String::new()];
    let mut section = 0usize;
    // the backtick run of the fence currently open, if any. every fence is
    // tracked, not just the `by` ones: a `# error:` comment at column 0 inside a
    // ```py or ```byi block is python code, and counting it as a heading would
    // split the section, separating a companion-module marker from the ```by
    // block that imports it
    let mut fence: Option<usize> = None;
    let mut current: Option<String> = None;
    for line in markdown.lines() {
        let trimmed = line.trim();
        let info = trimmed.trim_start_matches('`');
        let ticks = trimmed.len() - info.len();
        if let Some(open) = fence {
            // a closing fence is a bare run at least as long as the opening one
            if ticks >= open && info.is_empty() {
                fence = None;
                if let Some(block) = current.take() {
                    blocks.push((block, section));
                }
            } else if let Some(block) = &mut current {
                block.push_str(line);
                block.push('\n');
            }
            continue;
        }
        if ticks >= 3 {
            fence = Some(ticks);
            if info.trim() == "by" {
                current = Some(String::new());
            }
            continue;
        }
        if line.starts_with('#') {
            section += 1;
            headings.push(line.trim_start_matches('#').trim().to_owned());
        }
        // a companion-module marker: `` `pylib.py`: `` ahead of its fence
        if trimmed.starts_with('`')
            && (trimmed.ends_with(".py`:")
                || trimmed.ends_with(".by`:")
                || trimmed.ends_with(".byi`:"))
        {
            multi_file_sections.push(section);
        }
    }
    blocks
        .into_iter()
        .map(|(source, section)| Block {
            source,
            multi_file: multi_file_sections.contains(&section),
            heading: headings[section].clone(),
        })
        .collect()
}

/// Whether the block asserts diagnostics of its own, and so has intentionally
/// unspecified runtime behaviour. A `# snapshot` marker counts exactly as an
/// `# error:` one does: it *replaces* the error assertion, so a block carrying
/// only snapshots is not checker-clean either — and a snapshot of an
/// `unresolved-reference` names something that is undefined at runtime too.
fn has_expected_diagnostics(block: &str) -> bool {
    block.contains("# error:") || block.contains("# snapshot")
}

fn transpile(source: &str, min_version: &str) -> Result<String, String> {
    // soundness checks are intended runtime behavior, but mdtest snippets are
    // full of stub bodies (`def f[T]() -> T: ...`) that legitimately violate
    // their annotations when executed — the checks would report those as
    // divergences rather than transpiler bugs, so this harness opts out
    transpile_with(source, min_version, "none")
}

fn transpile_with(source: &str, min_version: &str, soundness: &str) -> Result<String, String> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_by"))
        .args([
            "transpile",
            "--min-version",
            min_version,
            "--soundness",
            soundness,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn by");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

/// the error-level diagnostics `by check` reports for the module `dir/main.by`, without their
/// positions, which removing an import shifts
fn check_errors(dir: &Path) -> Vec<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .args([
            "check",
            "--output-format",
            "concise",
            "--exit-zero",
            "--no-progress",
        ])
        .arg("main.by")
        .current_dir(dir)
        .output()
        .expect("failed to spawn by");
    let mut errors: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(": error["))
        .map(|line| line.splitn(4, ':').nth(3).unwrap_or(line).trim().to_owned())
        .collect();
    errors.sort();
    errors
}

/// what the module `source` does when it is transpiled with every soundness check on and
/// run as `dir/main.py`: whether it transpiles, its exit status and its output, with `dir`
/// left out so two directories compare
fn behaviour(source: &str, min_version: &str, python: &str, dir: &Path) -> String {
    let transpiled = match transpile_with(source, min_version, "all") {
        Ok(transpiled) => transpiled,
        Err(error) => return format!("transpile failed:\n{error}"),
    };
    let py = dir.join("main.py");
    fs::write(&py, with_reveal_stub(&transpiled)).unwrap();
    let run = Command::new(python)
        .arg(&py)
        .current_dir(dir)
        .output()
        .expect("failed to run python");
    let text = format!(
        "status {:?}\n{}\n{}",
        run.status.code(),
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );
    text.replace(&dir.to_string_lossy().into_owned(), "<dir>")
}

/// the second leg: remove the block's unused imports with `by check --fix`, and require the
/// result to check and behave as the block did. `None` when it does, or when there was
/// nothing to remove
fn unused_import_leg(
    block: &str,
    min_version: &str,
    python: &str,
    root: &Path,
    key: &str,
) -> Option<String> {
    let project = format!(
        "[project]\nname = \"leg\"\nversion = \"0\"\nrequires-python = \">={min_version}\"\n"
    );
    let original = root.join(format!("{key}_written"));
    let fixed = root.join(format!("{key}_fixed"));
    for dir in [&original, &fixed] {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("pyproject.toml"), &project).unwrap();
        fs::write(dir.join("main.by"), block).unwrap();
    }
    Command::new(env!("CARGO_BIN_EXE_by"))
        .args([
            "check",
            "--ignore",
            "all",
            "--warn",
            "unused-import",
            "--fix",
            "--exit-zero",
            "--no-progress",
        ])
        .arg("main.by")
        .current_dir(&fixed)
        .output()
        .expect("failed to spawn by");
    let without = fs::read_to_string(fixed.join("main.by")).unwrap();
    if without == block {
        return None;
    }
    let checked = (check_errors(&original), check_errors(&fixed));
    if checked.0 != checked.1 {
        return Some(format!(
            "removing its unused imports changed what the checker reports:\nbefore: {:?}\nafter: {:?}\n--- after ---\n{without}",
            checked.0, checked.1
        ));
    }
    let behaved = (
        behaviour(block, min_version, python, &original),
        behaviour(&without, min_version, python, &fixed),
    );
    (behaved.0 != behaved.1).then(|| {
        format!(
            "removing its unused imports changed what the program does:\n--- before ---\n{}\n--- after ---\n{}\n--- without the imports ---\n{without}",
            behaved.0, behaved.1
        )
    })
}

/// the sections whose checker-clean blocks do not run, each with the reason, keyed
/// by mdtest file (relative to the mdtest directory) and heading text
///
/// each entry is a known divergence that is not fixed yet. a block that is not
/// meant to run as a program has no place here: it is written to run, or asserts
/// the diagnostic that leaves its runtime behaviour unspecified. an entry that no
/// longer matches a checker-clean block fails the test, so the list cannot
/// outlive its reasons
const EXCLUDED: &[(&str, &str, &str)] = &[];

/// the sections a transpile for an older python refuses, each with the version its blocks
/// need and the reason, keyed by mdtest file (relative to the mdtest directory) and heading
/// text. a heading of `None` stands for every section of the file
///
/// below its version, a block of such a section is either refused with the transpiler's
/// version requirement — `requires python 3.N or newer`, for an `N` the interpreter is older
/// than — or transpiled and run like any other block, and a refusal is not a divergence. an
/// entry that no longer matches a checker-clean block fails the test, and so does one that
/// none of its blocks is refused for below its version
const REQUIRES: &[(&str, Option<&str>, u8, &str)] = &[
    (
        "basedpython_reified_generics.md",
        None,
        13,
        "a reified generic keeps its native pep 695 type-parameter list, which python has from \
         3.12, and a pep 696 default in it, which python has from 3.13",
    ),
    (
        "basedpython_reified_class_generics.md",
        None,
        13,
        "a reified class keeps its native pep 695 type-parameter list, which python has from \
         3.12, and a pep 696 default in it, which python has from 3.13",
    ),
    (
        "basedpython_checked_cast.md",
        Some("a reified type parameter is checked, not assumed"),
        12,
        "a reified generic keeps its native pep 695 type-parameter list",
    ),
    (
        "basedpython_exceptions.md",
        Some("an explicit specialization is read like a solved call"),
        12,
        "a reified generic keeps its native pep 695 type-parameter list",
    ),
    (
        "basedpython_optional_type.md",
        Some("a wrapped optional read as a value is the wrapper class"),
        12,
        "a reified generic keeps its native pep 695 type-parameter list",
    ),
    (
        "basedpython_parametric_is.md",
        Some("reified type parameters carry the answer"),
        12,
        "a reified generic keeps its native pep 695 type-parameter list",
    ),
    (
        "basedpython_parametric_is.md",
        Some("a tuple target compares each element cell"),
        12,
        "a reified generic keeps its native pep 695 type-parameter list",
    ),
    (
        "basedpython_parametric_is.md",
        Some("a nested generic value unifies structurally"),
        12,
        "a reified generic keeps its native pep 695 type-parameter list",
    ),
    (
        "basedpython_parametric_is.md",
        Some("a reified class answers the probe from its specialization"),
        12,
        "a reified class keeps its native pep 695 type-parameter list",
    ),
];

/// the sections whose checker-clean blocks diverge on a target older than a python version,
/// each with the version and the reason, keyed like [`EXCLUDED`]
///
/// each entry is a known divergence that is not fixed yet. below its version a block of the
/// section still runs, and has to diverge: an entry none of whose blocks diverges there fails
/// the test, so the list cannot outlive its reasons. from its version on, the section runs like
/// any other
const DIVERGES_BELOW: &[(&str, &str, u8, &str)] = &[];

/// every mdtest under `dir`, at any depth, in a stable order
fn markdown_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("mdtest dir") {
            let path = entry.expect("mdtest dir entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Stub `reveal_type` (an mdtest debugging device with no runtime binding)
/// after the `__future__` import, which must stay first.
fn with_reveal_stub(transpiled: &str) -> String {
    const STUB: &str = "def reveal_type(x, *a, **k):\n    return x\n";
    match transpiled.strip_prefix("from __future__ import annotations\n") {
        Some(rest) => format!("from __future__ import annotations\n{STUB}{rest}"),
        None => format!("{STUB}{transpiled}"),
    }
}

/// how many tests the blocks are spread over. each is a test of its own, so nextest runs them
/// beside one another and holds each to its own time limit, where one test running the whole
/// corpus was held to a limit that the corpus outgrows as features add blocks
const SHARDS: u64 = 8;

/// the shard the blocks of the mdtest at `name` run in. a path's FNV-1a hash keeps a file in
/// one shard as others come and go, and spreads the files without a list to rebalance. an
/// entry of [`EXCLUDED`], [`REQUIRES`] or [`DIVERGES_BELOW`] goes to its file's shard the same
/// way, so the shard that runs a section is the one that checks the entries naming it
fn shard_of(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    }) % SHARDS
}

#[test_case(0)]
#[test_case(1)]
#[test_case(2)]
#[test_case(3)]
#[test_case(4)]
#[test_case(5)]
#[test_case(6)]
#[test_case(7)]
fn clean_mdtest_blocks_run(shard: u64) {
    run_shard(shard);
}

/// the checker-clean blocks of the mdtests in `shard`, each transpiled and run
#[expect(
    clippy::print_stderr,
    reason = "skip diagnostic when python is unavailable"
)]
fn run_shard(shard: u64) {
    assert!(shard < SHARDS, "shard {shard} is past the last of {SHARDS}");
    let Some(python) = python() else {
        eprintln!("skipping: no python 3.13 interpreter available (uv on unix, or set PYTHON)");
        return;
    };
    let Some(version) = python_version(&python) else {
        eprintln!("skipping: `{python}` not runnable");
        return;
    };

    let Some(minor) = version
        .split_once('.')
        .and_then(|(_, minor)| minor.parse::<u8>().ok())
    else {
        panic!("`{python}` reports a version that is not `3.N`: {version}");
    };

    // third-party runtime deps are environment-dependent; skip blocks that
    // need one the interpreter doesn't have
    let has_typing_extensions = Command::new(&python)
        .args(["-c", "import typing_extensions"])
        .output()
        .is_ok_and(|o| o.status.success());

    // the pydantic divergence suite needs the framework installed to execute;
    // when it isn't present those blocks are skipped exactly like the
    // typing_extensions ones. run them locally against an interpreter that has
    // pydantic (e.g. `PYTHON=/path/to/venv/bin/python`) to enforce the contract
    let has_pydantic = Command::new(&python)
        .args(["-c", "import pydantic"])
        .output()
        .is_ok_and(|o| o.status.success());

    // the sqlalchemy divergence suite likewise needs the framework installed to
    // execute; skip its blocks when it isn't present, and run them locally
    // against an interpreter that has sqlalchemy to enforce the contract
    let has_sqlalchemy = Command::new(&python)
        .args(["-c", "import sqlalchemy"])
        .output()
        .is_ok_and(|o| o.status.success());

    // the django mdtests run their models against the framework; skip their
    // blocks when it isn't installed, and run them locally against an
    // interpreter that has django to enforce the contract
    let has_django = Command::new(&python)
        .args(["-c", "import django"])
        .output()
        .is_ok_and(|o| o.status.success());

    // the pytest divergence suite drives real pytest over the transpiled output
    // (each block re-invokes `pytest.main` on itself under `__main__`); skip its
    // blocks when pytest isn't installed, and run them locally against an
    // interpreter that has pytest to enforce the contract
    let has_pytest = Command::new(&python)
        .args(["-c", "import pytest"])
        .output()
        .is_ok_and(|o| o.status.success());

    // the basedpython-ui suite needs the framework installed to execute: its
    // blocks import `basedpython_ui`, and the mocks the mdtests declare live in
    // the checker's own file system rather than on disk. skipped exactly like
    // the other frameworks; run them locally against an interpreter that has
    // basedpython_ui to enforce the contract
    let has_basedpython_ui = Command::new(&python)
        .args(["-c", "import basedpython_ui"])
        .output()
        .is_ok_and(|o| o.status.success());

    // `frozendict` is a 3.15 builtin, so on the 3.13 floor this harness targets
    // its blocks cannot run at all. skipped exactly like a missing third-party
    // dependency; run them locally against a 3.15 interpreter to enforce the
    // contract
    let has_frozendict = Command::new(&python)
        .args(["-c", "frozendict"])
        .output()
        .is_ok_and(|o| o.status.success());
    // `string.templatelib` arrived in 3.14, and a block importing a name from it to
    // annotate with imports the module where the statement stands — the lazy-import
    // polyfill keeps a name an annotation reads eager
    let has_templatelib = Command::new(&python)
        .args(["-c", "import string.templatelib"])
        .output()
        .is_ok_and(|o| o.status.success());

    let dir = mdtest_dir();
    let files = markdown_files(&dir);
    assert!(!files.is_empty(), "no mdtests found in {dir:?}");

    // gather every runnable block up front — skipping ones with expected
    // diagnostics (only checker-clean blocks carry the contract) or that import
    // section-local companion modules this harness doesn't materialize — so the
    // work can be spread across a pool of workers rather than run serially. each
    // block is an independent `by transpile` + python subprocess pair, so the
    // harness is dominated by process spawn latency and parallelises cleanly.
    //
    // a block naming a module this interpreter does not have is dropped here rather
    // than after transpiling it. the checks below still have to run, because the
    // transpiler can *introduce* one of these — lowering `float` pulls in
    // `ty_extensions` — but it never drops an import the source wrote, so a source
    // mention is a subset of what those checks would catch. transpiling one only to
    // throw the result away costs a `by` spawn, which reads typeshed at startup and
    // is the single most expensive thing this harness does per block
    let unavailable: Vec<&str> = [
        (!has_typing_extensions).then_some("typing_extensions"),
        (!has_frozendict).then_some("frozendict"),
        (!has_templatelib).then_some("string.templatelib"),
        (!has_pydantic).then_some("pydantic"),
        (!has_sqlalchemy).then_some("sqlalchemy"),
        (!has_django).then_some("django"),
        (!has_pytest).then_some("pytest"),
        (!has_basedpython_ui).then_some("basedpython_ui"),
        Some("ty_extensions"),
    ]
    .into_iter()
    .flatten()
    .collect();

    let mut items: Vec<Item> = Vec::new();
    let mut excluded_seen = vec![false; EXCLUDED.len()];
    let mut requires_seen = vec![false; REQUIRES.len()];
    let mut diverges_below_seen = vec![false; DIVERGES_BELOW.len()];
    for file in &files {
        let name = file
            .strip_prefix(&dir)
            .expect("an mdtest is under the mdtest directory")
            .to_string_lossy()
            .replace('\\', "/");
        if shard_of(&name) != shard {
            continue;
        }
        let markdown = fs::read_to_string(file).expect("read mdtest");
        for (i, block) in by_blocks(&markdown).into_iter().enumerate() {
            if has_expected_diagnostics(&block.source) || block.multi_file {
                continue;
            }
            if let Some(excluded) = EXCLUDED
                .iter()
                .position(|(file, heading, _)| *file == name && *heading == block.heading)
            {
                excluded_seen[excluded] = true;
                continue;
            }
            let requires = REQUIRES.iter().position(|(file, heading, _, _)| {
                *file == name && heading.is_none_or(|heading| heading == block.heading)
            });
            if let Some(entry) = requires {
                requires_seen[entry] = true;
            }
            let diverges_below = DIVERGES_BELOW
                .iter()
                .position(|(file, heading, _, _)| *file == name && *heading == block.heading);
            if let Some(entry) = diverges_below {
                diverges_below_seen[entry] = true;
            }
            if unavailable
                .iter()
                .any(|module| block.source.contains(module))
            {
                continue;
            }
            items.push(Item {
                name: name.clone(),
                index: i,
                source: block.source,
                // an entry only speaks for an interpreter older than its version
                requires: requires.filter(|&entry| minor < REQUIRES[entry].2),
                diverges_below: diverges_below.filter(|&entry| minor < DIVERGES_BELOW[entry].2),
            });
        }
    }
    let stale: Vec<String> = EXCLUDED
        .iter()
        .zip(&excluded_seen)
        .filter(|((file, ..), seen)| shard_of(file) == shard && !**seen)
        .map(|((file, heading, _), _)| format!("{file}: {heading}"))
        .chain(
            REQUIRES
                .iter()
                .zip(&requires_seen)
                .filter(|((file, ..), seen)| shard_of(file) == shard && !**seen)
                .map(|((file, heading, _, _), _)| {
                    format!("{file}: {}", heading.unwrap_or("every section"))
                }),
        )
        .chain(
            DIVERGES_BELOW
                .iter()
                .zip(&diverges_below_seen)
                .filter(|((file, ..), seen)| shard_of(file) == shard && !**seen)
                .map(|((file, heading, _, _), _)| format!("{file}: {heading}")),
        )
        .collect();
    assert!(
        stale.is_empty(),
        "a listed section no longer holds a checker-clean block; remove it from `EXCLUDED`, \
         `REQUIRES` or `DIVERGES_BELOW`:\n{}",
        stale.join("\n")
    );
    let total = items.len();
    assert!(total > 0, "no checker-clean by blocks found");

    let tmp = tempfile::tempdir().expect("tempdir");
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    // the entries of `REQUIRES` a block was refused for, and of `DIVERGES_BELOW` a block
    // diverged for, on this interpreter
    let refused: Mutex<Vec<bool>> = Mutex::new(vec![false; REQUIRES.len()]);
    let diverged: Mutex<Vec<bool>> = Mutex::new(vec![false; DIVERGES_BELOW.len()]);
    // a divergence of a block `DIVERGES_BELOW` lists for this interpreter is the one expected
    // of it, and any other is reported
    let diverge = |item: &Item, report: String| match item.diverges_below {
        Some(entry) => diverged.lock().unwrap()[entry] = true,
        None => failures.lock().unwrap().push(report),
    };
    let next = AtomicUsize::new(0);
    // each block alternates a cpu-bound `by transpile` with a wait on the python
    // subprocess, and `by` startup also reads typeshed from disk — so a little
    // oversubscription past the core count overlaps that i/o and keeps the cores
    // busy, while the cap bounds how many processes are ever live at once
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get() * 2)
        .clamp(1, 16)
        .min(total);

    // work-stealing pool: each worker claims the next index and processes it
    // until the list is drained. the closures borrow the shared state (the
    // work list, the cursor, the failure sink) so `failures` stays owned for
    // the drain below. temp files are keyed by `name_i`, unique per block, so
    // concurrent writes never collide.
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while let Some(item) = items.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let Item {
                        name,
                        index: i,
                        source: block,
                        ..
                    } = item;
                    let transpiled = match transpile(block, &version) {
                        Ok(t) => t,
                        Err(e) => {
                            if let Some(entry) = item.requires
                                && refuses_for_version(&e, minor, REQUIRES[entry].2)
                            {
                                refused.lock().unwrap()[entry] = true;
                                continue;
                            }
                            diverge(
                                item,
                                format!(
                                    "{name} block {i}: transpile failed:\n{e}\n--- block ---\n{block}"
                                ),
                            );
                            continue;
                        }
                    };
                    // `ty_extensions` is a checker-only surface: the predicates
                    // a block asserts with it (`static_assert`,
                    // `is_deeply_immutable`) are answered during checking and
                    // there is no runtime module behind them, on any
                    // interpreter. such a block has no runtime behaviour to
                    // diverge, so it is skipped outright rather than gated on a
                    // dependency that could never be installed
                    if transpiled.contains("ty_extensions") {
                        continue;
                    }
                    if !has_typing_extensions && transpiled.contains("typing_extensions") {
                        continue;
                    }
                    if !has_frozendict && transpiled.contains("frozendict") {
                        continue;
                    }
                    if !has_pydantic && transpiled.contains("pydantic") {
                        continue;
                    }
                    if !has_sqlalchemy && transpiled.contains("sqlalchemy") {
                        continue;
                    }
                    if !has_django && transpiled.contains("django") {
                        continue;
                    }
                    if !has_pytest && transpiled.contains("pytest") {
                        continue;
                    }
                    if !has_basedpython_ui && transpiled.contains("basedpython_ui") {
                        continue;
                    }
                    let py = tmp.path().join(format!(
                        "{}_{i}.py",
                        name.trim_end_matches(".md").replace(['-', '/'], "_")
                    ));
                    fs::write(&py, with_reveal_stub(&transpiled)).unwrap();
                    let run = Command::new(&python)
                        .arg(&py)
                        .output()
                        .expect("failed to run python");
                    if !run.status.success() {
                        diverge(
                            item,
                            format!(
                                "{name} block {i}: checker-clean block crashed at runtime:\n{}\n--- block ---\n{block}",
                                String::from_utf8_lossy(&run.stderr)
                            ),
                        );
                        continue;
                    }
                    let key = format!("{}_{i}", name.trim_end_matches(".md").replace(['-', '/'], "_"));
                    if let Some(divergence) =
                        unused_import_leg(block, &version, &python, tmp.path(), &key)
                    {
                        failures.lock().unwrap().push(format!(
                            "{name} block {i}: {divergence}\n--- block ---\n{block}"
                        ));
                    }
                }
            });
        }
    });

    let mut failures = failures.into_inner().expect("failures mutex poisoned");
    let refused = refused.into_inner().expect("refusals mutex poisoned");
    let diverged = diverged.into_inner().expect("divergences mutex poisoned");
    for (entry, (file, heading, floor, _)) in REQUIRES.iter().enumerate() {
        if shard_of(file) == shard && minor < *floor && !refused[entry] {
            failures.push(format!(
                "{file}: {}: no block is refused below 3.{floor} any more; remove it from `REQUIRES`",
                heading.unwrap_or("every section")
            ));
        }
    }
    for (entry, (file, heading, floor, _)) in DIVERGES_BELOW.iter().enumerate() {
        if shard_of(file) == shard && minor < *floor && !diverged[entry] {
            failures.push(format!(
                "{file}: {heading}: no block diverges below 3.{floor} any more; remove it from \
                 `DIVERGES_BELOW`"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} checker-clean blocks diverge at runtime:\n\n{}",
        failures.len(),
        total,
        failures.join("\n\n")
    );
}

/// A `#` comment inside a non-`by` fence is code, not a heading: counting it as
/// one would move the following `by` block into a section of its own, away from
/// the companion-module marker, and the harness would try to run it standalone.
#[test]
fn comment_in_companion_fence_keeps_section() {
    let markdown = "\
## a section

`m.byi`:

```byi
# error: [some-rule]
def f() -> None: ...
```

```by
from m import f
f()
```
";
    let blocks = by_blocks(markdown);
    assert_eq!(
        blocks,
        vec![Block {
            source: "from m import f\nf()\n".to_string(),
            multi_file: true,
            heading: "a section".to_string(),
        }],
        "the block belongs to the marker's section"
    );
}

#[test]
fn sections_split_on_headings_outside_fences() {
    let markdown = "\
## multi-file

`m.by`:

```by
def f() -> None: ...
```

```by
from m import f
```

## standalone

```by
print(1)
```
";
    let block = |source: &str, multi_file: bool, heading: &str| Block {
        source: source.to_string(),
        multi_file,
        heading: heading.to_string(),
    };
    assert_eq!(
        by_blocks(markdown),
        vec![
            block("def f() -> None: ...\n", true, "multi-file"),
            block("from m import f\n", true, "multi-file"),
            block("print(1)\n", false, "standalone"),
        ]
    );
}
