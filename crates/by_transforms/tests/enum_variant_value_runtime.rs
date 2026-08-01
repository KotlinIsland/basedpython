//! Runtime test for the value semantics of a based-enum unit variant.
//!
//! `case Point` in a payload-bearing enum lowers to a module-level subclass
//! attached back as `Shape.Point` — but as the *instance*, so the class it was
//! built from is reachable under no name at all. `copy` and `pickle` both
//! resolve an object through its `__qualname__`, so without a `__reduce__` the
//! singleton silently becomes a second object (`copy`) or refuses to serialize
//! (`pickle`). The same surface syntax in an all-unit enum lowers to a real
//! `enum.Enum`, which gets all three right, so the two lowerings have to agree.

mod common;

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

/// a unit variant is a *value*: every round trip that claims to reproduce it
/// must hand back the same object, and the all-unit `Enum` lowering next to it
/// is the reference for what that means
const PROGRAM: &str = r#"
import copy
import pickle


enum class Shape:
    case Circle(radius: float)
    case Point


enum class Colour:
    case Red, Green


private enum class Hidden:
    case Wrapped(n: int)
    case Nothing


def main() -> None:
    p = Shape.Point
    assert copy.copy(p) === p, "copy"
    assert copy.deepcopy(p) === p, "deepcopy"
    assert pickle.loads(pickle.dumps(p)) === p, "pickle"

    r = Colour.Red
    assert copy.copy(r) === r, "enum copy"
    assert copy.deepcopy(r) === r, "enum deepcopy"
    assert pickle.loads(pickle.dumps(r)) === r, "enum pickle"

    c = Shape.Circle(2.0)
    assert pickle.loads(pickle.dumps(c)) == c, "payload pickle"

    # a `private enum class` is renamed to `_Hidden`, so a variant path spelled
    # from the surface name would resolve to nothing
    assert pickle.loads(pickle.dumps(Hidden.Wrapped(1))) == Hidden.Wrapped(1), "private payload"
    assert pickle.loads(pickle.dumps(Hidden.Nothing)) === Hidden.Nothing, "private unit"

    print("ok")
"#;

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn enum_unit_variant_round_trips_to_itself() {
    let Some(python) = common::python() else {
        eprintln!("skipping enum variant value runtime test: no interpreter found");
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
        "transpiled enum variant program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
