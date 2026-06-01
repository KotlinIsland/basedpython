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
    // `[T: constraints (int, str)]` (invalid python) into the output; on the
    // 3.10 polyfill path the params become constrained `TypeVar`s and the
    // variant field annotations are renamed to match
    let out = transpile(
        "\
enum class Box[T: constraints (int, str)]:
    case Full(T)
    case Empty
",
    );
    assert!(
        !out.contains("constraints ("),
        "constraints leaked into output\n{out}"
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
    x: int & str = compute(n)
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
        stderr.contains("line 2, in deeper") && stderr.contains("x: int & str = compute(n)"),
        "deeper frame should show the original intersection syntax at .by line 2:\n{stderr}"
    );
    assert!(
        stderr.contains("ZeroDivisionError"),
        "exception type should be preserved:\n{stderr}"
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
