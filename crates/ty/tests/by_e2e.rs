use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

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
    let dir = std::env::temp_dir().join("by_cli_only_named");
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

    let out = dir.join("out");
    let result = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["compile", "wanted.py", "-o"])
        .arg(&out)
        .arg("--emit-c-only")
        .current_dir(&dir)
        .output()
        .expect("failed to spawn by");
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
    let dir = std::env::temp_dir().join("by_cli_package_tree");
    write_package_project(&dir);

    let out = dir.join("o");
    let result = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["compile", "-o"])
        .arg(&out)
        .arg("--emit-c-only")
        .current_dir(&dir)
        .output()
        .expect("failed to spawn by");
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
    let dir = std::env::temp_dir().join("by_cli_artifact_clash");
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
        .args(["compile", "-o"])
        .arg(&out)
        .arg("--emit-c-only")
        .current_dir(&dir)
        .output()
        .expect("failed to spawn by");
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
fn compile_transpiles_the_fallback_with_the_lowering_options_it_was_given() {
    // a declined function *runs* from the embedded source, so `by compile` has to
    // transpile it with the same options a `by transpile` would use. the library
    // has always taken them; until this reached the cli there was no way to say so,
    // and every compile silently used the defaults
    //
    // `except*` has no lowering, so this declines and the fallback is what runs
    let source = "\
def total(s: str, n: int) -> int:
    try:
        pass
    except* ValueError:
        pass
    return len(s) + n
";
    let dir = std::env::temp_dir().join("by_cli_soundness");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("sound.by");
    std::fs::write(&file, source).unwrap();

    let emitted = |spec: &str| -> String {
        let out = dir.join(spec);
        let status = Command::new(env!("CARGO_BIN_EXE_by"))
            .args(["compile"])
            .arg(&file)
            .arg("-o")
            .arg(&out)
            .args(["--emit-c-only", "--soundness", spec])
            .current_dir(&dir)
            .output()
            .expect("failed to spawn by");
        assert!(
            status.status.success(),
            "by exited with error:\n{}",
            String::from_utf8_lossy(&status.stderr)
        );
        std::fs::read_to_string(out.join("sound.c")).expect("the C is readable")
    };

    assert!(
        emitted("all").contains("_soundness_check"),
        "`all` puts the entry checks in the fallback"
    );
    assert!(
        !emitted("none").contains("_soundness_check"),
        "`none` leaves them out, so the flag is what made the difference"
    );
}

#[test]
fn run_executes_module() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "print('hello from by run')\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
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

#[test]
fn run_force_unwrap_yields_inner_value() {
    // `Some(x)` lowers to the `Optional(x)` wrapper; force-unwrapping it must
    // yield the inner value, not the wrapper object
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("main.by"), "x = Some(5)\nprint(x! + 1)\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
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
    return n // 0

def main() -> None:
    deeper(5)

main()
",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
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
        fs::read_to_string(dir.path().join("out/good.py")).unwrap(),
        "x = 1\n"
    );
    assert!(
        !dir.path().join("out/bad.py").exists(),
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
        !dir.path().join("out").join("bad.py").exists(),
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
    assert!(dir.path().join("out").join("main.py").exists());
    assert!(
        !dir.path().join("out").join(".claude").exists(),
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
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    assert_eq!(
        fs::read_to_string(dir.path().join("out").join("my_lib").join("by.typed")).unwrap(),
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
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    let marker = dir.path().join("out").join("my_lib").join("by.typed");
    assert!(marker.exists(), "expected out/my_lib/by.typed:\n{stderr}");
    assert_eq!(fs::read_to_string(marker).unwrap(), "");
}

/// a src-layout project's `src/pkg/main.by` is the module `pkg.main`, so the
/// emitted tree has to be rooted at `src` — mirroring the directory instead
/// emits `out/src/pkg/main.py`, whose module is `src.pkg.main`, a name nothing
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
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    let out = dir.path().join("out");
    assert!(
        out.join("package_name").join("main.py").exists(),
        "expected out/package_name/main.py:\n{stderr}"
    );
    assert!(
        !out.join("src").exists(),
        "the source root must not appear in the output tree:\n{stderr}"
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
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "by build failed:\n{stderr}");
    let emitted = fs::read_to_string(dir.path().join("out/main.py")).unwrap();
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
        .arg("build")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn by");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        dir.path().join("out/good.py").exists(),
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
    assert!(dir.path().join("out/main.py").exists());
    assert!(!dir.path().join("out/tests").exists());
}

#[test]
fn transpile_proceeds_past_non_syntax_errors() {
    // type errors are surfaced as diagnostics but don't block transpile —
    // many basedpython type forms look like type errors to ty
    let dir = tempfile::tempdir().expect("tempdir");
    let by_path = dir.path().join("typed.by");
    fs::write(&by_path, "x: int = \"string\"\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_by"))
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
        .arg("build")
        .current_dir(root)
        .output()
        .expect("failed to spawn by");
    assert!(
        output.status.success(),
        "build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let built = fs::read_to_string(root.join("out/pkg/models.py")).unwrap();
    assert!(
        built.contains("x if x is not None else 0"),
        "coalesce lowered back to python:\n{built}"
    );
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
    def __getitem__(self, i): ...
    def __len__(self): ...

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

/// The `override-raise` strictness option is off unless asked for.
///
/// mdtest force-enables every rule, including default-ignored ones, so the
/// default posture can only be pinned from outside it.
#[test]
fn override_raise_is_off_by_default() {
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
            .arg("check")
            .current_dir(dir.path())
            .output()
            .expect("failed to spawn by")
    };

    let output = check();
    assert!(
        output.status.success(),
        "expected no diagnostic by default:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );

    fs::write(
        dir.path().join("pyproject.toml"),
        "[tool.ty.rules]\noverride-raise = \"error\"\n",
    )
    .unwrap();

    let output = check();
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(
        rendered.contains("override-raise")
            && rendered.contains("which the method it overrides cannot"),
        "expected the override to be reported once enabled:\n{rendered}"
    );
}
