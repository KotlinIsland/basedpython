//! Runtime divergence test for `if let` lowering.
//!
//! The mdtests verify the *types* an `if let` clause produces (captures, subject
//! narrowing) and the transform unit tests verify the *lowered text*. This test
//! closes the loop: it transpiles chains that exercise the runtime contract —
//! clause selection, capture bindings leaking into the enclosing scope, lazy
//! evaluation of an `elif let` subject, and `else` reachability — and runs them
//! on a real interpreter. A selector that let two clauses run, or an eagerly
//! evaluated subject, would fail here even though the type-level tests pass.
//!
//! No third-party packages are needed, so the program is transpiled for whichever
//! interpreter runs it; if none is found the test skips, and says so, rather than
//! fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
mod interpreters;
use common::python;
use interpreters::Interpreter;

/// basedpython whose module-level `assert`s exercise the `if let` runtime
/// contract end to end.
const PROGRAM: &str = r#"
def describe(v: int | str | None) -> str:
    if let int(n) := v:
        return f"int {n}"
    elif let str(s) := v:
        return f"str {s}"
    else:
        return "none"

assert describe(3) == "int 3", "first clause"
assert describe("hi") == "str hi", "second clause"
assert describe(None) == "none", "else clause"

# a capture is bound in the enclosing scope, so it outlives the clause
pair: tuple[int, int] | None = (1, 2)
if let (a, b) := pair:
    total = a + b
assert total == 3, "captures bind in the enclosing scope"

# an `elif let` subject is only evaluated once every earlier clause has failed
calls: list[str] = []

def probe(tag: str, value: int | None) -> int | None:
    calls.append(tag)
    return value

if let int(first) := probe("first", 1):
    taken = first
elif let int(second) := probe("second", 2):
    taken = second
assert taken == 1, "the first matching clause wins"
assert calls == ["first"], "a later subject is not evaluated"

# no clause matching leaves every body unrun
ran = False
if let str(_unused) := 3:
    ran = True
assert not ran, "a failed match runs nothing"

# a chain in a class body leaves no machinery behind — a stray assignment there
# would be a class attribute, and in an `enum class` body an outright variant
class Holder:
    field: int | str = 1
    if let int(n) := field:
        doubled = n * 2

assert Holder.doubled == 2, "a class-body chain still runs"
assert not [name for name in vars(Holder) if name.startswith("_by_if_let")], (
    "the selector does not survive as a class attribute"
)
assert not [name for name in dict(globals()) if name.startswith("_by_if_let")], (
    "the selector does not survive at module level"
)

print("ok")
"#;

/// a subject and a condition written over several lines inside parentheses, which
/// are not part of their ranges. the lowering moves each into a statement header it
/// writes, where it still needs a grouping
const MULTI_LINE: &str = r#"
class Box:
    def __init__(self, value: int | None):
        self.value = value

def unboxed(b: Box, fallback: bool) -> int:
    if let int(x) := (
        b
        .value
    ):
        return x
    elif (
        fallback
        and b.value is None
    ):
        return -1
    return 0

assert unboxed(Box(3), False) == 3, "a subject over several lines"
assert unboxed(Box(None), True) == -1, "a condition over several lines"
assert unboxed(Box(None), False) == 0

print("ok")
"#;

fn run(python: &Interpreter, program: &str) {
    let config = Config {
        min_version: python.version,
        ..Config::default()
    };
    let transpiled = transpile(program, &config).expect("transpile should succeed");

    let output = Command::new(&python.command)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled `if let` program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

#[test]
fn if_let_chains_run_correctly() {
    let Some(python) = python(PythonVersion::PY39) else {
        return;
    };

    run(&python, PROGRAM);
}

#[test]
fn a_subject_written_over_several_lines_keeps_its_grouping() {
    let Some(python) = python(PythonVersion::PY39) else {
        return;
    };
    run(&python, MULTI_LINE);
}
