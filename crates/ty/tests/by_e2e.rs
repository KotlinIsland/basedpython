use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use ty_static::EnvVars;

/// a temp directory of this process's own
///
/// the same reason the `by_build` suites have one: a fixed path under the system temp
/// directory is shared by two concurrent runs, which then overwrite each other between
/// the build and the read
fn cli_root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("by_cli_p{}", std::process::id()))
}

fn transpile(source: &str) -> String {
    let raw = run_transpile(source, &[]);
    // the future import is opt-in, so it's normally absent. a few inputs
    // (e.g. user-written `from __future__`) can still surface it first;
    // strip it here so tests assert on the user-relevant tail either way
    raw.strip_prefix("from __future__ import annotations\n")
        .map(str::to_owned)
        .unwrap_or(raw)
}

fn reverse_transpile(source: &str) -> String {
    run_transpile(source, &["--reverse"])
}

fn run_transpile(source: &str, extra_args: &[&str]) -> String {
    // Cargo sets `CARGO_BIN_EXE_<name>` for integration tests, pointing to
    // the binary built in the same package. The `ty` crate's binary is
    // `by`, so we use its compiled path rather than relying on `by` being
    // on `$PATH`
    let bin = env!("CARGO_BIN_EXE_by");
    let mut cmd = Command::new(bin);
    cmd.arg("transpile");
    cmd.args(extra_args);
    let mut child = cmd
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
    assert!(
        output.status.success(),
        "by exited with error:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// A project whose program reads a data file sitting beside it.
///
/// Written by each `compile` staging test, which then differ only in what they do
/// to the tree afterwards.
fn resource_project(name: &str) -> PathBuf {
    let dir = cli_root().join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("data")).unwrap();
    fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname=\"s\"\nversion=\"0\"\nrequires-python=\">=3.13\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("data").join("config.json"),
        "{\"greeting\": \"hi\"}\n",
    )
    .unwrap();
    fs::write(
        dir.join("helper.py"),
        "def helper() -> int:\n    return 1\n",
    )
    .unwrap();
    fs::write(
        dir.join("main.by"),
        "from pathlib import Path\n\n\ndef go() -> str:\n    \
         return (Path(__file__).parent / \"data\" / \"config.json\").read_text()\n",
    )
    .unwrap();
    dir
}

/// Whether `by` refused because this host's interpreter is below the native floor.
///
/// `by compile` probes an interpreter before it does anything, and
/// `by_build::MINIMUM_PYTHON` refuses one older than 3.11 by name. On a host whose
/// ambient `python3` is older that is not a failure of the code under test, and a
/// test that asserted its way through it reported a wall of unrelated noise — so
/// every `compile` test here skips on it, the way `by_build`'s own suite does.
/// Pin `PYTHON` to a 3.11+ interpreter to actually run them.
#[allow(
    clippy::print_stderr,
    reason = "skip notices belong on the harness's stderr"
)]
fn refused_for_python_version(result: &std::process::Output) -> bool {
    let refused = String::from_utf8_lossy(&result.stderr)
        .contains("a native build needs python 3.11 or later");
    if refused {
        eprintln!("skipping: this host's python is below the native compilation floor");
    }
    refused
}

/// Run `by compile` in `dir`, or `None` when this host cannot compile natively.
fn compile_in(dir: &Path) -> Option<std::process::Output> {
    let result = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["compile", "--emit-c-only"])
        .current_dir(dir)
        .output()
        .expect("failed to spawn by");
    if refused_for_python_version(&result) {
        return None;
    }
    assert!(
        result.status.success(),
        "by exited with error:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    Some(result)
}

#[test]
fn compile_carries_the_rest_of_the_project_into_the_output_tree() {
    // a compiled module is only half of a project. `main.by` reads its data file
    // relative to itself, and the extension's `__file__` is its own place in the
    // output tree — so a tree holding artefacts and nothing else fails on the
    // first `open`, with a `FileNotFoundError` naming a path in a directory the
    // author never wrote anything to. `compile` writes everything `build` writes
    // and the extensions as well, so a compiled module finds its data exactly
    // where its interpreted twin would
    let dir = resource_project("by_cli_compile_resources");
    if compile_in(&dir).is_none() {
        return;
    }

    let out = dir.join("build");
    assert!(out.join("main.c").exists(), "the module is compiled");
    assert!(
        out.join("data").join("config.json").exists(),
        "a data file lands at the same relative place it had in the source"
    );
    assert!(
        out.join("helper.py").exists(),
        "a hand-written python module beside the source is carried over too"
    );
    assert!(
        out.join("main.py").exists(),
        "the `.by` is transpiled too, so the module imports whether or not its \
         extension was built"
    );
    assert!(
        out.join("_by_sourcemap.py").exists() && out.join("_by_build.json").exists(),
        "the tree describes itself, the way a `by build` tree does"
    );
}

#[test]
fn compiling_one_module_leaves_the_others_importable() {
    // the docs offer `by compile app.hot` as "compile one module, leave the rest
    // interpreted". while `compile` wrote artefacts alone that was not what
    // happened: the modules nobody named reached the tree as `.by`, which python
    // cannot import, and the second invocation's `finish` took the first's
    // artefact back as well — so `compile a` then `compile b` left a tree that
    // could import neither
    let dir = resource_project("by_cli_compile_one_of_many");
    fs::write(dir.join("other.by"), "def other() -> int:\n    return 2\n").unwrap();

    let compile_one = |name: &str| -> bool {
        let result = Command::new(env!("CARGO_BIN_EXE_by"))
            .args(["compile", "--emit-c-only", name])
            .current_dir(&dir)
            .output()
            .expect("failed to spawn by");
        if refused_for_python_version(&result) {
            return false;
        }
        assert!(
            result.status.success(),
            "by exited with error:\n{}",
            String::from_utf8_lossy(&result.stderr)
        );
        true
    };
    let out = dir.join("build");

    if !compile_one("main.by") {
        return;
    }
    assert!(out.join("main.c").exists(), "the named module is compiled");
    assert!(
        out.join("other.py").exists(),
        "a module nobody named still reaches the tree as importable python"
    );

    assert!(compile_one("other.by"), "the second compile ran");
    assert!(
        out.join("other.c").exists(),
        "the newly named one is compiled"
    );
    assert!(
        out.join("main.py").exists(),
        "and the previously named one is still importable"
    );
}

#[test]
fn a_compile_leaves_a_build_tree_readable() {
    // `compile` and `build` write to the same directory by design. while
    // `compile` wrote artefacts alone it took the sourcemap and the build record
    // with it, and `by restage` — the language server's single-file re-stage —
    // then refused the tree for having no `_by_build.json`, so a compile silently
    // disabled the editor plugin against it
    let dir = resource_project("by_cli_compile_then_restage");
    let result = Command::new(env!("CARGO_BIN_EXE_by"))
        .arg("build")
        .current_dir(&dir)
        .output()
        .expect("failed to spawn by");
    assert!(result.status.success());
    if compile_in(&dir).is_none() {
        return;
    }

    let restaged = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["restage", "build", "main.by"])
        .current_dir(&dir)
        .output()
        .expect("failed to spawn by");
    let answer = String::from_utf8_lossy(&restaged.stdout);
    assert!(
        answer.contains("\"generated\""),
        "the tree is still one a re-stage can read:\n{answer}"
    );
}

#[test]
fn a_build_with_nothing_to_write_leaves_no_output_directory() {
    // `by build` created the output directory before it knew whether it had
    // anything to put in it, so a project with no `.by` files — one whose sources
    // are all python, say — was left holding an empty `build/` it never asked for
    let dir = cli_root().join("by_cli_build_no_litter");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname=\"s\"\nversion=\"0\"\nrequires-python=\">=3.13\"\n",
    )
    .unwrap();
    fs::write(dir.join("only.py"), "x = 1\n").unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_by"))
        .arg("build")
        .current_dir(&dir)
        .output()
        .expect("failed to spawn by");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "by build failed:\n{stderr}");
    assert!(
        stderr.contains("no .by files found"),
        "the build found nothing to do:\n{stderr}"
    );
    assert!(
        !dir.join("build").exists(),
        "a build that wrote nothing left a directory behind:\n{stderr}"
    );
}

#[test]
fn a_second_output_directory_is_not_carried_into_the_first() {
    // a project can have more than one output tree — `by build --out one` beside
    // `by build --out two`. only the directory *this* run was given is known to
    // be an output from its arguments; the other is recognised by the
    // `.by-manifest` it carries, and without that it is carried over as though it
    // were source, putting a whole copy of one tree inside the other
    let dir = cli_root().join("by_cli_two_outputs");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname=\"s\"\nversion=\"0\"\nrequires-python=\">=3.13\"\n",
    )
    .unwrap();
    fs::write(dir.join("main.by"), "x = 1\n").unwrap();
    fs::write(dir.join("data.json"), "{}\n").unwrap();

    let build_into = |name: &str| {
        let result = Command::new(env!("CARGO_BIN_EXE_by"))
            .args(["build", "--out", name])
            .current_dir(&dir)
            .output()
            .expect("failed to spawn by");
        assert!(
            result.status.success(),
            "by build failed:\n{}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    build_into("one");
    build_into("two");

    assert!(dir.join("two").join("main.py").exists(), "the build ran");
    assert!(
        !dir.join("two").join("one").exists(),
        "the first output tree was copied into the second"
    );
}

#[test]
fn compile_takes_back_what_the_previous_compile_wrote() {
    // the output tree is a mirror rather than a pile: a resource deleted from the
    // source is deleted from the tree. without the manifest it would keep being
    // read, and a wheel built from the same tree would ship it
    let dir = resource_project("by_cli_compile_stale");
    if compile_in(&dir).is_none() {
        return;
    }
    let out = dir.join("build");
    assert!(out.join("data").join("config.json").exists());

    fs::remove_file(dir.join("data").join("config.json")).unwrap();
    assert!(compile_in(&dir).is_some(), "the second compile ran");
    assert!(
        !out.join("data").join("config.json").exists(),
        "a resource the source no longer has is taken back out of the tree"
    );
}

#[test]
fn a_build_takes_back_the_artifacts_a_compile_left() {
    // `compile` and `build` write to the same directory by default, and python's
    // finder prefers an extension to source. an artefact left behind by an
    // earlier `compile` would therefore go on shadowing the `.py` this `build`
    // writes in its place — which is why `compile` records what it produced even
    // though `by_build` is what laid it out
    let dir = resource_project("by_cli_compile_then_build");
    if compile_in(&dir).is_none() {
        return;
    }
    let out = dir.join("build");
    assert!(out.join("main.c").exists());

    let result = Command::new(env!("CARGO_BIN_EXE_by"))
        .arg("build")
        .current_dir(&dir)
        .output()
        .expect("failed to spawn by");
    assert!(
        result.status.success(),
        "by exited with error:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(out.join("main.py").exists(), "the build wrote the module");
    assert!(
        !out.join("main.c").exists(),
        "what the compile produced is taken back"
    );
}

#[test]
fn compile_refuses_to_carry_a_file_over_an_artifact_it_wrote() {
    // a project can keep a `main.c` of its own beside `main.by` — and the
    // compiler writes its generated C under that same name. carrying the
    // hand-written one over would leave a tree whose generated half is somebody
    // else's file, with nothing said about it, so it is reported the way two
    // sources claiming one module are
    let dir = resource_project("by_cli_compile_artifact_collision");
    fs::write(dir.join("main.c"), "/* hand-written */\n").unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["compile", "--emit-c-only"])
        .current_dir(&dir)
        .output()
        .expect("failed to spawn by");
    if refused_for_python_version(&result) {
        return;
    }
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success(), "by should have failed:\n{stderr}");
    assert!(
        stderr.contains("already wrote an artifact of that name"),
        "the collision is named:\n{stderr}"
    );
    assert!(
        !fs::read_to_string(dir.join("build").join("main.c"))
            .unwrap()
            .contains("hand-written"),
        "the generated C is left as the compiler wrote it"
    );
}

#[test]
fn compile_does_not_read_the_tree_it_writes() {
    // the output holds a copy of every resource the build carried over, including
    // the project's `.py` modules. a second `compile` that walked them would
    // compile each module twice — once from the source and once from the copy —
    // and the two would claim the same artifact
    //
    // the directory is deliberately *not* one of the names the project walk skips
    // by default (`build`, `out`, `target`, …). those are turned away whoever
    // asks, so a tree written to one of them would pass this test even if the
    // build never told the database where its own output was going.
    //
    // two mechanisms keep `generated/` out — the output this run was given, and
    // the `.by-manifest` any output carries — and either alone is enough here.
    // the manifest is the only one that covers an output this run was *not*
    // given, which `a_second_output_directory_is_not_carried_into_the_first`
    // is for
    let dir = resource_project("by_cli_compile_not_own_input");
    let compile = || -> Option<usize> {
        let result = Command::new(env!("CARGO_BIN_EXE_by"))
            .args(["compile", "--emit-c-only", "-o", "generated"])
            .current_dir(&dir)
            .output()
            .expect("failed to spawn by");
        if refused_for_python_version(&result) {
            return None;
        }
        assert!(
            result.status.success(),
            "by exited with error:\n{}",
            String::from_utf8_lossy(&result.stderr)
        );
        // the count `compile` reports, not the per-artifact lines: `--emit-c-only`
        // does not print those, so counting them compares nothing to nothing
        let stderr = String::from_utf8_lossy(&result.stderr);
        let reported = stderr
            .lines()
            .find_map(|line| line.strip_prefix("compiled ")?.strip_suffix(" module(s)"))
            .and_then(|count| count.parse::<usize>().ok());
        Some(reported.unwrap_or_else(|| panic!("no module count in:\n{stderr}")))
    };
    let Some(first) = compile() else {
        return;
    };
    assert!(first > 0, "the first compile compiled something");
    assert_eq!(
        Some(first),
        compile(),
        "the second compile sees the same sources as the first"
    );
}

#[test]
fn compile_emits_only_the_files_it_was_given_and_still_resolves_the_others() {
    // `by compile a.py` used to compile every source in the project and ignore the
    // argument entirely. that is not a harmless superset: it costs every other
    // module's build time, it fails the command for a diagnostic in a file nobody
    // named, and it silently compiles a file sitting beside the one under test —
    // which invalidated a delta-debugging run whose original was in the same
    // directory as each candidate
    //
    // the database still holds the whole project, because a type imported from a
    // sibling has to resolve. that is what `lib.py` is here to prove: it is never
    // compiled, and `wanted.py` still lowers `Point` rather than declining
    let dir = cli_root().join("by_cli_only_named");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname=\"s\"\nversion=\"0\"\nrequires-python=\">=3.13\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("lib.py"),
        "class Point:\n    def __init__(self) -> None:\n        self.x: int = 7\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("wanted.py"),
        "from lib import Point\n\n\ndef go() -> int:\n    p = Point()\n    return p.x\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("other.py"),
        "def unrelated() -> int:\n    return 2\n",
    )
    .unwrap();

    let out = dir.join("build");
    let result = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["compile", "wanted.py", "-o"])
        .arg(&out)
        .arg("--emit-c-only")
        .current_dir(&dir)
        .output()
        .expect("failed to spawn by");
    if refused_for_python_version(&result) {
        return;
    }
    assert!(
        result.status.success(),
        "by exited with error:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );

    assert!(out.join("wanted.c").exists(), "the named file is compiled");
    assert!(
        !out.join("lib.c").exists() && !out.join("other.c").exists(),
        "a file that was not named is not compiled"
    );

    // the cross-module type resolved: a declined body would not carry the
    // attribute read at all
    let emitted = std::fs::read_to_string(out.join("wanted.c")).expect("the C is readable");
    assert!(
        emitted.contains("by_wanted_go"),
        "`go` lowered natively, so `Point` resolved out of the uncompiled sibling"
    );
}

/// write a project of package members under `dir`, each answering with its own
/// dotted name
fn write_package_project(dir: &std::path::Path) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir.join("pkg/sub")).unwrap();
    std::fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname=\"s\"\nversion=\"0\"\nrequires-python=\">=3.13\"\n",
    )
    .unwrap();
    for (path, tag) in [
        ("pkg/__init__.py", 1),
        ("pkg/sub/__init__.py", 2),
        ("pkg/dup.py", 3),
        ("pkg/sub/dup.py", 4),
    ] {
        std::fs::write(
            dir.join(path),
            format!("def tag() -> int:\n    return {tag}\n"),
        )
        .unwrap();
    }
}

#[test]
fn compile_writes_each_package_member_at_its_own_place_in_the_output_tree() {
    // `by compile -o out` used to write every artefact flat, named after the
    // module's last component. two members of a package sharing a last component
    // then wrote the same file and the second silently won — and *no* package
    // member's artefact was importable under the name it had been compiled as,
    // because a flat `dup.so` can only ever be imported as `dup`
    let dir = cli_root().join("by_cli_package_tree");
    write_package_project(&dir);

    let out = dir.join("o");
    let result = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["compile", "-o"])
        .arg(&out)
        .arg("--emit-c-only")
        .current_dir(&dir)
        .output()
        .expect("failed to spawn by");
    if refused_for_python_version(&result) {
        return;
    }
    assert!(
        result.status.success(),
        "by exited with error:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );

    // four sources, four artefacts — a package's own file is the `__init__` inside
    // its directory, which is the only name cpython's finder looks for
    for relative in [
        "pkg/__init__.c",
        "pkg/sub/__init__.c",
        "pkg/dup.c",
        "pkg/sub/dup.c",
    ] {
        assert!(out.join(relative).exists(), "{relative} was written");
    }
    // and nothing named after a last component alone
    assert!(
        !out.join("dup.c").exists() && !out.join("sub.c").exists() && !out.join("pkg.c").exists()
    );

    // the two `dup` members are distinct modules, not one file written twice
    let first = fs::read_to_string(out.join("pkg/dup.c")).unwrap();
    let second = fs::read_to_string(out.join("pkg/sub/dup.c")).unwrap();
    assert!(first.contains("by_pkg_dup_tag"), "{first}");
    assert!(second.contains("by_pkg_sub_dup_tag"), "{second}");
}

#[test]
fn compile_refuses_two_sources_that_would_write_the_same_artifact() {
    // laying the output out as the module tree settles the collision between two
    // package members, but not this one: neither directory here has a name python
    // could import, so neither file has a dotted name and both fall back to their
    // stem. one artefact would be written twice and only the second kept, which is
    // the silent loss the tree was meant to end — so it is refused before anything
    // is written rather than half-performed
    let dir = cli_root().join("by_cli_artifact_clash");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("a-one")).unwrap();
    std::fs::create_dir_all(dir.join("b-two")).unwrap();
    std::fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname=\"s\"\nversion=\"0\"\nrequires-python=\">=3.13\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("a-one/m.py"), "def tag() -> int:\n    return 1\n").unwrap();
    std::fs::write(dir.join("b-two/m.py"), "def tag() -> int:\n    return 2\n").unwrap();

    let out = dir.join("o");
    let result = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["compile", "-o"])
        .arg(&out)
        .arg("--emit-c-only")
        .current_dir(&dir)
        .output()
        .expect("failed to spawn by");
    if refused_for_python_version(&result) {
        return;
    }
    assert!(!result.status.success(), "the clash is refused");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("would both be compiled as the module `m`"),
        "{stderr}"
    );
    // and it said so before writing either one
    assert!(!out.join("m.c").exists(), "{stderr}");
}

#[test]
fn compile_declines_a_package_body_whose_package_has_no_importable_name() {
    // an `__init__.py` is the body of the package its directory names, and `a-one`
    // is not a name python can import — so there is no package for the file to be
    // the body of. compiled under its stem it became a module called `__init__`,
    // which loads and answers `__name__ == "__init__"`: its relative imports have
    // no package to be relative to and its submodules are bound to nothing. a
    // sibling that *is* nameable from its own directory still compiles, because
    // its stem really is the only name it could be imported under
    let dir = cli_root().join("by_cli_unnameable_package");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("a-one")).unwrap();
    std::fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname=\"s\"\nversion=\"0\"\nrequires-python=\">=3.13\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("a-one/__init__.py"), "VALUE = 1\n").unwrap();
    std::fs::write(dir.join("a-one/inner.py"), "VALUE = 2\n").unwrap();

    let out = dir.join("o");
    let result = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["compile", "-o"])
        .arg(&out)
        .arg("--emit-c-only")
        .current_dir(&dir)
        .output()
        .expect("failed to spawn by");
    if refused_for_python_version(&result) {
        return;
    }
    let stderr = String::from_utf8_lossy(&result.stderr);
    // declining one source is not a failed build — the rest of the project is
    // compiled, and what was left out is said rather than silently produced
    assert!(result.status.success(), "{stderr}");
    assert!(stderr.contains("skipping"), "{stderr}");
    assert!(!out.join("__init__.c").exists(), "{stderr}");
    assert!(out.join("inner.c").exists(), "{stderr}");
}

#[test]
fn compile_transpiles_the_fallback_with_the_lowering_options_it_was_given() {
    // a declined function *runs* from the embedded source, so `by compile` has to
    // transpile it with the same options a `by transpile` would use. the library
    // has always taken them; until this reached the cli there was no way to say so,
    // and every compile silently used the defaults
    //
    // an `async def` has no native lowering, so this declines and the fallback
    // is what runs
    let source = "\
async def total(s: str, n: int) -> int:
    return len(s) + n
";
    let dir = cli_root().join("by_cli_soundness");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("sound.by");
    std::fs::write(&file, source).unwrap();

    let emitted = |spec: &str| -> Option<String> {
        let out = dir.join(spec);
        let status = Command::new(env!("CARGO_BIN_EXE_by"))
            .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
            .args(["compile"])
            .arg(&file)
            .arg("-o")
            .arg(&out)
            .args(["--emit-c-only", "--soundness", spec])
            .current_dir(&dir)
            .output()
            .expect("failed to spawn by");
        if refused_for_python_version(&status) {
            return None;
        }
        assert!(
            status.status.success(),
            "by exited with error:\n{}",
            String::from_utf8_lossy(&status.stderr)
        );
        Some(std::fs::read_to_string(out.join("sound.c")).expect("the C is readable"))
    };

    let Some(all) = emitted("all") else {
        return;
    };
    assert!(
        all.contains("_soundness_check"),
        "`all` puts the entry checks in the fallback"
    );
    assert!(
        !emitted("none").is_some_and(|none| none.contains("_soundness_check")),
        "`none` leaves them out, so the flag is what made the difference"
    );
}

#[test]
fn run_executes_module() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "print('hello from by run')\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "hello from by run"
    );
}

/// the program runs out of a directory of transpiled copies that is deleted when
/// the run ends, so every path python derives from a module's origin named a
/// temporary file — leaving a tool started anywhere but the project root with
/// nothing but the cwd to walk up from
#[test]
fn run_names_the_by_source_for_file_and_argv() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("helper.by"),
        "def where() -> str:\n    return __file__\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("main.by"),
        "import sys\nimport helper\n\nprint(__file__)\nprint(sys.argv[0])\nprint(helper.where())\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "unexpected output:\n{stdout}");
    // the entry module, `sys.argv[0]`, and a module the entry imported
    assert!(lines[0].ends_with("main.by"), "__file__ was {}", lines[0]);
    assert!(lines[1].ends_with("main.by"), "argv[0] was {}", lines[1]);
    assert!(
        lines[2].ends_with("helper.by"),
        "an imported module's __file__ was {}",
        lines[2]
    );
}

/// `multiprocessing`'s spawn start method — the default on macos and windows —
/// reads `__main__.__spec__.name` to tell the child what to re-import, and falls
/// back to running `__file__` as a *path* when there is none. `__file__` is a
/// `.by`, which python cannot compile, so a missing spec broke every child
#[test]
fn run_leaves_a_spawned_child_able_to_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    // a basedpython-only construct, so a child that tried to compile the `.by`
    // as python would fail rather than accidentally succeed
    fs::write(
        dir.path().join("main.by"),
        r#"import multiprocessing as mp

class Pair:
    init(let a: int, let b: int)

def main():
    ctx = mp.get_context("spawn")
    p = ctx.Process(target=print, args=("child ran",))
    p.start()
    p.join()
    print("exitcode", p.exitcode)
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("child ran") && stdout.contains("exitcode 0"),
        "the spawned child did not run:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// `by run <package>` runs the package's `__main__`, the way `python -m` does.
/// Running the package's own `__init__` instead executes the wrong file and
/// never reaches the program
#[test]
fn run_enters_a_package_through_its_main_module() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir(dir.path().join("app")).unwrap();
    fs::write(dir.path().join("app/__init__.by"), "print('init ran')\n").unwrap();
    fs::write(
        dir.path().join("app/__main__.by"),
        "print('package main ran')\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "app"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("package main ran"),
        "the package's `__main__` did not run:\n{stdout}"
    );
}

/// a frame is still keyed by the staged `.py` the code object came from, so
/// moving `__file__` must not stop the sourcemap finding it
#[test]
fn run_still_rewrites_traceback_frames_to_by_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("lib.by"),
        "def boom(items: list[int]) -> int:\n    return items[9]\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("main.by"),
        "import lib\n\ndef main():\n    print(lib.boom([1, 2]))\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("main.by\", line 4"),
        "no mapped entry frame:\n{stderr}"
    );
    assert!(
        stderr.contains("lib.by\", line 2"),
        "no mapped library frame:\n{stderr}"
    );
    assert!(
        stderr.contains("IndexError"),
        "the exception itself is missing:\n{stderr}"
    );
}

/// a frame inside a trailing-lambda block is reported at the block's own `.by`
/// line. the block's suite is hoisted into a `def` ahead of the call it hung
/// off, and the map used to charge every line of that `def` — header and body
/// alike — to the statement owning the block, so an exception raised in a
/// handler named the widget call instead of the line that raised
#[test]
fn run_reports_a_handler_blocks_own_line_for_an_exception_raised_in_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
def handle(fn: () -> None):
    fn()

def main():
    handle:
        print(\"handling\")
        raise ValueError(\"boom\")

main()
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(!output.status.success(), "expected a non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("main.by\", line 7, in _trailing_lambda_0")
            && stderr.contains("raise ValueError(\"boom\")"),
        "the raise inside the block should map to its own .by line 7:\n{stderr}"
    );
    assert!(
        stderr.contains("main.by\", line 2, in handle") && stderr.contains("fn()"),
        "the callee's frame should map to .by line 2:\n{stderr}"
    );
    assert!(
        stderr.contains("main.by\", line 5, in main") && stderr.contains("handle:"),
        "the statement owning the block should map to .by line 5:\n{stderr}"
    );
    assert!(
        stderr.contains("main.by\", line 9, in <module>"),
        "the module-level call should map to .by line 9:\n{stderr}"
    );
    assert!(
        !stderr.contains(".py\""),
        "traceback should not leak generated .py paths:\n{stderr}"
    );
    assert!(
        stderr.contains("ValueError: boom"),
        "exception type should be preserved:\n{stderr}"
    );
}

/// inference recurses with the shape of the expression it is checking, and `run`
/// checks on the thread it was dispatched to rather than through the rayon pool.
/// on the stack a process starts with — 1 MiB on windows — a file like this one
/// overflowed before that thread was sized for the work
#[test]
fn run_checks_a_deeply_nested_expression() {
    let dir = tempfile::tempdir().expect("tempdir");
    let terms = vec!["1"; 2000].join(" + ");
    fs::write(dir.path().join("main.by"), format!("print({terms})\n")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "2000");
}

#[test]
fn run_force_unwrap_yields_inner_value() {
    // `Some(x)` lowers to the `Optional(x)` wrapper; force-unwrapping it must
    // yield the inner value, not the wrapper object
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "x = Some(5)\nprint(x! + 1)\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "6");
}

#[test]
fn run_invokes_top_level_main() {
    // a top-level `def main` with no hand-written call still executes when the
    // module is run, via the synthesised `if __name__ == "__main__"` guard
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "def main():\n    print('ran main')\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ran main");
}

#[test]
fn run_invokes_async_main_via_asyncio() {
    // an `async def main` entry point is driven through `asyncio.run`
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "async def main():\n    print('ran async main')\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "ran async main"
    );
}

#[test]
fn run_uses_the_configured_entry_point() {
    // `run.main` names the module `by run` executes when none is given
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("ty.toml"), "[run]\nmain = \"app\"\n").unwrap();
    fs::write(dir.path().join("app.by"), "print('ran the entry point')\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("run")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "ran the entry point"
    );
}

#[test]
fn run_reads_the_entry_point_from_pyproject() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[tool.ty.run]\nmain = \"pkg.cli\"\n",
    )
    .unwrap();
    fs::create_dir(dir.path().join("pkg")).unwrap();
    fs::write(dir.path().join("pkg/__init__.by"), "").unwrap();
    fs::write(dir.path().join("pkg/cli.by"), "print('ran pkg.cli')\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("run")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "ran pkg.cli"
    );
}

#[test]
fn run_reads_the_entry_point_from_basedpython_toml() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("basedpython.toml"),
        "[run]\nmain = \"app\"\n",
    )
    .unwrap();
    fs::write(dir.path().join("app.by"), "print('ran the entry point')\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("run")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "ran the entry point"
    );
}

#[test]
fn run_reads_the_entry_point_from_the_basedpython_section() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[tool.basedpython.run]\nmain = \"pkg.cli\"\n",
    )
    .unwrap();
    fs::create_dir(dir.path().join("pkg")).unwrap();
    fs::write(dir.path().join("pkg/__init__.by"), "").unwrap();
    fs::write(dir.path().join("pkg/cli.by"), "print('ran pkg.cli')\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("run")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "ran pkg.cli"
    );
}

#[test]
fn run_prefers_an_explicit_module_over_the_configured_entry_point() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("ty.toml"), "[run]\nmain = \"app\"\n").unwrap();
    fs::write(dir.path().join("app.by"), "print('configured')\n").unwrap();
    fs::write(dir.path().join("other.by"), "print('explicit')\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "other"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "explicit");
}

#[test]
fn run_forwards_arguments_to_the_named_entry_point() {
    // arguments belong to the module, so reaching the configured entry point's
    // parameters means naming it: the first positional is always the module
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("ty.toml"), "[run]\nmain = \"app\"\n").unwrap();
    fs::write(
        dir.path().join("app.by"),
        "def main(name: str):\n    print(name)\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "app", "--name", "asdf"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "asdf");
}

#[test]
fn run_without_a_module_or_entry_point_reports_both_ways_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "print('unreached')\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("run")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no module given and no entry point configured"),
        "stderr:\n{stderr}"
    );
    assert!(stderr.contains("run.main"), "stderr:\n{stderr}");
}

/// write `source` as `main.by`, run it with `args`, and return
/// `(stdout, stderr, exit code)`
fn run_main_with_args(source: &str, args: &[&str]) -> (String, String, i32) {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), source).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .args(args)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    (
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        output.status.code().unwrap_or(-1),
    )
}

/// Build stamps are experimental, so a project that writes a `build:` block has
/// to ask for them by name — these tests are projects like any other.
const OPT_IN_TO_STAMPS: &str = "[experimental]\nbuild-stamps = true\n";

/// `--stamp` comes before the module: everything after the module name is the
/// program's own `sys.argv`
fn run_main_stamped(source: &str, stamps: &[&str]) -> (String, String, i32) {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), source).unwrap();
    fs::write(dir.path().join("basedpython.toml"), OPT_IN_TO_STAMPS).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_by"));
    command.env(EnvVars::BY_NO_PROJECT_SERVER, "1");
    command.arg("run");
    for stamp in stamps {
        command.args(["--stamp", stamp]);
    }
    let output = command
        .arg("main")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    (
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        output.status.code().unwrap_or(-1),
    )
}

#[test]
fn run_supplies_a_stamp_the_command_line_gave_it() {
    let source = r#"
build:
    GIT_SHA: str
    BUILD_NUMBER: int

def main():
    print(build.GIT_SHA, build.BUILD_NUMBER + 1)
"#;

    let (stdout, stderr, code) =
        run_main_stamped(source, &["GIT_SHA=deadbeef", "BUILD_NUMBER=416"]);
    assert_eq!(
        (stdout.as_str(), code),
        ("deadbeef 417", 0),
        "stderr:\n{stderr}"
    );
}

/// a stamp with no default is a claim the build has to satisfy, and nothing
/// supplies a name this one invented — so the transpile has to say so rather
/// than reach for a value
#[test]
fn run_refuses_a_required_stamp_nothing_supplied() {
    let source = r#"
build:
    RELEASE_CHANNEL: str

def main():
    print(build.RELEASE_CHANNEL)
"#;

    let (_, stderr, code) = run_main_stamped(source, &[]);
    assert_ne!(code, 0, "a stamp nothing supplied must not build");
    assert!(
        stderr.contains("supplied no value for the stamp `RELEASE_CHANNEL`"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn run_falls_back_to_a_stamp_default() {
    let source = r#"
build:
    RELEASE_CHANNEL: str = "dev"

def main():
    print(build.RELEASE_CHANNEL)
"#;

    let (stdout, stderr, code) = run_main_stamped(source, &[]);
    assert_eq!((stdout.as_str(), code), ("dev", 0), "stderr:\n{stderr}");
}

/// the python the output was lowered to is one of the values a build knows
/// without being told, and `by run` lowers for the interpreter it runs on
#[test]
fn run_discovers_the_python_it_lowered_for() {
    let source = r#"
import sys

build:
    PYTHON_VERSION: str

def main():
    print(build.PYTHON_VERSION == f"{sys.version_info[0]}.{sys.version_info[1]}")
"#;

    let (stdout, stderr, code) = run_main_stamped(source, &[]);
    assert_eq!((stdout.as_str(), code), ("True", 0), "stderr:\n{stderr}");
}

/// An interpreter that can host a native build, or `None` to skip.
///
/// `by compile` needs 3.11 or later, so on a host whose ambient python is older
/// the compiled leg cannot run at all — see `by_build::MINIMUM_PYTHON`.
fn native_interpreter() -> Option<String> {
    let candidates = std::env::var("PYTHON")
        .map(|python| vec![python])
        .unwrap_or_else(|_| {
            [
                "python3.14",
                "python3.13",
                "python3.12",
                "python3.11",
                "python3",
            ]
            .map(String::from)
            .to_vec()
        });
    candidates.into_iter().find(|python| {
        Command::new(python)
            .args([
                "-c",
                "import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)",
            ])
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// A stamp has to survive native compilation with its value intact.
///
/// The entry module always runs interpreted, so the block goes in an imported
/// module — the one that actually gets compiled. A stamp reaching the native
/// leg as an annotation with no value would leave `build.GIT_SHA` an
/// `AttributeError` that nothing else in this suite would catch.
#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn a_stamp_survives_native_compilation() {
    let Some(python) = native_interpreter() else {
        eprintln!("skipping: no interpreter new enough for a native build");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("stamps.by"),
        r#"
build:
    GIT_SHA: str
    GIT_DIRTY: bool

def describe() -> str:
    return build.GIT_SHA + ("-dirty" if build.GIT_DIRTY else "")
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("main.by"),
        "from stamps import describe

def main():
    print(describe())
",
    )
    .unwrap();
    fs::write(dir.path().join("basedpython.toml"), OPT_IN_TO_STAMPS).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "--compiled", "--python"])
        .arg(&python)
        .args([
            "--stamp",
            "GIT_SHA=abc123",
            "--stamp",
            "GIT_DIRTY=true",
            "main",
        ])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("C toolchain") || stderr.contains("no working C compiler") {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    assert!(
        output.status.success(),
        "by run --compiled failed:\n{stderr}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "abc123-dirty",
        "stderr:\n{stderr}"
    );
}

#[test]
fn a_stamp_that_is_not_a_pair_is_refused() {
    let (_, stderr, code) = run_main_stamped("def main(): ...\n", &["GIT_SHA"]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains("is not a `NAME=VALUE` pair"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn run_fills_main_parameter_positionally_or_by_name() {
    let source = "def main(name: str):\n    print(name)\n";

    let (stdout, stderr, code) = run_main_with_args(source, &["asdf"]);
    assert_eq!((stdout.as_str(), code), ("asdf", 0), "stderr:\n{stderr}");

    let (stdout, stderr, code) = run_main_with_args(source, &["--name", "asdf"]);
    assert_eq!((stdout.as_str(), code), ("asdf", 0), "stderr:\n{stderr}");
}

#[test]
fn run_converts_arguments_to_the_annotated_type() {
    let source = "from pathlib import Path\n\
                  def main(count: int, ratio: float, out: Path):\n\
                  \x20   print(count + 1, ratio * 2, out.name)\n";

    let (stdout, stderr, code) = run_main_with_args(source, &["2", "1.5", "/tmp/x.txt"]);
    assert_eq!(
        (stdout.as_str(), code),
        ("3 3.0 x.txt", 0),
        "stderr:\n{stderr}"
    );
}

#[test]
fn run_rejects_an_argument_the_annotation_cannot_convert() {
    let source = "def main(count: int):\n    print(count)\n";

    let (_, stderr, code) = run_main_with_args(source, &["nope"]);
    assert_eq!(code, 2, "stderr:\n{stderr}");
    assert!(
        stderr.contains("invalid int value: 'nope'"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn run_reports_a_missing_required_argument() {
    let source = "def main(name: str):\n    print(name)\n";

    let (_, stderr, code) = run_main_with_args(source, &[]);
    assert_eq!(code, 2, "stderr:\n{stderr}");
    assert!(
        stderr.contains("the following arguments are required: name"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn run_treats_a_bool_parameter_as_a_flag() {
    // a `bool` never takes a positional slot: it is set by `--name` /
    // `--no-name`, so the value token still binds to the next real parameter
    let source = "def main(name: str, verbose: bool = False):\n    print(name, verbose)\n";

    let (stdout, stderr, code) = run_main_with_args(source, &["bob"]);
    assert_eq!(
        (stdout.as_str(), code),
        ("bob False", 0),
        "stderr:\n{stderr}"
    );

    let (stdout, stderr, code) = run_main_with_args(source, &["bob", "--verbose"]);
    assert_eq!(
        (stdout.as_str(), code),
        ("bob True", 0),
        "stderr:\n{stderr}"
    );

    let source = "def main(name: str, verbose: bool = True):\n    print(name, verbose)\n";
    let (stdout, stderr, code) = run_main_with_args(source, &["bob", "--no-verbose"]);
    assert_eq!(
        (stdout.as_str(), code),
        ("bob False", 0),
        "stderr:\n{stderr}"
    );
}

#[test]
fn run_fills_positional_only_and_keyword_only_parameters() {
    // a positional-only parameter must be passed positionally even when the
    // command line named it, and a keyword-only one only ever by name
    let source = "async def main(a: str, /, b: int = 1, *, c: str = \"z\"):\n\
                  \x20   print(a, b, c)\n";

    let (stdout, stderr, code) = run_main_with_args(source, &["x", "7", "--c", "q"]);
    assert_eq!((stdout.as_str(), code), ("x 7 q", 0), "stderr:\n{stderr}");

    let (stdout, stderr, code) = run_main_with_args(source, &["--a", "x"]);
    assert_eq!((stdout.as_str(), code), ("x 1 z", 0), "stderr:\n{stderr}");
}

#[test]
fn run_rejects_an_argument_given_twice() {
    let source = "def main(name: str):\n    print(name)\n";

    let (_, stderr, code) = run_main_with_args(source, &["bob", "--name", "jim"]);
    assert_eq!(code, 2, "stderr:\n{stderr}");
    assert!(
        stderr.contains("given both positionally and as an option"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn run_forwards_argv_to_a_hand_written_entry_point() {
    // the arguments reach the program itself, not just a synthesised guard
    let source = "import sys\nprint(sys.argv[1:])\n";

    let (stdout, stderr, code) = run_main_with_args(source, &["a", "--b"]);
    assert_eq!(
        (stdout.as_str(), code),
        ("['a', '--b']", 0),
        "stderr:\n{stderr}"
    );
}

#[test]
fn run_applies_transforms() {
    // Sanity check: tuple subscripts pass through unchanged after the
    // forward subscript-normalization transform was shelved. __getitem__
    // receives the tuple key directly, matching Python semantics.
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
class Grid:
    def __getitem__(self, key):
        row, col = key
        print(row, col)

Grid()[(1, 2)]
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1 2");
}

#[test]
fn sealed_class_exposes_members_at_runtime() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
sealed class A
class B(A)
class C(A)

print(A.__sealed_members__ == (B, C))
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "True");
}

#[test]
fn context_parameters_pass_implicitly_at_runtime() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
def greet(name: str, context greeting: str) -> str:
    return f\"{greeting}, {name}\"

def shout(name: str, context greeting: str) -> str:
    return greet(name).upper()

context g = \"hello\"
print(greet(\"world\"))
print(shout(\"moon\", greeting=\"good night\"))
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "hello, world\nGOOD NIGHT, MOON"
    );
}

#[test]
fn extension_methods_run_and_track_element_types() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
extension list:
    def second(self) -> Element:
        return self[1]

extension str:
    @property
    def shouty(self) -> str:
        return self.upper()

xs = [1, 2, 3]
print(xs.second())
print(\"quiet\".shouty)
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "2\nQUIET"
    );
}

/// an extension may supply an operator's dunder; the checker accepts the
/// operator and the lowering emits the backing-function call it resolved to
#[test]
fn extension_operators_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
class Money:
    def __init__(self, cents: int):
        self.cents = cents

extension Money:
    def __add__(self, other: Money) -> Money:
        return Money(self.cents + other.cents)

    def __neg__(self) -> Money:
        return Money(-self.cents)

    def __lt__(self, other: Money) -> bool:
        return self.cents < other.cents

class Wallet:
    def __init__(self, held: list[int]):
        self.held = held

extension Wallet:
    def __contains__(self, m: Money) -> bool:
        return m.cents in self.held

a = Money(5)
b = Money(7)
print((a + b).cents)
print((-a).cents)
print(a < b)
w = Wallet([5])
print(a in w)
print(b not in w)
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "12\n-5\nTrue\nTrue\nTrue"
    );
}

#[test]
fn static_property_reads_off_the_class_and_the_instance() {
    // a `static let` accessor block is a class-level computed property: in a plain
    // class it lowers to a descriptor, in an extension to a backing call taking the
    // class. both spellings have to answer on a class *and* an instance receiver
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
class Config:
    static let default_name: str
        get() = \"config\"

class Widget: ...

extension Widget:
    static let kind: str
        get() = \"widget\"

print(Config.default_name)
print(Config().default_name)
print(Widget.kind)
print(Widget().kind)
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "config\nconfig\nwidget\nwidget"
    );
}

#[test]
fn extension_member_called_before_its_block_runs() {
    // the backing function is hoisted above the call, so a member used before
    // the `extension` block's source position resolves at runtime
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
print(\"asdf\".shout())

extension str:
    def shout(self) -> str:
        return self.upper() + \"!\"
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ASDF!");
}

#[test]
fn extension_member_called_in_a_class_body_runs() {
    // python private-name-mangles any `__name` reference inside a class body,
    // so the backing function carries a single leading underscore
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
extension list:
    def second(self) -> Element:
        return self[1]

class Holder:
    value: int = [1, 2, 3].second()

print(Holder.value)
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "2");
}

#[test]
fn imported_extension_runs_across_modules() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("ext.by"),
        "\
extension list:
    def second(self) -> Element:
        return self[1]
",
    )
    .unwrap();
    fs::write(
        dir.path().join("main.by"),
        "\
import ext

xs = [\"a\", \"b\", \"c\"]
print(xs.second())
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "b");
}

#[test]
fn enum_lowers_to_sealed_dataclass_hierarchy() {
    let out = transpile(
        "\
enum class Shape:
    case Circle(radius: int)
    case Point

    def kind(self) -> str:
        return type(self).__name__
",
    );
    assert!(out.contains("class Shape:"), "got:\n{out}");
    // variants are module-level subclasses of the enum, attached back as
    // `Shape.Circle` / `Shape.Point` (the unit variant as its singleton value)
    assert!(out.contains("class _Shape_Circle(Shape):"), "got:\n{out}");
    assert!(out.contains("class _Shape_Point(Shape):"), "got:\n{out}");
    assert!(out.contains("Shape.Circle = _Shape_Circle"), "got:\n{out}");
    assert!(out.contains("Shape.Point = _Shape_Point()"), "got:\n{out}");
    // unit variants get a derived repr (the bare name), not the default object repr
    assert!(
        out.contains("def __repr__(self): return \"Point\""),
        "unit variant should have a derived __repr__\n{out}"
    );
}

#[test]
fn enum_bounded_generic_lowers_type_args_not_declaration() {
    // a bounded generic enum must not leak the declaration text
    // `[T in (int, str)]` (invalid python) into the output; on the
    // 3.10 polyfill path the params become constrained `TypeVar`s and the
    // variant field annotations are renamed to match
    let out = transpile(
        "\
enum class Box[T in (int, str)]:
    case Full(T)
    case Empty
",
    );
    assert!(
        !out.contains("T in (int, str)"),
        "type mapping leaked into output\n{out}"
    );
    assert!(
        out.contains("class _Box_Full(Box):"),
        "variant should subclass the enum\n{out}"
    );
    assert!(
        out.contains("_0: _T"),
        "variant field should use the mangled typevar\n{out}"
    );
}

#[test]
fn enum_all_unit_runs_as_python_enum() {
    // an all-unit enum lowers to `enum.Enum` + `auto()`, which runs on any
    // supported Python (no match/union syntax involved)
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
enum class Color:
    case Red, Green, Blue

print(Color.Green.name)
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "Green");
}

#[test]
fn run_traceback_rewritten_to_by_source() {
    // a runtime error must surface a traceback in `.by` coordinates: the
    // original file path, the original line numbers, and the original surface
    // syntax (here the `int & str` intersection, not its transpiled form)
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
def deeper(n: int) -> int:
    x: int & object = compute(n)
    return x

def compute(n: int) -> int:
    return n // 0  # ty: ignore[division-by-zero]

def main() -> None:
    deeper(5)

main()
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(!output.status.success(), "expected a non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // frames point at main.by, never at a generated .py in the build dir
    assert!(
        stderr.contains("main.by"),
        "traceback should reference the .by file:\n{stderr}"
    );
    assert!(
        !stderr.contains(".py\""),
        "traceback should not leak generated .py paths:\n{stderr}"
    );
    // correct line + original surface syntax for the failing call site
    assert!(
        stderr.contains("line 6, in compute") && stderr.contains("return n // 0"),
        "compute frame should map to .by line 6:\n{stderr}"
    );
    assert!(
        stderr.contains("line 2, in deeper") && stderr.contains("x: int & object = compute(n)"),
        "deeper frame should show the original intersection syntax at .by line 2:\n{stderr}"
    );
    assert!(
        stderr.contains("ZeroDivisionError"),
        "exception type should be preserved:\n{stderr}"
    );
}

#[test]
fn run_sourcemap_digests_match_the_files_they_describe() {
    // `SOURCEMAP` describes a pair of files, and a consumer that reports a `.by`
    // line from it is trusting that both are still the ones it was built from.
    // `DIGESTS` is what makes that checkable, so it has to be over the bytes
    // actually read and written — the debuggee recomputes both from disk here,
    // then edits its own source to prove the digest discriminates at all
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
import hashlib
import importlib

# imported dynamically: it is generated into the build dir, so there is nothing
# for the checker to resolve at the time this file is checked
sourcemap = importlib.import_module('_by_sourcemap')


def digest_of(path: str) -> str:
    with open(path, 'rb') as handle:
        return 'sha256:' + hashlib.sha256(handle.read()).hexdigest()


checked = 0
for py_path, entry in sourcemap.SOURCEMAP.items():
    by_path = entry[0]
    digests = sourcemap.DIGESTS[py_path]
    assert digest_of(by_path) == digests['by'], 'stale .by digest for ' + by_path
    assert digest_of(py_path) == digests['py'], 'stale .py digest for ' + py_path
    for path, side in ((by_path, 'by'), (py_path, 'py')):
        with open(path, 'ab') as handle:
            handle.write(b'# edited after the transpile\\n')
        assert digest_of(path) != digests[side], 'an edited file still matched its digest'
    checked += 1

print('verified', checked)
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by run failed:\n{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "verified 1",
        "every sourcemap entry should have been checked:\n{stderr}"
    );
}

#[test]
fn run_leaves_a_frame_generated_when_its_source_no_longer_matches() {
    // the traceback shim is the digests' first consumer: once the `.by` has been
    // saved over, its line table describes the file that was replaced, so a
    // mapped frame would quote the *new* text at the *old* line numbers. it has
    // to refuse the mapping and say why, rather than answer confidently wrong
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
import importlib

sourcemap = importlib.import_module('_by_sourcemap')


def boom() -> None:
    raise ValueError('bang')


# stand a different file in the place of every source the map describes
for entry in sourcemap.SOURCEMAP.values():
    with open(entry[0], 'w') as handle:
        handle.write('# not the file that was transpiled\\n')

boom()
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(!output.status.success(), "expected a non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("no longer matches what it was transpiled from"),
        "the refusal should be reported, not silent:\n{stderr}"
    );
    assert!(
        !stderr.contains("main.by\", line"),
        "no frame may claim a line in a .by the map no longer describes:\n{stderr}"
    );
    assert!(
        stderr.contains("main.py\", line"),
        "frames should fall back to the generated python:\n{stderr}"
    );
    assert!(
        stderr.contains("ValueError"),
        "exception type should be preserved:\n{stderr}"
    );
}

#[test]
fn build_skips_a_source_it_cannot_read() {
    // a source ty cannot decode reads as an empty module, so emitting for it
    // would write an empty `.py` over the real one. it is reported and skipped
    // instead, and every other module still builds
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("good.by"), "x = 1\n").unwrap();
    // PEP 263: a declared latin-1 encoding, and a byte no utf-8 decoder accepts
    fs::write(
        dir.path().join("bad.by"),
        b"# -*- coding: latin-1 -*-\ns = '\xdf'\n".as_slice(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["build", "--min-version", "3.12"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a module that did not build must not report success:\n{stderr}"
    );
    assert!(
        stderr.contains("valid UTF-8"),
        "the skipped file must be reported:\n{stderr}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("build/good.py")).unwrap(),
        "x = 1\n"
    );
    assert!(
        !dir.path().join("build/bad.py").exists(),
        "an unreadable source must not be emitted as an empty module"
    );
}

#[test]
fn parenthesized_tuple_subscript_unchanged() {
    // Subscript normalization is shelved: tuple keys pass through verbatim.
    assert_eq!(transpile("x = {}\nx[(a, b)]\n"), "x = {}\nx[(a, b)]\n");
}

#[test]
fn bare_tuple_subscript_unchanged() {
    assert_eq!(transpile("x = {}\nx[a, b]\n"), "x = {}\nx[a, b]\n");
}

#[test]
fn empty_tuple_subscript_unchanged() {
    // Critical edge: `x[()]` must keep its empty-tuple key intact.
    assert_eq!(transpile("x = {}\nx[()]\n"), "x = {}\nx[()]\n");
}

#[test]
fn single_element_tuple_subscript_unchanged() {
    // `x[(a,)]` and `x[a,]` are author-explicit 1-tuple keys; never re-wrap.
    assert_eq!(transpile("x = {}\nx[(a,)]\n"), "x = {}\nx[(a,)]\n");
    assert_eq!(transpile("x = {}\nx[a,]\n"), "x = {}\nx[a,]\n");
}

#[test]
fn scalar_subscript_unchanged() {
    assert_eq!(transpile("x = {}\nx[a]\n"), "x = {}\nx[a]\n");
}

#[test]
fn subscript_in_function_unchanged() {
    let src = "d = {}\ndef foo():\n    return d[(x, y)]\n";
    assert_eq!(transpile(src), src);
}

#[test]
fn multiple_subscripts_unchanged() {
    let src = "a = {}\nb = {}\nc = {}\na[(1, 2)]\nb[(3, 4)]\nc[x]\n";
    assert_eq!(transpile(src), src);
}

#[test]
fn comments_and_unrelated_code_preserved() {
    let src = "# a comment\nx = 1\ny = {}\ny[(a, b)]\n";
    assert_eq!(transpile(src), src);
}

#[test]
fn reverse_empty_class() {
    assert_eq!(reverse_transpile("class A: ...\n"), "class A\n");
}

#[test]
fn export_generates_dunder_all() {
    let src = "export def api(): ...\nprivate def helper(): ...\ndef internal(): ...\n";
    let out = "def api(): ...\ndef _helper(): ...\ndef internal(): ...\n__all__ = [\"api\"]\n";
    assert_eq!(transpile(src), out);
}

#[test]
fn reverse_literal_union() {
    assert_eq!(reverse_transpile("a: Literal[1, 2]\n"), "a: 1 | 2\n",);
}

#[test]
fn reverse_paren_tuple_in_type_subscript() {
    assert_eq!(
        reverse_transpile("a: dict[(int, str)]\n"),
        "a: dict[int, str]\n",
    );
}

#[test]
fn transpile_renders_parse_error_with_location() {
    // file-based transpile should surface ty-style diagnostics on invalid
    // input rather than the opaque "transpiled output has invalid syntax"
    let dir = tempfile::tempdir().expect("tempdir");
    let by_path = dir.path().join("broken.by");
    fs::write(&by_path, "a b\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("transpile")
        .arg(&by_path)
        .output()
        .expect("failed to spawn by");

    assert!(
        !output.status.success(),
        "expected non-zero exit on bad input"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid-syntax"),
        "stderr should include invalid-syntax diagnostic:\n{stderr}"
    );
    assert!(
        stderr.contains("broken.by"),
        "stderr should include file path:\n{stderr}"
    );
    assert!(
        stderr.contains("Found 3 diagnostics"),
        "stderr should include diagnostic count footer:\n{stderr}"
    );
    assert!(
        !stderr.contains("transpile failed"),
        "stderr should not include legacy opaque message:\n{stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.is_empty(),
        "stdout should be empty when transpile aborts:\n{stdout}"
    );
}

#[test]
fn transpile_malformed_inputs_never_panic() {
    // adversarial / truncated basedpython snippets must produce a clean
    // outcome (a diagnostic or valid output), never a Rust panic
    let inputs = [
        "x: (",
        "a ?? ",
        "() -> ",
        "class A[",
        "a: int &",
        "lazy",
        "def f[T:",
        "x: (name:",
        "@kw",
        "typeof",
        "(a: int, b: int) ->",
        "x: list[(name: str,",
        "def f() ->",
        "a: int & str &",
        "match",
    ];
    for src in inputs {
        let mut child = Command::new(env!("CARGO_BIN_EXE_by"))
            .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
            .arg("transpile")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn by");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(src.as_bytes())
            .unwrap();
        let output = child.wait_with_output().expect("by did not exit");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("panicked") && !stderr.contains("RUST_BACKTRACE"),
            "transpiler panicked on malformed input {src:?}:\n{stderr}"
        );
        // a panic is signalled by exit code 101; anything else is a clean
        // diagnostic (failure) or successful transpile
        assert_ne!(
            output.status.code(),
            Some(101),
            "transpiler aborted (panic) on malformed input {src:?}"
        );
    }
}

#[test]
fn run_renders_parse_error_and_aborts() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "a b\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(!output.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid-syntax"),
        "stderr should include invalid-syntax diagnostic:\n{stderr}"
    );
    assert!(
        stderr.contains("main.by"),
        "stderr should reference the offending file:\n{stderr}"
    );
    assert!(
        stderr.contains("Found 3 diagnostics"),
        "stderr should include diagnostic count footer:\n{stderr}"
    );
}

#[test]
fn build_renders_parse_error_and_aborts() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("bad.by"), "a b\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(!output.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid-syntax"),
        "stderr should include invalid-syntax diagnostic:\n{stderr}"
    );
    assert!(
        !dir.path().join("build").join("bad.py").exists(),
        "build should not emit output when parse error present"
    );
}

#[test]
fn run_refuses_to_execute_on_check_errors() {
    // a program that fails `by check` must never execute — here `T` cannot be
    // inferred from the `object`-typed argument, so the bare call is a check
    // error; it previously slipped through to a runtime TypeError from the
    // `generic` wrapper
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "def f[T](t: object):\n    print(T)\n\nf(1)\nf(\"\")\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(!output.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stderr.contains("unspecialized-reified-generic"),
        "stderr should carry the check error:\n{stderr}"
    );
    assert!(
        !stderr.contains("Traceback") && !stdout.contains("Traceback"),
        "the program must not have executed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn run_min_version_newer_than_interpreter_errors() {
    // an explicit --min-version above the interpreter's version would emit
    // code the interpreter cannot parse; `run` must refuse with a clear error
    let Some(python) = ["python3.13", "python3"].into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }) else {
        eprintln!("skipping: no python interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "print(1)\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "--min-version", "3.99", "main"])
        .env("PYTHON", python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(!output.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is newer than") && stderr.contains("3.99"),
        "stderr should explain the version conflict:\n{stderr}"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn run_honors_explicit_min_version() {
    // the flag used to be silently overridden by the interpreter probe
    let Some(python) = ["python3.13", "python3"].into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }) else {
        eprintln!("skipping: no python interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "print('versioned')\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "--min-version", "3.9", "main"])
        .env("PYTHON", python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "versioned");
}

#[test]
fn build_skips_hidden_directories() {
    // files under hidden directories (`.claude`, `.git`, …) are not project
    // sources: they must be neither checked nor emitted
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "print(1)\n").unwrap();
    let hidden = dir.path().join(".claude").join("worktrees").join("x");
    fs::create_dir_all(&hidden).unwrap();
    fs::write(hidden.join("junk.by"), "a b\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    assert!(
        !stderr.contains("junk"),
        "hidden-directory file must not be checked:\n{stderr}"
    );
    assert!(dir.path().join("build").join("main.py").exists());
    assert!(
        !dir.path().join("build").join(".claude").exists(),
        "hidden-directory file must not be emitted"
    );
}

/// what a project exports is not in its `pyproject.toml` as far as its users are
/// concerned — nothing installs one — so the build writes it into the package
#[test]
fn build_writes_what_the_project_exports_into_its_marker() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"my-lib\"\nversion = \"0.1.0\"\n\
         \n[tool.basedpython.analysis]\nexported-dependencies = [\"numpy\"]\n",
    )
    .unwrap();
    let package = dir.path().join("my_lib");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("__init__.by"), "").unwrap();
    fs::write(
        package.join("frames.by"),
        "def frame() -> int:\n    return 1\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    assert_eq!(
        fs::read_to_string(dir.path().join("build").join("my_lib").join("by.typed")).unwrap(),
        "exported-dependencies = [\"numpy\"]\n"
    );
}

/// a package the build emitted is marked as basedpython's even when the project
/// exports nothing: the file's presence is what marks it
#[test]
fn build_writes_a_marker_for_a_project_that_exports_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let package = dir.path().join("my_lib");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("__init__.by"), "").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    let marker = dir.path().join("build").join("my_lib").join("by.typed");
    assert!(marker.exists(), "expected build/my_lib/by.typed:\n{stderr}");
    assert_eq!(fs::read_to_string(marker).unwrap(), "");
}

/// a src-layout project's `src/pkg/main.by` is the module `pkg.main`, so the
/// emitted tree has to be rooted at `src` — mirroring the directory instead
/// emits `build/src/pkg/main.py`, whose module is `src.pkg.main`, a name nothing
/// imports and `run.main` cannot sensibly be set to
#[test]
fn build_mirrors_the_module_tree_not_the_directory_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"package-name\"\nversion = \"0.1.0\"\n\
         \n[tool.basedpython.run]\nmain = \"package_name.main\"\n",
    )
    .unwrap();
    let package = dir.path().join("src").join("package_name");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("__init__.by"), "").unwrap();
    fs::write(package.join("main.by"), "print(\"src layout\")\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    let out = dir.path().join("build");
    assert!(
        out.join("package_name").join("main.py").exists(),
        "expected build/package_name/main.py:\n{stderr}"
    );
    assert!(
        !out.join("src").exists(),
        "the source root must not appear in the output tree:\n{stderr}"
    );
}

/// `build/` outlives the build that wrote it — a test runner, a debugger or an
/// editor reads it later — so it is the tree where a `.by` really can be saved
/// after the transpile, and the one that needs the digests to say so
#[test]
fn build_writes_a_sourcemap_beside_the_generated_python() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "print(\"built\")\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    let out = dir.path().join("build");
    let map = fs::read_to_string(out.join("_by_sourcemap.py")).expect("sourcemap module");

    // read the keys out of the file rather than rebuilding them: the build
    // spells a path the way the system handed it over, which is neither the
    // test's `dir.path()` (a symlink under `/tmp` on macOS) nor its canonical
    // form (a `\\?\` path with the long directory name on windows)
    let first_key_of = |table: &str| {
        let (_, body) = map
            .split_once(&format!("{table} = {{\n"))
            .unwrap_or_else(|| panic!("no {table} table:\n{map}"));
        let entry = body.lines().next().expect("an entry");
        entry
            .trim()
            .split_once(": ")
            .unwrap_or_else(|| panic!("no key in {table}:\n{map}"))
            .0
            .to_owned()
    };

    let mapped = first_key_of("SOURCEMAP");
    assert!(
        mapped.ends_with("main.py\""),
        "the generated module should be mapped by its own path:\n{map}"
    );
    assert_eq!(
        mapped,
        first_key_of("DIGESTS"),
        "both tables key the same generated file:\n{map}"
    );
    assert!(
        map.contains(&format!("{mapped}: {{\"by\": \"sha256:")),
        "the entry should carry a digest of each side:\n{map}"
    );
    // the runner shim belongs to `by run`; a build output is not an entry point
    assert!(
        !out.join("_by_runner.py").exists(),
        "the runner shim should not be written into a build output"
    );
}

/// Build `source` on its own and return its one `SOURCEMAP` line table, plus the
/// generated python it describes.
fn sourcemap_table_for(source: &str) -> (Vec<Option<u32>>, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), source).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");
    assert!(
        output.status.success(),
        "by build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let out = dir.path().join("build");
    let map = fs::read_to_string(out.join("_by_sourcemap.py")).expect("sourcemap module");
    let generated = fs::read_to_string(out.join("main.py")).expect("generated module");

    // the one entry's list, read out of the rendered table rather than rebuilt:
    // this is the text a debugger imports, so it is the text worth asserting on
    let (_, body) = map
        .split_once("SOURCEMAP = {\n")
        .expect("a SOURCEMAP table");
    let entry = body.lines().next().expect("an entry");
    let list = entry
        .split_once('[')
        .and_then(|(_, rest)| rest.split_once(']'))
        .unwrap_or_else(|| panic!("no line table in {entry}:\n{map}"))
        .0;
    let table = list
        .split(", ")
        .map(|item| match item {
            "None" => None,
            n => Some(n.parse::<u32>().unwrap_or_else(|e| panic!("{n}: {e}"))),
        })
        .collect();
    (table, generated)
}

/// **The last line of a file that ends without a newline is still a line.**
///
/// A debugger binds a `.by` breakpoint by looking the line up in this table, and
/// `None` there means "prelude, no `.by` line is behind this". The table used to
/// be built one entry per `\n`, so a file whose last line had no terminator lost
/// that line's entry and the entry-point epilogue's `None`s slid up over it — the
/// user's own last line reported as generated prelude, and a breakpoint on it
/// refused and silently never hit.
///
/// Asserted against the rendered `_by_sourcemap.py` rather than the library that
/// wrote it, because what a debugger reads is the file.
#[test]
fn a_source_without_a_trailing_newline_still_maps_its_last_line() {
    let body = "def helper() -> int:\n    return 1\n\ndef main():\n    print(helper())";
    let (without, generated) = sourcemap_table_for(body);
    let (with_newline, _) = sourcemap_table_for(&format!("{body}\n"));

    assert_eq!(
        without, with_newline,
        "the terminator closes the last line, it does not add one"
    );
    assert_eq!(
        without.len(),
        generated.lines().count(),
        "one entry per generated line:\n{generated}"
    );

    // `    print(helper())` is `.by` line 4, and it is the last line that has a
    // `.by` line behind it at all
    let printed = generated
        .lines()
        .position(|line| line.contains("print(helper())"))
        .expect("the statement is in the output");
    assert_eq!(
        without[printed],
        Some(4),
        "the user's last line maps to itself:\n{without:?}\n{generated}"
    );

    // and the entry-point guard `by` appended is prelude, which is what `None`
    // is for — the fix must not buy the last line back by mapping those
    let epilogue = generated
        .lines()
        .position(|line| line.starts_with("if __name__ =="))
        .expect("the entry-point guard is appended");
    assert!(
        without[epilogue..].iter().all(Option::is_none),
        "the appended guard has no `.by` line behind it:\n{without:?}\n{generated}"
    );
    assert!(
        epilogue > printed,
        "the guard comes after the user's code:\n{generated}"
    );
}

/// the counterpart at run time: `run.main` names the module, and the temporary
/// tree `by run` executes has to be rooted the same way
#[test]
fn run_resolves_a_src_layout_entry_point() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"package-name\"\nversion = \"0.1.0\"\n\
         \n[tool.basedpython.run]\nmain = \"package_name.main\"\n",
    )
    .unwrap();
    let package = dir.path().join("src").join("package_name");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("__init__.by"), "").unwrap();
    fs::write(package.join("main.by"), "print(\"src layout\")\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("run")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "src layout");
}

/// the emit target defaults to the version the project configures, so the two
/// halves of the toolchain agree about which python this project targets — a
/// 3.13 project was getting `typing_extensions` shims it does not need and
/// cannot import
#[test]
fn build_targets_the_configured_python_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\nrequires-python = \">=3.13\"\n",
    )
    .unwrap();
    fs::write(dir.path().join("main.by"), "type X = int\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    let emitted = fs::read_to_string(dir.path().join("build/main.py")).unwrap();
    assert!(
        !emitted.contains("typing_extensions"),
        "a 3.13 target needs no shim:\n{emitted}"
    );
    assert!(emitted.contains("type X = int"), "got:\n{emitted}");
}

/// a build is not all-or-nothing: a file mid-edit must not take down the build
/// of every unrelated module, which is exactly when a code generator or a test
/// runner is reached for
#[test]
fn build_emits_every_file_it_can_past_a_broken_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("good.by"), "print(1)\n").unwrap();
    fs::write(dir.path().join("broken.by"), "x = (\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        dir.path().join("build/good.py").exists(),
        "the parseable file must still be emitted:\n{stderr}"
    );
    assert!(
        !output.status.success(),
        "the broken file still fails the build:\n{stderr}"
    );
    assert!(stderr.contains("broken.by"), "got:\n{stderr}");
}

/// `by build` walks the project's own file set, so `src.exclude` applies to it
/// exactly as it does to `by check`
#[test]
fn build_honours_src_exclude() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\
         \n[tool.basedpython.src]\nexclude = [\"tests/negative\"]\n",
    )
    .unwrap();
    fs::write(dir.path().join("main.by"), "print(1)\n").unwrap();
    let negative = dir.path().join("tests").join("negative");
    fs::create_dir_all(&negative).unwrap();
    fs::write(negative.join("bad.by"), "def f(:\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    assert!(
        !stderr.contains("bad.by"),
        "excluded file checked:\n{stderr}"
    );
    assert!(dir.path().join("build/main.py").exists());
    assert!(!dir.path().join("build/tests").exists());
}

#[test]
fn transpile_proceeds_past_non_syntax_errors() {
    // type errors are surfaced as diagnostics but don't block transpile —
    // many basedpython type forms look like type errors to ty
    let dir = tempfile::tempdir().expect("tempdir");
    let by_path = dir.path().join("typed.by");
    fs::write(&by_path, "x: int = \"string\"\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("transpile")
        .arg(&by_path)
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "expected success despite type error:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid-assignment"),
        "stderr should include type-error diagnostic:\n{stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let body = stdout
        .strip_prefix("from __future__ import annotations\n")
        .unwrap_or(&stdout);
    assert_eq!(body.trim(), "x: int = \"string\"");
}

#[test]
fn transpile_directory_reverses_in_place() {
    // `by transpile --reverse <dir>` converts every `.py` under the tree into a
    // `.by` in place, deleting the original; venv/cache dirs are skipped
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(
        root.join("pkg/models.py"),
        "def find(x: int | None) -> int:\n    return x if x is not None else 0\n",
    )
    .unwrap();
    // a file inside a skipped directory must be left untouched
    fs::create_dir_all(root.join(".venv")).unwrap();
    fs::write(root.join(".venv/dep.py"), "x = 1\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("transpile")
        .arg("--reverse")
        .arg(root)
        .output()
        .expect("failed to spawn by");
    assert!(
        output.status.success(),
        "reverse dir failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!root.join("pkg/models.py").exists(), "original .py removed");
    let reversed = fs::read_to_string(root.join("pkg/models.by")).unwrap();
    assert!(
        reversed.contains("?? 0"),
        "coalesce reversed to basedpython form:\n{reversed}"
    );
    // skipped-dir file is left as-is
    assert!(root.join(".venv/dep.py").exists());
    assert!(!root.join(".venv/dep.by").exists());
}

#[test]
fn transpile_directory_round_trips_through_build() {
    // reverse a whole project, then `by build` it back: the forward pass uses
    // one shared project db, so the cross-module form round-trips
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(root.join("pkg/__init__.by"), "").unwrap();
    fs::write(
        root.join("pkg/models.by"),
        "def find(x: int | None) -> int:\n    return x ?? 0\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(root)
        .output()
        .expect("failed to spawn by");
    assert!(
        output.status.success(),
        "build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let built = fs::read_to_string(root.join("build/pkg/models.py")).unwrap();
    assert!(
        built.contains("x if x is not None else 0"),
        "coalesce lowered back to python:\n{built}"
    );
}

/// An interpreter new enough to run a reified class: the specializer reads the
/// class's own `__type_params__`, which needs the native PEP 695 header.
fn python_at_least_312() -> Option<String> {
    // `$PYTHON` is what pins the interpreter for every python-executing test,
    // so an explicit one is the answer whether or not it is new enough — a run
    // that pinned an old interpreter should say so rather than quietly pick
    // another
    if let Ok(pinned) = std::env::var("PYTHON") {
        return Some(pinned);
    }
    ["python3.13", "python3.12", "python3"]
        .into_iter()
        .find(|python| {
            Command::new(python)
                .args(["-c", "import sys; sys.exit(sys.version_info < (3, 12))"])
                .output()
                .is_ok_and(|out| out.status.success())
        })
        .map(str::to_owned)
}

/// transpile with `--min-version 3.13` — reified generics require native PEP
/// 695 syntax in the output (the closure mechanism), available from 3.12+.
fn transpile_at_313(source: &str) -> String {
    run_transpile(source, &["--min-version", "3.13"])
}

#[test]
fn reified_generic_wraps_and_preserves_call_site() {
    // `T` in a value position reifies: the function is wrapped in `@generic`
    // and the specialized call site keeps its `[int]` (routes through the
    // wrapper) instead of being stripped like an erased generic
    let out = transpile_at_313(
        "\
def f[T](t: object):
    return isinstance(t, T)

f[int](1)
",
    );
    assert!(
        out.contains("@generic  # basedpython: reified"),
        "reified function should be wrapped:\n{out}"
    );
    assert!(
        out.contains("f[int](1)"),
        "reified call site must keep its type args:\n{out}"
    );
}

#[test]
fn reified_class_binds_its_type_argument_from_the_receiver() {
    // `T` in a value position reifies the class: it is decorated with
    // `@generic_class`, the method that reads `T` opens by binding it from its
    // receiver, and the construction names the specialization ty solved
    let out = transpile_at_313(
        "\
class A[T]:
    def f(self):
        print(T)

a: A[int] = A()
a.f()
",
    );
    assert!(
        out.contains("@generic_class  # basedpython: reified"),
        "reified class should be decorated:\n{out}"
    );
    assert!(
        out.contains("        T = _type_argument(self, \"T\")"),
        "the read should bind from the receiver:\n{out}"
    );
    assert!(
        out.contains("a: A[int] = A[int]()"),
        "the construction should name its specialization:\n{out}"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn reified_class_reads_its_type_argument_at_runtime() {
    // the whole contract end to end: an instance built from `A[int]` answers
    // `int` when its method reads the class's type parameter
    let Some(python) = python_at_least_312() else {
        eprintln!("skipping: no python 3.12+ interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
class A[T]:
    def f(self):
        print(T)

a: A[int] = A()
a.f()
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .env("PYTHON", &python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "<class 'int'>"
    );
}

#[test]
fn declared_reified_generic_wraps_a_body_that_never_reads_it() {
    // the `reified` modifier drives the whole pipeline on its own: the keyword
    // is stripped from the output, the function is wrapped, and the call site
    // keeps its type argument even though the body never reads `T`
    let out = transpile_at_313(
        "\
def f[reified T]():
    print(\"ok\")

f[int]()
",
    );
    assert!(
        out.contains("@generic  # basedpython: reified"),
        "declared reification should wrap the function:\n{out}"
    );
    assert!(
        out.contains("def f[T]():"),
        "the modifier has no python spelling and must be stripped:\n{out}"
    );
    assert!(
        out.contains("f[int]()"),
        "reified call site must keep its type args:\n{out}"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn declared_reified_generic_runs() {
    // the specialization step is a real runtime operation, so `f[int]()` only
    // works because the keyword put the wrapper there — a plain `def f[T]` is
    // not subscriptable
    let Some(python) = ["python3.13"].into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }) else {
        eprintln!("skipping: no python 3.13 interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "def f[reified T]():\n    print(\"ok\")\n\nf[int]()\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .env("PYTHON", python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn reified_generic_infers_specialization_from_arguments() {
    // bare calls of a reified generic reify through inference: the transpiler
    // injects the statically inferred type argument, so `1 is T` observes the
    // argument's class at runtime
    let Some(python) = ["python3.13"].into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }) else {
        eprintln!("skipping: no python 3.13 interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "def f[T](t: T):\n    print(1 is T)\n\nf(1)\nf(\"\")\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .env("PYTHON", python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "True\nFalse"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn reified_generic_runs_value_position() {
    // run only on a 3.13+ interpreter — the source's `def g[T = int]()` uses a
    // PEP 696 type-param default, which parses natively only from 3.13 (a 3.12
    // interpreter rejects it). probe `python3.13`; skip cleanly when it's absent
    let Some(python) = ["python3.13"].into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }) else {
        eprintln!("skipping: no python 3.13 interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
def f[T](t: object):
    print(T)
    return isinstance(t, T)

def g[T = int]():
    print(T)

class Box:
    def kind[T](self) -> object:
        print(T)
        return T

print(f[int](1))
print(f[str](1))
g()
g[bytes]()
Box().kind[float]()
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .env("PYTHON", python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "<class 'int'>\nTrue\n<class 'str'>\nFalse\n<class 'int'>\n<class 'bytes'>\n<class 'float'>"
    );
}

#[test]
fn type_reification_makes_specializations_explicit() {
    // a bare generic *constructor* call carries its inferred specialization in
    // the generated python (the instance stamps `__orig_class__`); builtin
    // collection literals are not reified — the wrap would be erased bloat
    let out = transpile_at_313(
        "\
class A[T]:
    def __init__(self, t: T):
        self.t = t

a = A(1)
xs = [1, 2]
d = {\"k\": 1}
",
    );
    assert!(
        out.contains("a = A[int](1)"),
        "constructor should reify:\n{out}"
    );
    assert!(out.contains("xs = [1, 2]"), "list stays bare:\n{out}");
    assert!(out.contains("d = {\"k\": 1}"), "dict stays bare:\n{out}");
    assert!(
        !out.contains("list[int]") && !out.contains("dict[str"),
        "no collection reification:\n{out}"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn type_reification_observable_at_runtime() {
    // `A[int](…)` routes through `GenericAlias.__call__`, which stamps
    // `__orig_class__` on the instance — the specialization becomes a runtime
    // value. wrapped collection literals construct identical values
    let Some(python) = ["python3.13"].into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }) else {
        eprintln!("skipping: no python 3.13 interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
class A[T]:
    def __init__(self, t: T):
        self.t = t

a = A(1)
print(getattr(a, \"__orig_class__\", None), a.t)
xs = [1, 2]
print(xs)
d = {\"k\": 1}
print(d)
t = 1, \"x\"
print(t)
s = {3}
print(sorted(s))
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .env("PYTHON", python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "__main__.A[int] 1\n[1, 2]\n{'k': 1}\n(1, 'x')\n[3]"
    );
}

#[test]
fn parametric_is_folds_and_lowers() {
    // a concrete value folds statically; a dynamic value against a builtin
    // erases to `False`, but against a user generic probes `__orig_class__`
    let out = transpile_at_313(
        "\
class A[T]:
    def __init__(self, t: T): ...

xs = [1, 2]
a = xs is list[int]
b = xs is list[str]

def f(x) -> bool:
    return x is list[int]

def p(x) -> bool:
    return x is A[int]
",
    );
    assert!(
        out.contains("a = True"),
        "concrete match folds true:\n{out}"
    );
    assert!(
        out.contains("b = False"),
        "concrete mismatch folds false:\n{out}"
    );
    assert!(
        out.contains("return False"),
        "dynamic value against an erased builtin folds false:\n{out}"
    );
    assert!(
        out.contains("return _parametric_is(x, A[int], ("),
        "dynamic value against a user generic probes __orig_class__:\n{out}"
    );
}

#[test]
fn parametric_is_builtin_target_probes_at_runtime() {
    // a builtin-specialization target is probed, not rejected: the runtime
    // unwinds the value's mro. `A(True)` is not a `list`, so `is list[bool]`
    // is `False`; its `__orig_class__` makes `is A[bool]` `True`. no
    // erased-type-check error
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
class A[T]:
    def __init__(self, t: T): ...

def x(a: object):
    print(a is list[bool])
    print(a is A[bool])

x(A(True))
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "a builtin target now probes rather than erroring:\n{stderr}"
    );
    assert!(
        !stderr.contains("erased-type-check"),
        "no erased-type-check for a builtin target:\n{stderr}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "False\nTrue"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn parametric_is_observable_at_runtime() {
    // a user-generic probe reads `__orig_class__` (stamped by `A[int](…)`); a
    // reified type parameter carries the exact specialization even against a
    // builtin target; a user-generic union is discriminated per arm by the
    // probe (an invariant field keeps the union from collapsing)
    let Some(python) = ["python3.13"].into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }) else {
        eprintln!("skipping: no python 3.13 interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
class A[T]:
    def __init__(self, t: T):
        self.v: list[T] = [t]

def probe(a: object) -> bool:
    return a is A[int]

print(probe(A(1)))
print(probe(A(\"x\")))
print(probe([1]))

def g[T](x: T) -> bool:
    return x is list[int]

print(g([1, 2]))
print(g(\"x\"))

def h(items: A[int] | A[str]) -> bool:
    return items is A[int]

print(h(A(1)))
print(h(A(\"x\")))
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .env("PYTHON", python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "True\nFalse\nFalse\nTrue\nFalse\nTrue\nFalse"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn parametric_is_respects_variance_at_runtime() {
    // `a is C[args]` means `type(a) <: C[args]`, so the runtime probe follows
    // the target's variance: a covariant `A[int]` is an `A[object]`, an
    // invariant one is not, and a contravariant `A[object]` is an `A[int]`
    let Some(python) = ["python3.13"].into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }) else {
        eprintln!("skipping: no python 3.13 interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
class Co[out T]:
    def __init__(self): ...

class Inv[T]:
    def __init__(self):
        self.v: list[T] = []

class Con[in T]:
    def __init__(self): ...

def co(a: object):
    print(a is Co[object], a is Co[str])

def inv(a: object):
    print(a is Inv[object])

def con(a: object):
    print(a is Con[int])

co(Co[int]())
inv(Inv[int]())
con(Con[object]())
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .env("PYTHON", python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        // covariant Co[int] is Co[object] but not Co[str]; invariant Inv[int]
        // is not Inv[object]; contravariant Con[object] is Con[int]
        String::from_utf8_lossy(&output.stdout).trim(),
        "True False\nFalse\nTrue"
    );
}

#[test]
fn parametric_is_erased_union_answers_from_the_call_site() {
    // an empty `list[int]` records nothing in its mro, so no amount of looking
    // at the value can answer this — a probe used to say `False`, which was
    // sound (the positive branch is the only one that narrows) but useless.
    // the parameter's union is erased, so it carries a reified type parameter
    // and the answer comes from where the argument was written instead.
    //
    // asserted on the lowered output rather than by running it: reification
    // needs a 3.12+ *interpreter*, which the ambient `python3` may not be. the
    // runtime behaviour is covered by the mdtest divergence harness
    let out = transpile_at_313(
        "\
def x(a: list[int] | list[str]):
    print(a is list[int])

a: list[int] = []
x(a)
",
    );
    assert!(
        out.contains("print((__by_erased_0 == int))"),
        "the test reads the reified cell rather than probing the value:\n{out}"
    );
    assert!(
        out.contains("x[int](a)"),
        "the call site supplies the specialization the value cannot carry:\n{out}"
    );
}

/// the headline case: a concrete subclass fixes its type arguments in
/// `__orig_bases__`, so the runtime probe confirms it — even across the
/// `list` -> `Sequence` boundary, which pure `__mro__` introspection can't see
/// but `issubclass` + the recorded arguments can
#[test]
fn parametric_is_concrete_subclass_confirms_across_origins() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
from collections.abc import Sequence

class A(Sequence[int]):
    override def __getitem__(self, i): ...
    override def __len__(self): ...

class B(list[int]): ...

def f(x: object):
    print(x is Sequence[int])

f(A())
f(B())
print(object() is Sequence[int])
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by run failed:\n{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "True\nTrue\nFalse"
    );
}

#[test]
fn raises_guard_rejects_an_undeclared_exception_at_runtime() {
    // the guard exists for what the checker cannot see: `boom` raises whatever
    // it is handed, so only the runtime knows the clause was broken
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
def boom(kind: dynamic):
    raise kind(\"boom\")

def bad() raises ValueError:
    boom(TypeError)

def good() raises ValueError:
    raise ValueError(\"expected\")

def main():
    try:
        good()
    except ValueError as e:
        print(\"good\", type(e).__name__)
    try:
        bad()
    except BaseException as e:
        print(\"bad\", type(e).__name__)
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main", "--runtime-raises-checks"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "good ValueError\nbad AssertionError"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn raises_guard_of_a_reified_generic_tests_the_type_argument() {
    // a reified type parameter carries the type the caller chose, so the guard
    // tests exactly that rather than the ceiling: `PermissionError` is an
    // `OSError`, which the bound allows and `T = FileNotFoundError` does not.
    // the specialization is applied after the guard decorator, so this also says
    // the guard kept `rethrow[...]` answering
    let Some(python) = python_at_least_312() else {
        eprintln!("skipping: no python 3.12+ interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "def boom(kind: dynamic):
    raise kind(\"boom\")

def rethrow[reified T: OSError](kind: dynamic) raises T:
    boom(kind)

def main():
    try:
        rethrow[FileNotFoundError](FileNotFoundError)
    except BaseException as e:
        print(\"declared\", type(e).__name__)
    try:
        rethrow[FileNotFoundError](PermissionError)
    except BaseException as e:
        print(\"undeclared\", type(e).__name__)
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main", "--runtime-raises-checks"])
        .env("PYTHON", &python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "declared FileNotFoundError\nundeclared AssertionError"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn raises_guard_of_a_reified_class_reads_the_receiver() {
    // a class's type argument belongs to the instance, so a method's guard asks
    // the receiver it was called on for it
    let Some(python) = python_at_least_312() else {
        eprintln!("skipping: no python 3.12+ interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "def boom(kind: dynamic):
    raise kind(\"boom\")

class Rethrower[reified T: OSError]:
    def rethrow(self, kind: dynamic) raises T:
        boom(kind)

def main():
    r = Rethrower[FileNotFoundError]()
    try:
        r.rethrow(FileNotFoundError)
    except BaseException as e:
        print(\"declared\", type(e).__name__)
    try:
        r.rethrow(PermissionError)
    except BaseException as e:
        print(\"undeclared\", type(e).__name__)
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main", "--runtime-raises-checks"])
        .env("PYTHON", &python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "declared FileNotFoundError\nundeclared AssertionError"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn raises_guard_of_an_unreified_parameter_tests_its_ceiling() {
    // nothing carries `T` at runtime here, and asking for it would mean reifying
    // the parameter — an option that adds a check must not change the shape of
    // the program. so the guard tests what the declaration states without it:
    // an `OSError` passes, anything else does not. built for 3.9, the generic
    // `def` goes through the pep 695 polyfill, whose `TypeVar` definitions have
    // to land above the guard
    let Some(python) = python_at_least_312() else {
        eprintln!("skipping: no python 3.12+ interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "def boom(kind: dynamic):
    raise kind(\"boom\")

def rethrow[T: OSError](kind: dynamic) raises T:
    boom(kind)

def main():
    try:
        rethrow(PermissionError)
    except BaseException as e:
        print(\"inside the bound\", type(e).__name__)
    try:
        rethrow(ValueError)
    except BaseException as e:
        print(\"outside the bound\", type(e).__name__)
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args([
            "run",
            "main",
            "--runtime-raises-checks",
            "--min-version",
            "3.9",
        ])
        .env("PYTHON", &python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "inside the bound PermissionError\noutside the bound AssertionError"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn raises_guard_of_a_reified_generic_method_reads_the_bound_instance() {
    // a method that is itself reified is wrapped in `generic`, which binds the
    // receiver and passes the rest on: the class's argument comes from the
    // instance the method was looked up on, not from whatever is passed first
    let Some(python) = python_at_least_312() else {
        eprintln!("skipping: no python 3.12+ interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
def boom(kind: dynamic):
    raise kind(\"boom\")

class R[reified T: OSError]:
    def m[reified U](self, other: dynamic, marker: U, kind: dynamic) raises T:
        boom(kind)

def main():
    r = R[FileNotFoundError]()
    try:
        r.m(R[PermissionError](), 1, PermissionError)
    except BaseException as e:
        print(\"undeclared\", type(e).__name__)
    try:
        r.m(R[PermissionError](), 1, FileNotFoundError)
    except BaseException as e:
        print(\"declared\", type(e).__name__)
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main", "--runtime-raises-checks"])
        .env("PYTHON", &python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "undeclared AssertionError\ndeclared FileNotFoundError"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn raises_guard_of_a_nested_function_tests_a_class_parameter_at_its_ceiling() {
    // `inner` is not a method, so its first argument is not a receiver and the
    // class's argument cannot be read off it: the guard tests the bound instead
    let Some(python) = python_at_least_312() else {
        eprintln!("skipping: no python 3.12+ interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
def boom(kind: dynamic):
    raise kind(\"boom\")

class R[reified T: OSError]:
    def m(self):
        def inner(source: dynamic, kind: dynamic) raises T:
            boom(kind)
        for kind in [FileNotFoundError, PermissionError, ValueError]:
            try:
                inner(R[PermissionError](), kind)
            except BaseException as e:
                print(kind.__name__, type(e).__name__)

def main():
    R[FileNotFoundError]().m()
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main", "--runtime-raises-checks"])
        .env("PYTHON", &python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "FileNotFoundError FileNotFoundError\nPermissionError PermissionError\nValueError AssertionError"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn raises_guard_of_a_nested_function_reads_an_enclosing_reified_argument() {
    // the guard on `inner` is evaluated inside a call of `outer`, where `T` is
    // already the argument `outer` was specialized with
    let Some(python) = python_at_least_312() else {
        eprintln!("skipping: no python 3.12+ interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
def boom(kind: dynamic):
    raise kind(\"boom\")

def outer[reified T: OSError](kind: dynamic):
    def inner() raises T:
        boom(kind)
    try:
        inner()
    except BaseException as e:
        print(kind.__name__, type(e).__name__)

def main():
    outer[FileNotFoundError](FileNotFoundError)
    outer[FileNotFoundError](PermissionError)
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main", "--runtime-raises-checks"])
        .env("PYTHON", &python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "FileNotFoundError FileNotFoundError\nPermissionError AssertionError"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn raises_guard_tests_a_subscripted_type_argument_by_its_origin() {
    // `isinstance` refuses `MyErr[int]`, and a guard that raised `TypeError`
    // there would replace the exception the function legitimately raised. the
    // shallow test is the origin, as `list[str]` is tested as `list`
    let Some(python) = python_at_least_312() else {
        eprintln!("skipping: no python 3.12+ interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
def boom(kind: dynamic):
    raise kind(\"boom\")

class MyErr[X](OSError): ...

def rethrow[reified T: OSError](kind: dynamic) raises T:
    boom(kind)

def main():
    try:
        rethrow[MyErr[int]](MyErr)
    except BaseException as e:
        print(\"declared\", type(e).__name__)
    try:
        rethrow[MyErr[int]](PermissionError)
    except BaseException as e:
        print(\"undeclared\", type(e).__name__)
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main", "--runtime-raises-checks"])
        .env("PYTHON", &python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "declared MyErr\nundeclared AssertionError"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn raises_guard_keeps_a_reified_generic_documented() {
    // the guard wraps a reified generic in an object of its own, which must not
    // answer for the function's docstring with its own
    let Some(python) = python_at_least_312() else {
        eprintln!("skipping: no python 3.12+ interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
def rethrow[reified T: OSError](error: T) raises T:
    \"\"\"rethrows what it is given\"\"\"
    raise error

def main():
    print(rethrow.__doc__)
    print(rethrow[OSError].__doc__)
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main", "--runtime-raises-checks"])
        .env("PYTHON", &python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "rethrows what it is given\nrethrows what it is given"
    );
}

#[test]
fn raises_guard_covers_an_async_generator() {
    // an async generator answers `False` to both `iscoroutinefunction` and
    // `isgeneratorfunction`, so a wrapper that forgets it returns the generator
    // object without ever entering the body and catches nothing
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "\
async def boom(kind: dynamic):
    raise kind(\"x\")

async def gen() raises ValueError:
    yield 1
    await boom(TypeError)

async def drive():
    try:
        async for v in gen():
            print(\"got\", v)
    except BaseException as e:
        print(\"caught\", type(e).__name__)

def main():
    import asyncio
    asyncio.run(drive())
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main", "--runtime-raises-checks"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .trim(),
        "got 1\ncaught AssertionError"
    );
}

/// The `override-raise` strictness option is on under the default preset, and off under
/// `ty-compatible`.
///
/// mdtest force-enables every rule, including default-ignored ones, so the default posture can
/// only be pinned from outside it.
#[test]
fn override_raise_follows_the_preset() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = "\
def a() -> A:
    return B()

class A:
    def foo(self):
        pass

class B(A):
    override def foo(self):
        raise TypeError

def main():
    a().foo()
";
    fs::write(dir.path().join("main.by"), source).unwrap();

    let check = || {
        Command::new(env!("CARGO_BIN_EXE_by"))
            .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
            .arg("check")
            .current_dir(dir.path())
            .output()
            .expect("failed to spawn by")
    };

    let output = check();
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(
        rendered.contains("override-raise")
            && rendered.contains("which the method it overrides cannot"),
        "expected the override to be reported by default:\n{rendered}"
    );

    fs::write(
        dir.path().join("pyproject.toml"),
        "[tool.ty]\ntype-checking-preset = \"ty-compatible\"\n",
    )
    .unwrap();

    let output = check();
    assert!(
        output.status.success(),
        "expected no diagnostic under `ty-compatible`:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test prints why it was skipped"
)]
fn a_lazy_from_import_resolves_a_submodule_and_refuses_a_missing_name_as_python_does() {
    // `_LazyAttr` defers the attribute read, and reading an attribute is not how a
    // submodule gets bound: `urllib/__init__.py` never imports `parse`, and cpython
    // binds it only because `__import__` is handed a fromlist. so a transpiled
    // `from urllib import parse` used to raise `AttributeError` where the same source
    // run by cpython is fine — a wrong answer in shipped output rather than a decline
    //
    // the refusal for a name that really is missing is asserted against the
    // interpreter's own, on the same interpreter, rather than against a string
    // written here: a program that catches this reports it, so the report must not
    // say where the import was written
    let Some(python) = ["python3.13", "python3"].into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }) else {
        eprintln!("skipping: no python interpreter available");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname=\"s\"\nversion=\"0\"\nrequires-python=\">=3.13\"\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("main.by"),
        "from urllib import parse\nfrom urllib import nosuch\n\n\n\
         def main():\n\
         \x20   print(parse.quote(\"a b\"))\n\
         \x20   try:\n\
         \x20       print(nosuch)\n\
         \x20   except ImportError as e:\n\
         \x20       print(type(e).__name__, str(e), e.name, e.path, sep=\"|\")\n",
    )
    .unwrap();

    let transpiled = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["transpile", "main.by"])
        .env("PYTHON", python)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");
    assert!(
        transpiled.status.success(),
        "{}",
        String::from_utf8_lossy(&transpiled.stderr)
    );
    let program = String::from_utf8_lossy(&transpiled.stdout).into_owned();
    // the laziness is the thing under test, so its absence must not pass silently
    assert!(
        program.contains("_lazy_attr(\"urllib\", \"parse\")"),
        "{program}"
    );
    fs::write(dir.path().join("prog.py"), &program).unwrap();

    let run = |body: &str| {
        let out = Command::new(python)
            .args(["-c", body])
            .current_dir(dir.path())
            .output()
            .expect("the interpreter runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let mut lines = run("import prog; prog.main()")
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(
        lines.first().map(String::as_str),
        Some("a%20b"),
        "{lines:?}"
    );
    let ours = lines.pop().expect("the refusal is printed");

    // the same import, written the way python writes it, refused by python itself
    let theirs = run(
        "try:\n    from urllib import nosuch\nexcept ImportError as e:\n\
         \x20   print(type(e).__name__, str(e), e.name, e.path, sep='|')\n",
    );
    assert_eq!(ours, theirs);
}

// ── building a project, not just its `.by` files ─────────────────────────────

/// a project is its hand-written python too. an output tree holding only the
/// transpiled half is not a project: the first `import` of a `.py` sibling
/// fails, and there is nothing the author can do about it from the `.by` side
#[test]
fn build_carries_a_python_module_into_the_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "from helper import shout\n").unwrap();
    fs::write(
        dir.path().join("helper.py"),
        "def shout(text: str) -> str:\n    return text.upper()\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    assert_eq!(
        fs::read_to_string(dir.path().join("build/helper.py")).unwrap(),
        "def shout(text: str) -> str:\n    return text.upper()\n",
        "a hand-written python module belongs in the output verbatim"
    );
}

/// and its data. a program that opens a file beside itself is the ordinary case,
/// not an exotic one
#[test]
fn build_carries_data_files_into_the_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let package = dir.path().join("app");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("__init__.by"), "").unwrap();
    fs::write(package.join("settings.json"), "{\"key\": 1}\n").unwrap();
    fs::write(package.join("py.typed"), "").unwrap();
    fs::write(package.join("template.html"), "<p>hi</p>\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    let out = dir.path().join("build").join("app");
    assert_eq!(
        fs::read_to_string(out.join("settings.json")).unwrap(),
        "{\"key\": 1}\n"
    );
    assert!(out.join("py.typed").exists());
    assert!(out.join("template.html").exists());
}

/// a stub is not a module: emitting `a.byi` as `a.py` would put a body-less
/// definition where python imports the implementation, and shadow the real
/// module at runtime
#[test]
fn build_writes_a_stub_as_a_stub() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "x = 1\n").unwrap();
    fs::write(dir.path().join("shapes.byi"), "def area() -> int: ...\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    assert!(
        dir.path().join("build/shapes.pyi").exists(),
        "a `.byi` builds to a `.pyi`:\n{stderr}"
    );
    assert!(
        !dir.path().join("build/shapes.py").exists(),
        "a stub emitted as a module shadows the implementation"
    );
}

/// a stub is read by a checker and never run, so a built one holds what its
/// source declares and none of what a module gets for running: its imports stay
/// imports, and `main` gets no entry point. a build hands every source the same
/// config, so it is the file that says it is a stub
#[test]
fn build_writes_a_stub_as_declarations() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "x = 1\n").unwrap();
    let stub =
        "import json\nfrom dataclasses import dataclass\n\ndef main(name: str) -> None: ...\n";
    fs::write(dir.path().join("shapes.byi"), stub).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    assert_eq!(
        fs::read_to_string(dir.path().join("build/shapes.pyi")).unwrap(),
        stub
    );
}

/// `a.by` and a hand-written `a.py` are both the module `a`. picking one and
/// carrying on means the build disagrees with what python will import, so this
/// is reported rather than resolved
#[test]
fn build_refuses_two_sources_that_are_one_module() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("thing.by"), "x = 1\n").unwrap();
    fs::write(dir.path().join("thing.py"), "x = 2\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a collision must fail the build:\n{stderr}"
    );
    assert!(
        stderr.contains("same module"),
        "the collision must say what is wrong:\n{stderr}"
    );
}

/// an output tree that only ever grows keeps a module that was deleted months
/// ago importable — locally, where nobody notices, and then in the wheel built
/// from the same tree, where somebody does
#[test]
fn build_deletes_output_the_project_no_longer_has() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("kept.by"), "x = 1\n").unwrap();
    fs::write(dir.path().join("removed.by"), "y = 2\n").unwrap();

    let build = || {
        let output = Command::new(env!("CARGO_BIN_EXE_by"))
            .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
            .arg("build")
            .current_dir(dir.path())
            .output()
            .expect("failed to spawn by");
        assert!(
            output.status.success(),
            "by build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };

    build();
    assert!(dir.path().join("build/removed.py").exists());

    fs::remove_file(dir.path().join("removed.by")).unwrap();
    build();

    assert!(dir.path().join("build/kept.py").exists());
    assert!(
        !dir.path().join("build/removed.py").exists(),
        "output for a source that is gone must not survive the next build"
    );
}

/// only what the build itself wrote is ever deleted — anything else in the
/// output directory was put there by somebody
#[test]
fn build_leaves_output_it_never_wrote_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "x = 1\n").unwrap();
    fs::create_dir_all(dir.path().join("build")).unwrap();
    fs::write(dir.path().join("build/theirs.txt"), "hands off\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.path().join("build/theirs.txt").exists());
}

#[test]
fn build_writes_where_out_says() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "x = 1\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["build", "--out", "elsewhere"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    assert!(dir.path().join("elsewhere/main.py").exists());
    assert!(!dir.path().join("build").exists());
}

/// the output directory is not an input to itself, wherever it is put
#[test]
fn build_does_not_read_its_own_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "x = 1\n").unwrap();

    for _ in 0..2 {
        let output = Command::new(env!("CARGO_BIN_EXE_by"))
            .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
            .args(["build", "--out", "elsewhere"])
            .current_dir(dir.path())
            .output()
            .expect("failed to spawn by");
        assert!(
            output.status.success(),
            "by build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    assert!(
        !dir.path().join("elsewhere/elsewhere").exists(),
        "a second build must not copy the first build's output into itself"
    );
}

/// a source distribution has to carry exactly what the build read, and a wheel
/// exactly the packages it produced. both are the build's answers
#[test]
fn build_reports_what_it_read_and_what_it_produced() {
    let dir = tempfile::tempdir().expect("tempdir");
    let package = dir.path().join("src").join("app");
    fs::create_dir_all(&package).unwrap();
    fs::write(dir.path().join("README.md"), "# app\n").unwrap();
    fs::write(package.join("__init__.by"), "").unwrap();
    fs::write(package.join("helper.py"), "x = 1\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["build", "--print-manifest"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let listed: Vec<&str> = stdout.lines().collect();
    assert!(
        output.status.success(),
        "by build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for expected in ["README.md", "src/app/__init__.by", "src/app/helper.py"] {
        let expected = format!(
            "input {}",
            expected.replace('/', std::path::MAIN_SEPARATOR_STR)
        );
        assert!(
            listed.contains(&expected.as_str()),
            "`{expected}` is part of this project:\n{stdout}"
        );
    }
    assert!(
        listed.contains(&"package app"),
        "the package the wheel ships:\n{stdout}"
    );
    assert_eq!(
        listed
            .iter()
            .filter(|line| line.ends_with("__init__.by"))
            .count(),
        1,
        "a source that produced two outputs is still one input:\n{stdout}"
    );
}

/// `tests` beside `src` is a package python can import and a package nobody
/// installs. a wheel that shipped it would put a top-level `tests` module into
/// every environment the project is installed into
#[test]
fn build_does_not_ship_what_lives_outside_the_source_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let package = dir.path().join("src").join("app");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("__init__.by"), "").unwrap();
    let tests = dir.path().join("tests");
    fs::create_dir_all(&tests).unwrap();
    fs::write(tests.join("__init__.py"), "").unwrap();
    fs::write(tests.join("test_it.py"), "def test_x(): pass\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["build", "--print-manifest"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let listed: Vec<&str> = stdout.lines().collect();
    assert!(
        output.status.success(),
        "by build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(listed.contains(&"package app"), "{stdout}");
    assert!(
        !listed.contains(&"package tests"),
        "`tests` is not part of the distribution:\n{stdout}"
    );
    // it is still built, because it is still the project — running the tests out
    // of the output tree is the point of building them
    assert!(dir.path().join("build/tests/test_it.py").exists());
    assert!(
        !dir.path().join("build/tests/by.typed").exists(),
        "a marker only speaks for what the project ships"
    );
}

/// the marker is what tells a downstream basedpython project to read the `.by`
/// beside a module rather than the python it was transpiled into
#[test]
fn build_marks_a_package_as_carrying_its_sources() {
    let dir = tempfile::tempdir().expect("tempdir");
    let package = dir.path().join("app");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("__init__.by"), "").unwrap();
    fs::write(package.join("deep.by"), "x = 1\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    let out = dir.path().join("build").join("app");
    assert!(
        out.join("by.typed").exists(),
        "expected a marker:\n{stderr}"
    );
    assert!(
        out.join("deep.by").exists(),
        "the marker is a claim about sources, which have to be there:\n{stderr}"
    );
    assert!(out.join("deep.py").exists());
}

#[test]
fn build_ships_python_only_when_the_project_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\
         \n[tool.basedpython.build]\nsources = false\n",
    )
    .unwrap();
    let package = dir.path().join("app");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("__init__.by"), "").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    let out = dir.path().join("build").join("app");
    assert!(out.join("__init__.py").exists());
    assert!(
        !out.join("__init__.by").exists(),
        "`sources = false` ships python only"
    );
    // the marker still goes out. its precedence claim is vacuous without sources
    // — there is no `.by` to prefer — but its contents are what declare which
    // dependencies this project hands out on purpose, and a python-only build has
    // those too
    assert!(
        out.join("by.typed").exists(),
        "the marker carries the export declaration, sources or no sources"
    );
}

#[test]
fn build_honours_the_configured_exclusions() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\
         \n[tool.basedpython.build]\nexclude = [\"secrets.json\"]\n",
    )
    .unwrap();
    fs::write(dir.path().join("main.by"), "x = 1\n").unwrap();
    fs::write(dir.path().join("secrets.json"), "{}\n").unwrap();
    fs::write(dir.path().join("public.json"), "{}\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    assert!(dir.path().join("build/public.json").exists());
    assert!(
        !dir.path().join("build/secrets.json").exists(),
        "an excluded file must not reach the output"
    );
}

/// a directory ty's defaults drop can be taken back with a negated exclude, and
/// the build has to honour that for every file in it — not just the `.by` ones.
/// re-dropping the rest would leave the transpiled half of a directory the
/// project deliberately re-included
#[test]
fn build_carries_a_directory_a_negated_exclude_takes_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\
         \n[tool.basedpython.src]\nexclude = [\"!dist\"]\n",
    )
    .unwrap();
    let generated = dir.path().join("dist");
    fs::create_dir_all(&generated).unwrap();
    fs::write(generated.join("kept.by"), "x = 1\n").unwrap();
    fs::write(generated.join("kept.json"), "{}\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    assert!(
        dir.path().join("build/dist/kept.py").exists(),
        "the re-included `.by` builds:\n{stderr}"
    );
    assert!(
        dir.path().join("build/dist/kept.json").exists(),
        "and so does everything beside it:\n{stderr}"
    );
}

/// the rule follows the module tree rather than the name `src`. a `src` that is
/// itself a package is not a source root, so the module really is `src.mymod` —
/// and a wheel that dropped the `src` component would ship a package under a name
/// nothing imports
#[test]
fn build_ships_a_source_directory_that_is_itself_a_package() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"mymod\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let package = dir.path().join("src").join("mymod");
    fs::create_dir_all(&package).unwrap();
    fs::write(dir.path().join("src").join("__init__.py"), "").unwrap();
    fs::write(package.join("__init__.by"), "").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["build", "--print-manifest"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "by build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.lines().any(|line| line == "package src"),
        "`src.mymod` is the module, so `src` is the package:\n{stdout}"
    );
    assert!(dir.path().join("build/src/mymod/__init__.py").exists());
}

/// lowering for an older python can put a name in the output that only
/// `typing_extensions` has there. nothing in the source says so — the project
/// never asked for it — so nothing but the build can, and a wheel that shipped
/// without it would install cleanly and fail on the first import
#[test]
fn build_reports_what_lowering_needs_at_run_time() {
    let dir = tempfile::tempdir().expect("tempdir");
    let package = dir.path().join("src").join("app");
    fs::create_dir_all(&package).unwrap();
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"app\"\nversion = \"0.1.0\"\nrequires-python = \">=3.9\"\n",
    )
    .unwrap();
    // `Self` reached `typing` in 3.11, so a 3.9 target has to borrow it
    fs::write(
        package.join("__init__.by"),
        "from typing import Self\n\nclass N:\n    def me(self) -> Self:\n        return self\n",
    )
    .unwrap();

    let manifest = |extra: &[&str]| -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_by"))
            .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
            .args(["build", "--print-manifest"])
            .args(extra)
            .current_dir(dir.path())
            .output()
            .expect("failed to spawn by");
        assert!(
            output.status.success(),
            "by build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    let lowered = manifest(&[]);
    assert!(
        lowered
            .lines()
            .any(|line| line.starts_with("requires typing_extensions")),
        "a 3.9 target borrows the name, so the wheel depends on it:\n{lowered}"
    );

    // and on a python that has it, the dependency would be dead weight
    let native = manifest(&["--min-version", "3.13"]);
    assert!(
        !native.contains("requires "),
        "a 3.13 target needs no backport:\n{native}"
    );
}

/// the packaging is `uv`'s, so without it there is nothing to drive — and the
/// command has to say that rather than fail somewhere further in
#[test]
fn building_wheels_without_a_frontend_says_what_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\nrequires-python = \">=3.12\"\n",
    )
    .unwrap();
    let package = dir.path().join("src").join("demo");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("__init__.by"), "").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["build", "--wheels"])
        .current_dir(dir.path())
        // an empty `PATH` is the only way to be sure this machine's `uv` is not
        // found, whatever the developer happens to have installed
        .env("PATH", "")
        .env_remove("VIRTUAL_ENV")
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "expected a failure:\n{stderr}");
    assert!(
        stderr.contains("could not find `uv`"),
        "the message has to name what is missing:\n{stderr}"
    );
    assert!(
        stderr.contains("uv build"),
        "and what to do without it:\n{stderr}"
    );
}

/// one source touched by all three lowering options, so a test can tell which
/// of them crossed the wire and which quietly did not
///
/// each moves in a different direction, so a payload of non-defaults cannot
/// pass by accident: the soundness check disappears, the raises guard appears,
/// and the loop's per-iteration wrapper disappears
const LOWERED_THREE_WAYS: &str = "\
items: list[int] = []
fns: list[object] = []


def t[T]() -> T:
    raise NotImplementedError


def uses():
    a: str = t()


def f() raises ValueError:
    raise ValueError


for i in items:
    fns.append(lambda: i)
";

/// build `source` in a fresh project and return the emitted python
fn build_emitting(source: &str, settled: Option<&str>) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), source).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_by"));
    command.env(EnvVars::BY_NO_PROJECT_SERVER, "1");
    command
        .args(["build", "--out", "build"])
        .current_dir(dir.path());
    match settled {
        Some(settled) => command.env("BY_BUILD_LOWERING", settled),
        // whatever the developer's own environment holds must not decide what
        // this observes
        None => command.env_remove("BY_BUILD_LOWERING"),
    };
    let output = command.output().expect("failed to spawn by");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "by build failed:\n{stderr}");
    fs::read_to_string(dir.path().join("build").join("main.py")).expect("emitted python")
}

/// the builds inside a `--wheels` release are the ones that transpile, so every
/// lowering option has to reach them. one that stopped at the outer command
/// would be accepted and then change nothing about the wheels
#[test]
fn a_build_inside_a_release_lowers_the_way_the_release_settled() {
    // on its own the build lowers with the defaults, which is what makes each
    // difference below mean something
    let alone = build_emitting(LOWERED_THREE_WAYS, None);
    assert!(
        alone.contains("_soundness_check(t(), str)"),
        "checks are on by default:\n{alone}"
    );
    assert!(
        !alone.contains("_by_raises"),
        "the raises guard is off by default:\n{alone}"
    );
    assert!(
        alone.contains("fns.append((lambda i: lambda: i)(i))"),
        "a loop binds per iteration by default:\n{alone}"
    );

    let inside_a_release = build_emitting(
        LOWERED_THREE_WAYS,
        Some(r#"{"soundness":"none","runtime_raises_checks":true,"no_unique_loop_bindings":true}"#),
    );
    assert!(
        !inside_a_release.contains("_soundness_check"),
        "the release settled `--soundness none`:\n{inside_a_release}"
    );
    assert!(
        inside_a_release.contains("_by_raises"),
        "the release settled `--runtime-raises-checks`:\n{inside_a_release}"
    );
    assert!(
        inside_a_release.contains("fns.append(lambda: i)"),
        "the release settled `--no-unique-loop-bindings`:\n{inside_a_release}"
    );
}

/// the two `by` executables in a release need not be the same build, because a
/// project asks for `basedpython` by a lower bound and the backend prefers the
/// one the frontend resolved. so a build has to read a message written by a
/// `by` that had never heard of one of these options, and take the rest of it
#[test]
fn a_build_reads_settled_lowering_written_by_an_older_by() {
    let emitted = build_emitting(LOWERED_THREE_WAYS, Some(r#"{"soundness":"none"}"#));
    assert!(
        !emitted.contains("_soundness_check"),
        "the option that was written still applies:\n{emitted}"
    );
    assert!(
        !emitted.contains("_by_raises"),
        "and one that was not written takes its own default:\n{emitted}"
    );
}

/// the variable is the outer command's, so anything else in it is not this
/// build's to refuse: it lowers the way it would have anyway rather than
/// stopping for a reason nobody could act on
#[test]
fn a_build_ignores_settled_lowering_it_cannot_read() {
    let emitted = build_emitting(LOWERED_THREE_WAYS, Some("not the outer command's"));
    assert!(
        emitted.contains("_soundness_check(t(), str)"),
        "an unreadable value leaves the lowering alone:\n{emitted}"
    );
}

/// only `by build` is ever run inside a release, and these options change what
/// the emitted python does — so a variable left in the environment must not
/// silently re-lower a `by transpile` that was given options of its own
#[test]
fn only_a_build_takes_its_lowering_from_the_environment() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), LOWERED_THREE_WAYS).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["transpile", "main.by"])
        .current_dir(dir.path())
        .env(
            "BY_BUILD_LOWERING",
            r#"{"soundness":"none","runtime_raises_checks":false,"no_unique_loop_bindings":false}"#,
        )
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by transpile failed:\n{stderr}");
    let emitted = String::from_utf8_lossy(&output.stdout);
    assert!(
        emitted.contains("_soundness_check(t(), str)"),
        "a stray variable must not re-lower an unrelated transpile:\n{emitted}"
    );
}

/// the release settles the options once, so a spec it cannot parse is one error
/// from the command the user ran — not the same error from each of the builds
/// inside it, and not a release that got as far as driving the frontend
#[test]
fn building_wheels_refuses_a_soundness_spec_it_cannot_parse() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "x = 1\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["build", "--wheels", "--soundness", "nonsense"])
        .current_dir(dir.path())
        // refused before the frontend is even looked for, so this holds on a
        // machine with no `uv` as well as on one with it
        .env("PATH", "")
        .env_remove("VIRTUAL_ENV")
        .output()
        .expect("failed to spawn by");

    assert!(!output.status.success(), "expected a failure");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown soundness position"),
        "the message has to name what was wrong with the spec:\n{stderr}"
    );
}

/// what the outer command actually writes, which nothing else here observes:
/// every test above sets the variables by hand, so a release that wrote them to
/// the wrong names — or swapped them — would pass all of them
///
/// this drives the real `cmd_build_wheels` with a stub standing in for `uv`, so
/// it needs no frontend and no network. the release fails afterwards, because
/// the stub builds nothing, but by then it has recorded what it was handed
#[cfg(unix)]
#[test]
fn a_release_hands_each_build_the_stamps_and_the_lowering() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\nrequires-python = \">=3.13\"\n",
    )
    .unwrap();
    fs::write(dir.path().join("main.by"), "x = 1\n").unwrap();

    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let report = dir.path().join("handed.txt");
    let uv = bin.join("uv");
    fs::write(
        &uv,
        "#!/bin/sh\n\
         printf 'STAMPS=%s\\nLOWERING=%s\\n' \"$BY_BUILD_STAMPS\" \"$BY_BUILD_LOWERING\" \
         >> \"$BY_TEST_REPORT\"\n",
    )
    .unwrap();
    fs::set_permissions(&uv, fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["build", "--wheels", "--soundness", "none"])
        .current_dir(dir.path())
        // the stub is the only `uv` reachable, so this cannot accidentally
        // drive the developer's real one
        .env("PATH", &bin)
        .env_remove("VIRTUAL_ENV")
        .env("BY_TEST_REPORT", &report)
        .output()
        .expect("failed to spawn by");

    let handed = fs::read_to_string(&report).unwrap_or_else(|_| {
        panic!(
            "the release never ran the frontend:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    });

    assert!(
        handed.contains(r#"LOWERING={"soundness":"none""#),
        "the release has to hand down what it was asked to lower to:\n{handed}"
    );
    // and the stamps still travel beside them, under their own name
    assert!(
        handed.contains(r#"STAMPS={"#) && handed.contains("BUILT_AT"),
        "the stamps must not have been displaced:\n{handed}"
    );
}

/// `--wheels` produces a release, `--min-version` produces one tree lowered to
/// one python. asking for both is asking for two different things at once
#[test]
fn building_wheels_refuses_a_single_target_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "x = 1\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["build", "--wheels", "--min-version", "3.12"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot be used with"),
        "clap has to reject the combination:\n{stderr}"
    );
}

// ── running a project, not just its `.by` files ──────────────────────────────

/// the same hole at run time, where it is fatal rather than untidy: `by run`
/// executes out of a directory it stages, so a `.py` module missing from it
/// cannot be imported at all
#[test]
fn run_imports_a_python_module_beside_the_transpiled_ones() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "from helper import shout\n\nprint(shout(\"mixed\"))\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("helper.py"),
        "def shout(text: str) -> str:\n    return text.upper()\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "MIXED");
}

#[test]
fn run_reads_a_data_file_beside_the_program() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.by"),
        "from pathlib import Path\n\n\
         print(Path(__file__).parent.joinpath(\"greeting.txt\").read_text().strip())\n",
    )
    .unwrap();
    fs::write(dir.path().join("greeting.txt"), "read from disk\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    assert!(
        output.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "read from disk"
    );
}

/// running a project on an interpreter older than it targets used to fail as a
/// `SyntaxError` inside generated code, in a temporary directory that was
/// already deleted. it is knowable before anything runs, so it is said before
/// anything runs
#[test]
fn run_refuses_an_interpreter_older_than_the_project_targets() {
    let dir = tempfile::tempdir().expect("tempdir");
    // the environment is named rather than discovered, so the version the project
    // is refused for is the version of the interpreter it would actually have run
    // on — probing whatever `python3` resolves to says nothing about the one
    // `by run` would pick
    let environment = python_environment(&dir.path().join(".venv"));
    let (major, minor) = environment.version;
    let unreachable = format!("{major}.{}", minor + 1);
    fs::write(
        dir.path().join("pyproject.toml"),
        format!(
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             requires-python = \">={unreachable}\"\n\
             \n[tool.basedpython.environment]\npython = \".venv\"\n"
        ),
    )
    .unwrap();
    fs::write(dir.path().join("main.by"), "print(1)\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .env_remove("PYTHON")
        .env_remove("VIRTUAL_ENV")
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a project that cannot run on this interpreter must say so:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("targets python {unreachable}")),
        "the message has to name both versions:\n{stderr}"
    );
    assert!(
        stderr.contains("--min-version"),
        "and what to do about it:\n{stderr}"
    );
}

/// A real virtual environment, and the version of the interpreter in it.
///
/// `python -m venv` rather than a directory shaped like one: this is what
/// discovery looks for, `by run` has to be able to *execute* what it finds, and
/// on windows a python is not something a shell script can stand in for.
///
/// The version comes back because it is the thing a project has to declare it
/// targets. Probing whatever `python3` resolves to says nothing about the
/// interpreter `by run` would pick, which is the whole question here.
struct Environment {
    root: PathBuf,
    version: (u8, u8),
}

fn python_environment(root: &Path) -> Environment {
    let status = Command::new("python3")
        .args(["-m", "venv", "--without-pip"])
        .arg(root)
        .status()
        .expect("python3 is needed to run this test");
    assert!(
        status.success(),
        "could not create a virtual environment at {}",
        root.display()
    );
    Environment {
        root: root.to_path_buf(),
        version: interpreter_version(&interpreter_in(root)),
    }
}

impl Environment {
    /// what a project must say it targets to run on this
    fn requires_python(&self) -> String {
        let (major, minor) = self.version;
        format!("requires-python = \">={major}.{minor}\"\n")
    }

    fn interpreter(&self) -> PathBuf {
        interpreter_in(&self.root)
    }

    /// Whether the program ran on this environment's interpreter.
    ///
    /// Compared as canonical *directories*: windows hands a process the short
    /// (`RUNNER~1`) form of a path it was given the long form of, so two
    /// spellings of one directory do not compare equal as text. It is the
    /// directory that is canonicalized rather than the interpreter itself,
    /// because a virtual environment's `python3` is a symlink to the interpreter
    /// it was made from — resolving *that* leads out of the environment, which is
    /// the one place this must not look.
    fn ran_it(&self, stdout: &str) -> bool {
        let reported = PathBuf::from(stdout.trim());
        let Some(Ok(directory)) = reported.parent().map(fs::canonicalize) else {
            return false;
        };
        let Ok(root) = fs::canonicalize(&self.root) else {
            return false;
        };
        directory.starts_with(root)
    }
}

/// the program these tests run: it reports the interpreter that ran it, which is
/// the whole question
const REPORTS_ITS_INTERPRETER: &str = "import sys\n\nprint(sys.executable)\n";

fn interpreter_in(root: &Path) -> PathBuf {
    if cfg!(windows) {
        root.join("Scripts").join("python.exe")
    } else {
        root.join("bin").join("python3")
    }
}

fn interpreter_version(python: &Path) -> (u8, u8) {
    let output = Command::new(python)
        .args([
            "-c",
            "import sys; print(f'{sys.version_info[0]} {sys.version_info[1]}')",
        ])
        .output()
        .unwrap_or_else(|error| panic!("could not run {}: {error}", python.display()));
    let rendered = String::from_utf8_lossy(&output.stdout);
    let mut parts = rendered.split_whitespace();
    let major = parts.next().expect("a major version").parse().unwrap();
    let minor = parts.next().expect("a minor version").parse().unwrap();
    (major, minor)
}

/// the version of the interpreter `by run` would pick, so a test can name one
/// that is definitely newer
fn running_python_version() -> (u8, u8) {
    let output = Command::new("python3")
        .args([
            "-c",
            "import sys; print(f'{sys.version_info[0]} {sys.version_info[1]}')",
        ])
        .output()
        .expect("python3 is needed to run this test");
    let rendered = String::from_utf8_lossy(&output.stdout);
    let mut parts = rendered.split_whitespace();
    let major = parts.next().unwrap().parse().unwrap();
    let minor = parts.next().unwrap().parse().unwrap();
    (major, minor)
}

/// the project environment is the environment the project *is* — `by check`
/// resolved this project's imports against it, so running against a different
/// python answers a question nobody asked
#[test]
fn run_uses_the_environment_the_project_configures() {
    let dir = tempfile::tempdir().expect("tempdir");
    let environment = python_environment(&dir.path().join("environments").join("current"));
    fs::write(
        dir.path().join("pyproject.toml"),
        format!(
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n{}\
             \n[tool.basedpython.environment]\npython = \"environments/current\"\n",
            environment.requires_python()
        ),
    )
    .unwrap();
    fs::write(dir.path().join("main.by"), REPORTS_ITS_INTERPRETER).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .env_remove("PYTHON")
        .env_remove("VIRTUAL_ENV")
        .output()
        .expect("failed to spawn by");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        environment.ran_it(&stdout),
        "expected the configured environment's interpreter ({}):\n{stdout}\n{}",
        environment.interpreter().display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// a `.venv` belongs to the project, not to whichever directory the command was
/// run from — and neither do the sources
#[test]
fn run_from_a_subdirectory_is_still_the_project() {
    let dir = tempfile::tempdir().expect("tempdir");
    let environment = python_environment(&dir.path().join(".venv"));
    fs::write(
        dir.path().join("pyproject.toml"),
        format!(
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n{}\
             \n[tool.basedpython.run]\nmain = \"app.main\"\n",
            environment.requires_python()
        ),
    )
    .unwrap();
    let package = dir.path().join("src").join("app");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("__init__.by"), "").unwrap();
    fs::write(package.join("main.by"), REPORTS_ITS_INTERPRETER).unwrap();
    let elsewhere = dir.path().join("tools");
    fs::create_dir_all(&elsewhere).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("run")
        .current_dir(&elsewhere)
        .env_remove("PYTHON")
        .env_remove("VIRTUAL_ENV")
        .output()
        .expect("failed to spawn by");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        environment.ran_it(&stdout),
        "the project's `.venv` is the project's wherever this was run:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn build_from_a_subdirectory_builds_the_project() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let package = dir.path().join("src").join("app");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("__init__.by"), "").unwrap();
    let elsewhere = dir.path().join("tools");
    fs::create_dir_all(&elsewhere).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(&elsewhere)
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    assert!(
        elsewhere
            .join("build")
            .join("app")
            .join("__init__.py")
            .exists(),
        "the module tree is the project's, not the caller's:\n{stderr}"
    );
}

/// `$PYTHON` names an interpreter, not an environment, so it stands in only where
/// there is no project environment to prefer. this is a change in what the
/// variable does: it used to be the only mechanism, and so beat everything
#[test]
fn run_prefers_the_project_environment_to_the_python_variable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let project = python_environment(&dir.path().join(".venv"));
    // a second environment, so that what `$PYTHON` names is never what the
    // project would have chosen anyway — otherwise the two answers are the same
    // and the test asserts nothing
    let named = python_environment(&dir.path().join("named"));
    fs::write(
        dir.path().join("pyproject.toml"),
        format!(
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n{}",
            project.requires_python()
        ),
    )
    .unwrap();
    fs::write(dir.path().join("main.by"), REPORTS_ITS_INTERPRETER).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .env("PYTHON", named.interpreter())
        .env_remove("VIRTUAL_ENV")
        .output()
        .expect("failed to spawn by");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        project.ran_it(&stdout),
        "the project's environment outranks `$PYTHON`:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// and where there is no project environment, `$PYTHON` is still what stands in —
/// demoting it below discovery entirely would have made it dead, since discovery
/// always ends at *some* interpreter on `PATH`
#[test]
fn run_falls_back_to_the_python_variable() {
    let dir = tempfile::tempdir().expect("tempdir");
    // outside the project, so that discovery does not find it and the only way
    // to reach it is the variable
    let elsewhere = python_environment(&dir.path().join("chosen"));
    let project = dir.path().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("pyproject.toml"),
        format!(
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n{}",
            elsewhere.requires_python()
        ),
    )
    .unwrap();
    fs::write(project.join("main.by"), REPORTS_ITS_INTERPRETER).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(&project)
        .env("PYTHON", elsewhere.interpreter())
        .env_remove("VIRTUAL_ENV")
        .output()
        .expect("failed to spawn by");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        elsewhere.ran_it(&stdout),
        "with no project environment, `$PYTHON` is the answer:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// a configured environment that cannot be resolved is what `by check` refuses
/// outright. falling past it ran the program on a different python than the one
/// it had just been checked against, and reported that as a version mismatch —
/// naming the wrong cause entirely
#[test]
fn run_refuses_a_configured_environment_that_is_not_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\
         \n[tool.basedpython.environment]\npython = \"absent\"\n",
    )
    .unwrap();
    fs::write(dir.path().join("main.by"), "print(1)\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "expected a refusal:\n{stderr}");
    assert!(
        stderr.contains("`environment.python`"),
        "the message has to name the setting that is wrong:\n{stderr}"
    );
}

/// the shim `by run` puts in the tree it executes is written through the same
/// staging as everything else, so a project file of that name is a reported
/// collision rather than a silent overwrite
#[test]
fn run_refuses_a_project_file_that_collides_with_its_shim() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "print(1)\n").unwrap();
    fs::write(dir.path().join("_by_runner.py"), "x = 1\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args(["run", "main"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "expected a refusal:\n{stderr}");
    assert!(stderr.contains("_by_runner.py"), "{stderr}");
    assert!(stderr.contains("same module"), "{stderr}");
}

/// a compiler's output directory is not project source, and it is the one most
/// likely to be enormous — this used to be copied in full on every build and
/// every run
#[test]
fn build_does_not_carry_a_compilers_output_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "x = 1\n").unwrap();
    let artifacts = dir.path().join("target").join("debug");
    fs::create_dir_all(&artifacts).unwrap();
    fs::write(artifacts.join("blob"), "an enormous binary").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    assert!(dir.path().join("build").join("main.py").exists());
    assert!(
        !dir.path().join("build").join("target").exists(),
        "a build directory must not be carried into the build:\n{stderr}"
    );
}

// ── starting a project ───────────────────────────────────────────────────────

/// what `by init` writes has to be a project the rest of the toolchain accepts,
/// or it is a template for a thing that does not work
#[test]
fn init_writes_a_project_that_builds_and_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (major, minor) = running_python_version();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .args([
            "init",
            "demo",
            "--python-version",
            &format!("{major}.{minor}"),
        ])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by init failed:\n{stderr}");

    let project = dir.path().join("demo");
    let pyproject = fs::read_to_string(project.join("pyproject.toml")).unwrap();
    assert!(pyproject.contains("build-backend = \"basedpython.build\""));
    assert!(project.join("src/demo/__init__.by").exists());

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("run")
        .current_dir(&project)
        .output()
        .expect("failed to spawn by");
    assert!(
        output.status.success(),
        "a new project has to run:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "hello from basedpython"
    );
}

#[test]
fn init_refuses_to_write_over_a_project() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"already-here\"\nversion = \"9.9.9\"\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .env(EnvVars::BY_NO_PROJECT_SERVER, "1")
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "expected a refusal:\n{stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
    assert!(
        fs::read_to_string(dir.path().join("pyproject.toml"))
            .unwrap()
            .contains("9.9.9"),
        "the existing project must be untouched"
    );
}

/// a project for the runtime tests: a package whose modules import one another
/// relatively, a subpackage, a hoisted named tuple and a call to a helper, and a
/// script in a folder that is no package at all
fn runtime_project(name: &str) -> PathBuf {
    let dir = cli_root().join(name);
    let _ = fs::remove_dir_all(&dir);
    let app = dir.join("src").join("app");
    fs::create_dir_all(app.join("sub")).unwrap();
    fs::create_dir_all(dir.join("scripts")).unwrap();
    fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname=\"s\"\nversion=\"0\"\nrequires-python=\">=3.11\"\n",
    )
    .unwrap();
    fs::write(app.join("__init__.py"), "").unwrap();
    fs::write(app.join("sub").join("__init__.py"), "").unwrap();
    // `cast!` of an `object` to a parameterized class is a runtime probe, and
    // `int??` in a named tuple
    // field is the runtime's `Optional` evaluated as the hoisted class is built
    fs::write(
        app.join("one.by"),
        "import json\n\n\ndef dumps(a: object) -> str:\n    return json.dumps(a cast! dict[str, int])\n\n\n\
         def pair() -> (a: int??, b: int):\n    return (a=None, b=2)\n",
    )
    .unwrap();
    fs::write(
        app.join("sub").join("deep.by"),
        "from .. import one\n\n\ndef twice(a: dict[str, int]) -> str:\n    return one.dumps(a) * 2\n",
    )
    .unwrap();
    fs::write(
        app.join("main.by"),
        "from . import one\nfrom .sub import deep\n\n\ndef main():\n    \
         print(one.dumps({}), deep.twice({}), one.pair().b)\n",
    )
    .unwrap();
    fs::write(
        dir.join("scripts").join("tool.by"),
        "def f(a: object) -> int:\n    return a cast! int\n\n\nprint(f(3))\n",
    )
    .unwrap();
    fs::write(
        dir.join("src").join("cli.by"),
        "def f(a: object) -> int:\n    return a cast! int\n\n\nprint(f(4))\n",
    )
    .unwrap();
    dir
}

fn build_in(dir: &Path) {
    let result = Command::new(env!("CARGO_BIN_EXE_by"))
        .arg("build")
        .current_dir(dir)
        .output()
        .expect("failed to spawn by");
    assert!(
        result.status.success(),
        "by build failed:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn run_python(python: &str, dir: &Path, args: &[&str]) -> String {
    let ran = Command::new(python)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn python");
    assert!(
        ran.status.success(),
        "python failed:\n{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    String::from_utf8_lossy(&ran.stdout).trim().to_owned()
}

/// a build writes the runtime once per package and its modules call into it
#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn a_built_package_runs_off_one_copy_of_the_runtime() {
    let Some(python) = native_interpreter() else {
        eprintln!("skipping: no interpreter new enough to run the built tree");
        return;
    };
    let dir = runtime_project("shared_runtime_build");
    build_in(&dir);

    let build = dir.join("build");
    assert!(
        build.join("app").join("_by_runtime.py").is_file(),
        "the package carries the runtime"
    );
    assert!(
        !build
            .join("app")
            .join("sub")
            .join("_by_runtime.py")
            .exists(),
        "which its subpackage shares"
    );
    // a distribution ships packages, so a copy at the root would not ship
    assert!(!build.join("_by_runtime.py").exists());

    let one = fs::read_to_string(build.join("app").join("one.py")).unwrap();
    assert!(
        one.contains("from app._by_runtime import"),
        "the module imports what it calls:\n{one}"
    );
    assert!(
        !one.contains("def _checked_cast("),
        "and does not define it as well:\n{one}"
    );
    // a helper reached through the lazy-import proxy is a proxy call on every use
    assert!(
        !one.contains("_lazy_attr(\"app._by_runtime\""),
        "the runtime import is eager:\n{one}"
    );

    assert_eq!(
        run_python(
            &python,
            &build,
            &["-c", "from app.main import main; main()"]
        ),
        "{} {}{} 2"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn by_run_runs_a_package_that_imports_relatively() {
    let Some(python) = native_interpreter() else {
        eprintln!("skipping: no interpreter new enough to run the program");
        return;
    };
    let dir = runtime_project("shared_runtime_run");
    let ran = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["run", "app.main"])
        .env("PYTHON", &python)
        .current_dir(&dir)
        .output()
        .expect("failed to spawn by");
    assert!(
        ran.status.success(),
        "by run failed:\n{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&ran.stdout).trim(), "{} {}{} 2");
}

/// a script in a folder that is no package has no import that works when it is
/// run, since the folder above it is not on the path — so it carries its own
#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn a_script_in_no_package_carries_its_own_helpers() {
    let Some(python) = native_interpreter() else {
        eprintln!("skipping: no interpreter new enough to run the script");
        return;
    };
    let dir = runtime_project("shared_runtime_script");
    build_in(&dir);

    let scripts = dir.join("build").join("scripts");
    let tool = fs::read_to_string(scripts.join("tool.py")).unwrap();
    assert!(
        !tool.contains("_by_runtime"),
        "the script imports no runtime:\n{tool}"
    );
    assert!(!scripts.join("_by_runtime.py").exists());
    assert_eq!(run_python(&python, &dir, &["build/scripts/tool.py"]), "3");
}

/// a copy at the root would be a top-level module, which a second basedpython
/// wheel built by another version overwrites on install — so a module at the
/// module root carries its own helpers too
#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn a_root_module_carries_its_own_helpers() {
    let Some(python) = native_interpreter() else {
        eprintln!("skipping: no interpreter new enough to run the module");
        return;
    };
    let dir = runtime_project("shared_runtime_root_module");
    build_in(&dir);

    let build = dir.join("build");
    let cli = fs::read_to_string(build.join("cli.py")).unwrap();
    assert!(
        !cli.contains("_by_runtime"),
        "the root module imports no runtime:\n{cli}"
    );
    assert!(!build.join("_by_runtime.py").exists());
    assert_eq!(run_python(&python, &build, &["cli.py"]), "4");
}

fn holds_file(dir: &Path, name: &str) -> bool {
    fs::read_dir(dir).unwrap().any(|entry| {
        let path = entry.unwrap().path();
        path.file_name().is_some_and(|file| file == name)
            || (path.is_dir() && holds_file(&path, name))
    })
}

/// transpiling in place writes into the source tree, where a runtime file with
/// no `.by` beside it would read as a module the author wrote
#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn transpiling_a_directory_in_place_writes_no_runtime_file() {
    let Some(python) = native_interpreter() else {
        eprintln!("skipping: no interpreter new enough to run the output");
        return;
    };
    let dir = runtime_project("shared_runtime_in_place");
    let result = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["transpile", "src"])
        .current_dir(&dir)
        .output()
        .expect("failed to spawn by");
    assert!(
        result.status.success(),
        "by transpile failed:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!holds_file(&dir.join("src"), "_by_runtime.py"));
    assert_eq!(
        run_python(
            &python,
            &dir.join("src"),
            &["-c", "from app.main import main; main()"]
        ),
        "{} {}{} 2"
    );
}

/// a re-stage patches one module into a tree an earlier build wrote, so an edit
/// that calls a helper nothing in that build called still has to find it there
#[test]
fn a_restaged_module_finds_a_helper_the_build_never_used() {
    let dir = runtime_project("shared_runtime_restage");
    build_in(&dir);
    let runtime = fs::read_to_string(dir.join("build").join("app").join("_by_runtime.py")).unwrap();
    assert!(runtime.contains("def _try_cast("));

    fs::write(
        dir.join("src").join("app").join("main.by"),
        "from . import one\n\n\ndef check(x: object) -> str | None:\n    return x cast? str\n\n\ndef main():\n    print(one.dumps({}), check(1))\n",
    )
    .unwrap();
    let restaged = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["restage", "build", "src/app/main.by"])
        .current_dir(&dir)
        .output()
        .expect("failed to spawn by");
    let answer = String::from_utf8_lossy(&restaged.stdout);
    assert!(
        answer.contains("_try_cast") && answer.contains("app._by_runtime"),
        "the re-staged module imports the helper from the tree's copy:\n{answer}\n{}",
        String::from_utf8_lossy(&restaged.stderr)
    );
}
