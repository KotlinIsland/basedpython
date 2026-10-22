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
//! No third-party packages are needed, so the program is transpiled for whichever
//! interpreter runs it; if none is found the test skips, and says so, rather than
//! fails. A destructuring is written as a `match` statement, so the program needs
//! python 3.10, and an older target is refused

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
mod interpreters;
use common::python;
use interpreters::Interpreter;

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
def one_point() -> list[Point]:
    return [Point(1, 2)]

class Holder:
    p: Point = Point(1, 2)
    let Point(x, y) := p
    let Point(a, b) and object() := p
    let Point(x=int() and 1, y=c) := p

    for Point(fx, fy) in one_point():
        pass

    with borrow(p) as Point(wx, wy):
        pass

assert Holder.x == 1, "a class-body `let` still binds"
assert Holder.a == 1, "so does one with a conjunction"
assert Holder.c == 2, "so does one with a hoisted conjunction"
assert (Holder.fx, Holder.fy) == (1, 2), "so does a class-body `for` pattern"
assert (Holder.wx, Holder.wy) == (1, 2), "so does a class-body `with` pattern"
assert not [name for name in vars(Holder) if "_by_" in name], (
    "no machinery survives as a class attribute"
)
assert not [name for name in dict(globals()) if "_by_let" in name], (
    "no selector survives at module level"
)

# an `enum class` body turns every ordinary name assigned in it into a member, so
# a binder left there is a variant of the enum
enum class Colour:
    case Red, Green

    for Point(ex, ey) in one_point():
        pass

assert [member.name for member in Colour] == ["Red", "Green"], (
    "no binder became an enum member"
)
assert not [name for name in vars(Colour) if "_by_" in name], (
    "no machinery survives in the enum's namespace"
)

print("ok")
"#;

/// subjects and guards written over several lines inside parentheses, which are not
/// part of their ranges. the lowering moves each onto a line of a statement it writes —
/// an assignment, a `match` header, a `case` guard — where it still needs a grouping
const MULTI_LINE: &str = r#"
class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

def make() -> Point:
    return Point(1, 2)

def summed() -> int:
    let Point(x, y) := (
        make
        ()
    )
    return x + y

assert summed() == 3, "a `let` subject over several lines"

def classify(p: Point, q: int) -> str:
    match (
        p if q >= 0
        else Point(0, 0)
    ):
        case Point(x, y) and object() if (
            x == 1
            and q > 0
        ):
            return "first"
        case _:
            return "other"

assert classify(Point(1, 2), 1) == "first", "a guard over several lines"
assert classify(Point(1, 2), 0) == "other"
assert classify(Point(0, 2), 1) == "other"

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
        "transpiled destructuring program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

#[test]
fn destructuring_runs_correctly() {
    let Some(python) = python(PythonVersion::PY310) else {
        return;
    };

    run(&python, PROGRAM);
}

#[test]
fn a_subject_written_over_several_lines_keeps_its_grouping() {
    let Some(python) = python(PythonVersion::PY310) else {
        return;
    };
    run(&python, MULTI_LINE);
}

/// every binding position, each on its own, refused on a target without `match`
#[test]
fn destructuring_is_refused_below_3_10() {
    let forms = [
        "let (a, b) := (1, 2)\n",
        "for int(n) in [1]:\n    pass\n",
        "with open('f') as int(n):\n    pass\n",
        "def f(int(n): int):\n    pass\n",
        "class C:\n    let (a, b) := (1, 2)\n",
    ];
    for min_version in [PythonVersion::PY38, PythonVersion::PY39] {
        for form in forms {
            let config = Config {
                min_version,
                ..Config::default()
            };
            let error = transpile(form, &config).expect_err(&format!("{form:?} on {min_version}"));
            assert!(
                error.starts_with(&format!(
                    "Cannot use a destructuring pattern, which is written as a `match` statement, \
                     on Python {min_version} (syntax was added in Python 3.10)"
                )),
                "{form:?} on {min_version}: {error}"
            );
        }
    }
}
