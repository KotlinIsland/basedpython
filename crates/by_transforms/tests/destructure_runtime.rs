//! Runtime divergence test for the destructuring lowering.
//!
//! The mdtests verify the *types* a destructuring produces and the transform
//! unit tests verify the *lowered text*. This test closes the loop: it
//! transpiles every binding position — the `let` statement, a `for` target, a
//! `with` item, a parameter — plus the `and` pattern, and runs the result on a
//! real interpreter. A binder that bound the wrong value, an `else` block that
//! ran when the pattern matched, or a conjunction that accepted a value only one
//! of its conjuncts matches would fail here even though the type-level tests
//! pass.
//!
//! No third-party packages are needed, so any `python3` will do; if none is
//! found the test skips rather than fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

/// basedpython whose module-level `assert`s exercise the destructuring runtime
/// contract end to end.
const PROGRAM: &str = r#"
from contextlib import contextmanager
from typing import Iterator

class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

# a `let` binds in the enclosing scope, so the captures outlive the statement
def sum_point(p: Point) -> int:
    let Point(x, y) := p
    return x + y

assert sum_point(Point(1, 2)) == 3, "a `let` binds its captures"

# the `else` block runs exactly when the pattern did not match
def describe(v: int | str) -> str:
    let int(n) := v else:
        return "not an int"
    return f"int {n}"

assert describe(3) == "int 3", "the pattern matched"
assert describe("x") == "not an int", "the block runs when it did not"

# a loop destructures each element
totals: list[int] = []
for Point(x, y) in [Point(1, 2), Point(3, 4)]:
    totals.append(x + y)
assert totals == [3, 7], "a `for` target destructures every element"

# a `with` item destructures the value it binds
@contextmanager
def borrow(p: Point) -> Iterator[Point]:
    yield p

with borrow(Point(5, 6)) as Point(x, y):
    assert (x, y) == (5, 6), "a `with` item destructures what it binds"

# a parameter destructures its argument, positionally
def distance(Point(x, y): Point) -> int:
    return x + y

assert distance(Point(7, 8)) == 15, "a parameter destructures its argument"

# a conjunction matches only what every conjunct matches, and binds all of them
def both(v: object) -> str:
    if let Point(x, y) and object() := v:
        return f"point {x} {y}"
    return "no"

assert both(Point(1, 1)) == "point 1 1", "every conjunct matched"
assert both(3) == "no", "a conjunct that does not match fails the conjunction"

# a conjunction nested inside another pattern is hoisted, and still only matches
# what every conjunct matches
def origin_only(p: Point) -> bool:
    if let Point(x=int() and 0, y=y) := p:
        return True
    return False

assert origin_only(Point(0, 9)), "the hoisted conjunction matched"
assert not origin_only(Point(1, 9)), "the hoisted conjunction did not match"

# a `match` with a conjunction still falls through to the next case
def classify(v: object) -> str:
    match v:
        case int() and 1:
            return "one"
        case int():
            return "int"
        case _:
            return "other"

assert classify(1) == "one", "the first case matched"
assert classify(2) == "int", "a failed conjunction falls through"
assert classify("x") == "other", "the wildcard case is reached"

# a case guard runs once the whole pattern has matched
def guarded(v: object, flag: bool) -> str:
    match v:
        case int() and 1 if flag:
            return "one"
        case _:
            return "other"

assert guarded(1, True) == "one", "the guard held"
assert guarded(1, False) == "other", "a failed guard falls through"

# the machinery a destructuring generates is never left behind — a stray name in a
# class body is an attribute, and in an `enum class` body an outright bogus variant
class Holder:
    p: Point = Point(1, 2)
    let Point(x, y) := p
    let Point(a, b) and object() := p
    let Point(x=int() and 1, y=c) := p

assert Holder.x == 1, "a class-body `let` still binds"
assert Holder.a == 1, "so does one with a conjunction"
assert Holder.c == 2, "so does one with a hoisted conjunction"
assert not [name for name in vars(Holder) if name.startswith("_by_")], (
    "no machinery survives as a class attribute"
)
assert not [name for name in dict(globals()) if name.startswith("_by_let")], (
    "no selector survives at module level"
)

print("ok")
"#;

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn destructuring_runs_correctly() {
    let Some(python) = python() else {
        eprintln!("skipping destructuring runtime test: no `python3` interpreter found");
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
        "transpiled destructuring program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
