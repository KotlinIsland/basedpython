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
