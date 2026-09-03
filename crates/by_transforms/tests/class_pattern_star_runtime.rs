//! Runtime divergence test for the starred wildcard in a class pattern.
//!
//! The unit tests verify the *text* `case A(x, *_, y)` lowers to and the mdtests
//! verify the types the captures get. Both would still pass if the fill counted
//! from the wrong end — `case Line(a, *_, b)` lowering to `Line(a, _, _, b)`
//! reads the same as `Line(b, _, _, a)` to a test that only compares shapes. So
//! this one runs the lowered python and asserts on the values, which are
//! different for every position.
//!
//! It also pins the two claims the lowering rests on: that a subpattern after
//! the star reads the last of `__match_args__` whatever its length, and that a
//! trailing star matches exactly what leaving it out matches.
//!
//! No third-party packages are needed, so any `python3` will do; if none is
//! found the test skips rather than fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

/// basedpython whose module-level `assert`s exercise the runtime contract of a
/// class pattern's starred wildcard.
const PROGRAM: &str = r#"
from dataclasses import dataclass


@dataclass
class Line:
    start: int
    mid: int
    stop: int
    end: int


line = Line(0, 1, 2, 3)


def ends(value: Line) -> tuple[int, int]:
    match value:
        case Line(a, *_, b):
            return (a, b)
    return (-1, -1)


assert ends(line) == (0, 3), "the star skips the middle, not the ends"


def last(value: Line) -> int:
    match value:
        case Line(*_, b):
            return b
    return -1


assert last(line) == 3, "a leading star counts every position back"


def first(value: Line) -> int:
    match value:
        case Line(a, *_):
            return a
    return -1


assert first(line) == 0, "a trailing star claims no position of its own"


# the same pattern shape against a class with fewer positions fills in fewer
# wildcards, which is the whole point of counting from the end
@dataclass
class Short:
    start: int
    end: int


def short_ends(value: Short) -> tuple[int, int]:
    match value:
        case Short(a, *_, b):
            return (a, b)
    return (-1, -1)


assert short_ends(Short(7, 8)) == (7, 8), "the fill follows the class, not the pattern"


# a lone star matches any instance of the class and nothing else
def is_line(value: object) -> bool:
    match value:
        case Line(*_):
            return True
    return False


assert is_line(line), "a lone star matches the class"
assert not is_line(Short(7, 8)), "and only the class"


# keywords still read the names they spell, whatever the star did to the
# positions before them
def start_and_stop(value: Line) -> tuple[int, int]:
    match value:
        case Line(a, *_, stop=s):
            return (a, s)
    return (-1, -1)


assert start_and_stop(line) == (0, 2), "a keyword after the star is unaffected"

print("ok")
"#;

fn run(python: &str, min_version: PythonVersion) {
    let config = Config {
        min_version,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");

    let output = Command::new(python)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled starred class pattern failed on {python} at {min_version}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn starred_class_patterns_read_the_positions_they_name() {
    let Some(python) = python() else {
        eprintln!("skipping starred class pattern runtime test: no `python3` interpreter found");
        return;
    };
    run(&python, PythonVersion::PY313);
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn starred_class_patterns_run_under_the_match_polyfill() {
    // below 3.10 the emitted `match` is itself rewritten into an `if` chain, so
    // the filled-in wildcards are read by the polyfill's `__match_args__` helper
    // rather than by python's own `MATCH_CLASS`
    let Some(python) = python() else {
        eprintln!("skipping starred class pattern polyfill test: no `python3` interpreter found");
        return;
    };
    run(&python, PythonVersion::PY39);
}
