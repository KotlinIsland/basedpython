//! Runtime test for default re-evaluation composing with sibling lowerings.
//!
//! A non-scalar default is moved into the function body, where it runs once per
//! call. The transform's unit tests assert on the lowered *text*; what actually
//! matters is that the expression still *means* the same thing once it has been
//! relocated — a lowering written inside the default has to arrive with it. A
//! dropped one is silent: `by check` passes, the emitted python parses, and the
//! program returns the wrong answer (`1 is int` is `False` where
//! `isinstance(1, int)` is `True`).
//!
//! No third-party packages are needed, so any `python3` will do; if none is
//! found the test skips rather than fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

/// every lowering that can be written inside a default, checked against the
/// same expression evaluated as ordinary body code
const COMPOSITION: &str = r#"
class Holder:
    def __init__(self) -> None:
        self.inner: int = 5

# `is` lowers to `isinstance`, and spans the whole default — the sentinel
# substitutes that span while the lowering rewrites it
def identity(x = 1 is int):
    return x

assert identity() is True, "an `is` test keeps its lowering"

# `?.` chains the whole operand
def chain(x = Holder()?.inner):
    return x

def chain_absent(h: Holder? = None, x = None?.inner):
    return x

assert chain() == 5, "an optional chain keeps its lowering"
assert chain_absent() is None, "an absent optional chain keeps its lowering"

# `!` is a wrap: an insertion at the first token, a replacement at the last.
# both halves have to relocate together
def unwrap(x = Holder().inner!):
    return x

assert unwrap() == 5, "a force unwrap keeps both halves of its lowering"

# a tuple index is a narrow edit strictly inside the default
def index(x = (7, 8).0):
    return x

assert index() == 7, "a tuple index keeps its lowering"

# re-evaluation itself still holds: the default runs per call, so a fresh list
# each time rather than one shared between calls
def fresh(items = []):
    items.append(1)
    return items

assert fresh() == [1] and fresh() == [1], "the default is re-evaluated per call"

# a scalar default is left in the signature, eagerly bound as python's own
def scalar(x = 3):
    return x

assert scalar() == 3, "a scalar default is untouched"

print("ok")
"#;

/// a `context` argument is resolved at the call site, and a call written in a
/// default is still a call — the implicit argument has to follow it into the body
const CONTEXT: &str = r#"
def need(a: int, context b: str) -> str:
    return f"{a}{b}"

context s = "S"

def in_default(x: str = need(1)) -> str:
    return x

def in_body() -> str:
    return need(1)

assert in_default() == "1S", "a context argument follows the default into the body"
assert in_body() == "1S", "and is unchanged in ordinary body code"

print("ok")
"#;

/// an extension member's backing `def` is hoisted to the module top, and its
/// signature reads the `_MISSING` sentinel the preamble defines — a `def` runs
/// its defaults at definition time, so it has to land *below* the preamble
const EXTENSION: &str = r#"
extension list:
    def tagged(self, xs = []) -> int:
        xs.append(1)
        return len(xs)

assert [1].tagged() == 1, "an extension member with a mutable default is callable"
assert [1].tagged() == 1, "and its default is re-evaluated per call"

print("ok")
"#;

/// the same program behind a UTF-8 BOM. the preamble the transform emits has to
/// splice in *after* it — a BOM is only a BOM at offset 0, and pushed onto the
/// second line it is an ordinary character python refuses to tokenize
const BOM: &str = "\u{feff}\ndef fresh(items = []):\n    items.append(1)\n    return items\n\nassert fresh() == [1], \"the default is re-evaluated per call\"\n\nprint(\"ok\")\n";

/// a default written over several lines inside parentheses, which are not part of its
/// range. moved into the body it still needs a grouping, or its second line is a
/// statement of its own: `+ [2]` runs as a unary plus and the default is just `[1]`
const MULTI_LINE: &str = r#"
def joined(xs: list[int] = (
    [1]
    + [2]
)) -> list[int]:
    return xs

assert joined() == [1, 2], "the whole default is re-evaluated, not its first line"

def keyword(*, xs: list[int] = (
    [1]
    + [2]
)) -> list[int]:
    return xs

assert keyword() == [1, 2], "so is a keyword-only one"

# an `init(...)` with no body of its own writes its guards with the body it makes
class Holder:
    init(self, let xs: list[int] = (
        [1]
        + [2]
    ))

assert Holder().xs == [1, 2], "so is one of a bodyless `init`"

print("ok")
"#;

/// a default that names a value stands in the signature as a marker whose `repr` is the
/// default as written — there is no value to put there, because the call reads the name as
/// it runs. handed back, as `apply_defaults` hands it, the marker means the argument was
/// not given
const NAME_DEFAULT: &str = r#"
import inspect

EARLY = 2

def f(x: int = EARLY) -> int:
    return x

signature = inspect.signature(f)
assert repr(signature.parameters["x"].default) == "<default: EARLY>", repr(signature.parameters["x"].default)
assert str(signature) == "(x: int = <default: EARLY>) -> int", str(signature)

EARLY = 3
bound = signature.bind()
bound.apply_defaults()
assert f(*bound.args, **bound.kwargs) == 3, "the default handed back is read as the call runs"
assert f() == 3, "and so is one left out"

print("ok")
"#;

/// a call is evaluated afresh by every call that leaves its parameter out, and by every
/// call handed the marker
const CALL_DEFAULT: &str = r#"
import inspect

def make() -> list[int]:
    return []

def f(items: list[int] = make()) -> list[int]:
    items.append(1)
    return items

signature = inspect.signature(f)
assert repr(signature.parameters["items"].default) == "<default: make()>", repr(signature.parameters["items"].default)

bound = signature.bind()
bound.apply_defaults()
assert f(*bound.args, **bound.kwargs) == [1], "the marker handed back is a fresh call"
assert f(*bound.args, **bound.kwargs) == [1], "every time"

print("ok")
"#;

/// a marker copied or pickled — a tool that keeps a signature's defaults may do either —
/// comes back as the very marker, and still means the argument was not given
const COPIED_MARKER: &str = r#"
import copy
import inspect
import pickle

def f(items: list[int] = []) -> list[int]:
    items.append(1)
    return items

default = inspect.signature(f).parameters["items"].default
assert copy.copy(default) === default, "a copy is the marker"
assert copy.deepcopy(default) === default, "so is a deep copy"
assert pickle.loads(pickle.dumps(default)) === default, "and a pickled one"
assert f(**{"items": pickle.loads(pickle.dumps(default))}) == [1], "which is the default"

print("ok")
"#;

/// each default has a marker of its own: one handed to another parameter is a value there,
/// as any other object would be
const ANOTHER_DEFAULTS_MARKER: &str = r#"
import inspect

def f(items: list[int] = []) -> list[int]:
    return items

def g(value: object = []) -> object:
    return value

marker = inspect.signature(f).parameters["items"].default
assert g(**{"value": marker}) === marker, "another default's marker is a value"
assert g() == [], "and the parameter's own default still applies"

print("ok")
"#;

/// a marker shows its default on one line, without the comments written in it
const MULTI_LINE_MARKER: &str = r#"
import inspect

def f(xs: list[int] = [
    1,  # one
    2,
], ys: list[int] = (
    [3]
    + [4]
)) -> list[int]:
    return xs + ys

parameters = inspect.signature(f).parameters
assert repr(parameters["xs"].default) == "<default: [1, 2,]>", repr(parameters["xs"].default)
assert repr(parameters["ys"].default) == "<default: [3] + [4]>", repr(parameters["ys"].default)
assert f() == [1, 2, 3, 4]

print("ok")
"#;

/// a default that reads an earlier parameter has no value until the call that supplies
/// that parameter — the marker is all there is to show
const LATE_BOUND_DEFAULT: &str = r#"
import inspect

def f(a: int, b: int = a + 1) -> int:
    return b

signature = inspect.signature(f)
assert repr(signature.parameters["b"].default) == "<default: a + 1>", repr(signature.parameters["b"].default)

bound = signature.bind(5)
bound.apply_defaults()
assert f(*bound.args, **bound.kwargs) == 6, "the default is computed from the argument given"

print("ok")
"#;

/// the constructor an `init(let ...)` declares shows its defaults the same way
const INIT_FIELD_DEFAULT: &str = r#"
import inspect

EARLY = 2

class Base:
    init(let geysers: int = EARLY)

default = inspect.signature(Base.__init__).parameters["geysers"].default
assert repr(default) == "<default: EARLY>", repr(default)

signature = inspect.signature(Base)

bound = signature.bind()
bound.apply_defaults()
assert Base(*bound.args, **bound.kwargs).geysers == 2, "the marker handed back is the default"

EARLY = 5
assert Base(*bound.args, **bound.kwargs).geysers == 5, "read as the call runs"
assert Base().geysers == 5, "and so is one left out"

print("ok")
"#;

/// a parameter python would reject — required, after a defaulted one — stands in the
/// signature with a marker that says it is required, and handed back it is the missing
/// argument it stands for
const REQUIRED_AFTER_DEFAULT: &str = r#"
import inspect

def f(x: int = 1, a: int) -> int:
    return x + a

signature = inspect.signature(f)
assert repr(signature.parameters["a"].default) == "<required>", repr(signature.parameters["a"].default)

bound = signature.bind()
bound.apply_defaults()
try:
    f(*bound.args, **bound.kwargs)
except TypeError as error:
    assert "missing required argument: 'a'" in str(error), str(error)
else:
    raise AssertionError("a required argument handed back as its marker was accepted")

print("ok")
"#;

/// a scalar default is python's own, so the signature holds its value
const SCALAR_DEFAULT: &str = r#"
import inspect

def f(x: int = 3, s: str = "a") -> int:
    return x

parameters = inspect.signature(f).parameters
assert parameters["x"].default == 3, repr(parameters["x"].default)
assert parameters["s"].default == "a", repr(parameters["s"].default)

print("ok")
"#;

fn run(python: &str, program: &str) {
    let config = Config {
        min_version: PythonVersion::PY313,
        ..Config::default()
    };
    let transpiled = transpile(program, &config).expect("transpile should succeed");
    let output = Command::new(python)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

#[test]
fn a_lowering_inside_a_default_survives_the_move_into_the_body() {
    let Some(python) = python() else {
        return;
    };
    run(&python, COMPOSITION);
}

#[test]
fn a_context_argument_inside_a_default_survives_the_move_into_the_body() {
    let Some(python) = python() else {
        return;
    };
    run(&python, CONTEXT);
}

#[test]
fn an_extension_members_default_is_hoisted_below_the_preamble() {
    let Some(python) = python() else {
        return;
    };
    run(&python, EXTENSION);
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn a_default_written_over_several_lines_survives_the_move_into_the_body() {
    let Some(python) = python() else {
        eprintln!(
            "skipping `a_default_written_over_several_lines_survives_the_move_into_the_body`: no `python3` interpreter found"
        );
        return;
    };
    run(&python, MULTI_LINE);
}

/// both preamble paths have to leave the BOM at offset 0: the sentinel the
/// transform emits, and the `from __future__ import annotations` line every
/// target below 3.10 gets
#[test]
fn a_leading_bom_stays_ahead_of_the_preamble() {
    let Some(python) = python() else {
        return;
    };

    for version in [PythonVersion::PY313, PythonVersion::PY39] {
        let config = Config {
            min_version: version,
            ..Config::default()
        };
        let transpiled = transpile(BOM, &config).expect("transpile should succeed");
        assert!(
            transpiled.starts_with('\u{feff}'),
            "the BOM must stay at offset 0 on {version:?}:\n{transpiled}"
        );

        // cpython strips a leading BOM when it reads a source *file*; `-c` does
        // not, so this one has to run from disk to mean anything
        let dir = tempfile::tempdir().expect("failed to create a temp dir");
        let path = dir.path().join("emitted.py");
        std::fs::write(&path, &transpiled).expect("failed to write the module");

        let output = Command::new(&python)
            .arg(&path)
            .output()
            .expect("failed to spawn python");

        assert!(
            output.status.success(),
            "transpiled program failed on {python} for {version:?}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    }
}

#[test]
fn a_name_default_shows_as_written_in_the_signature() {
    let Some(python) = python() else {
        return;
    };
    run(&python, NAME_DEFAULT);
}

#[test]
fn a_call_default_shows_as_written_in_the_signature() {
    let Some(python) = python() else {
        return;
    };
    run(&python, CALL_DEFAULT);
}

#[test]
fn a_late_bound_default_shows_as_written_in_the_signature() {
    let Some(python) = python() else {
        return;
    };
    run(&python, LATE_BOUND_DEFAULT);
}

#[test]
fn an_init_field_default_shows_as_written_in_the_signature() {
    let Some(python) = python() else {
        return;
    };
    run(&python, INIT_FIELD_DEFAULT);
}

#[test]
fn a_required_parameter_after_a_default_shows_as_required_in_the_signature() {
    let Some(python) = python() else {
        return;
    };
    run(&python, REQUIRED_AFTER_DEFAULT);
}

#[test]
fn a_scalar_default_shows_its_value_in_the_signature() {
    let Some(python) = python() else {
        return;
    };
    run(&python, SCALAR_DEFAULT);
}

#[test]
fn a_copied_or_pickled_marker_is_still_the_default() {
    let Some(python) = python() else {
        return;
    };
    run(&python, COPIED_MARKER);
}

#[test]
fn another_defaults_marker_is_a_value() {
    let Some(python) = python() else {
        return;
    };
    run(&python, ANOTHER_DEFAULTS_MARKER);
}

#[test]
fn a_marker_shows_its_default_on_one_line() {
    let Some(python) = python() else {
        return;
    };
    run(&python, MULTI_LINE_MARKER);
}
