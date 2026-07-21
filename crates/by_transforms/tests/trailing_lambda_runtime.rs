//! Runtime divergence test for trailing-lambda block lowering.
//!
//! The mdtest checker verifies the *types* a trailing block produces (narrowing,
//! `once` write-back) and the transform unit tests verify the *lowered text*.
//! This test closes the loop: it transpiles blocks that exercise the runtime
//! contract — `once` write-through via `nonlocal` / `global`, a fresh binding
//! surviving the boundary through its pre-init, the `once`-return value cell, and
//! a keyword-only `once` callback — and runs them on a real interpreter. A
//! `nonlocal` `SyntaxError`, a wrong-scope write, or a broken return cell would
//! fail here even though the type-level tests pass.
//!
//! No third-party packages are needed, so any `python3` will do; if none is
//! found the test skips rather than fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

/// basedpython whose module-level `assert`s exercise the trailing-lambda runtime
/// contract end to end.
const PROGRAM: &str = r#"
def with_resource(once fn: (int) -> None):
    fn(42)

def maybe(fn: () -> None):
    fn()

def kw(items: list[int], *, once fn: (int) -> None):
    fn(items[0])

# a `once` block writes through to an enclosing binding and a fresh binding it
# creates survives the block (both need the lowering's `nonlocal` + pre-init)
def writes_through() -> int:
    total: int = 1
    with_resource:
        total = it
        doubled = it * 2
    return total + doubled

assert writes_through() == 126, "once write-through + fresh binding survives"

# a `return` inside a `once` block returns from the enclosing function (carried
# out through the value cell), so the trailing `return -1` never runs
def early() -> int:
    with_resource:
        return it + 1
    return -1

assert early() == 43, "once-return targets the enclosing function"

# a module-level block writes through with `global`
counter: int = 0
maybe:
    counter = 5
assert counter == 5, "module-level write-through"

# a keyword-only `once` callback binds and writes through correctly
def via_kw() -> int:
    seen: int = 0
    kw([7]):
        seen = it
    return seen

assert via_kw() == 7, "keyword-only once callback write-through"

print("ok")
"#;

/// Locate a usable interpreter: `$PYTHON` first, then common names. Returns
/// `None` (test skips) when none is found.
fn python() -> Option<String> {
    let mut candidates = Vec::new();
    if let Ok(p) = std::env::var("PYTHON") {
        candidates.push(p);
    }
    candidates.extend(["python3.13", "python3", "python"].map(String::from));

    candidates.into_iter().find(|py| {
        Command::new(py)
            .args(["-c", ""])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn trailing_lambda_blocks_run_correctly() {
    let Some(python) = python() else {
        eprintln!("skipping trailing-lambda runtime test: no `python3` interpreter found");
        return;
    };

    let config = Config {
        min_version: PythonVersion::PY313,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");

    let output = Command::new(&python)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled trailing-lambda program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
