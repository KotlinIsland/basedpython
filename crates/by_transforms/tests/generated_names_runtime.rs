//! Runtime test for the temporaries the lowerings leave in a namespace.
//!
//! A lowering fires wherever its construct was written, and a class body is one
//! of those places. Python's `enum` turns every ordinary name assigned in a
//! class body into a member and records it the moment it is assigned, so an
//! ordinary temporary there is either a bogus variant or an outright
//! `TypeError` at import — and `del` cannot take it back. Every temporary is
//! therefore a dunder (see `source_util::temporary_name`), which `enum` skips
//! and name mangling leaves alone.
//!
//! The transform unit tests pin the *spelling* of each temporary. This test
//! closes the loop: it writes one of every lowering that mints a temporary into
//! a plain class body and into an `enum class` body, and runs the result. A
//! rename that made a temporary ordinary again would fail here even though the
//! type-level tests pass.
//!
//! No third-party packages are needed, so any `python3` will do; if none is
//! found the test skips rather than fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

/// basedpython whose module-level `assert`s exercise every lowering that mints
/// a temporary, in both namespaces that record what is assigned in them.
const PROGRAM: &str = r#"
from contextlib import contextmanager
from typing import Iterator

class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

def origin() -> Point:
    return Point(0, 0)

def one_point() -> list[Point]:
    return [Point(1, 2)]

@contextmanager
def borrow(p: Point) -> Iterator[Point]:
    yield p

def maybe() -> int | None:
    return 7

seen: list[int] = []

def machinery(names: object) -> list[str]:
    return sorted(n for n in names if "_by_" in n or n == "_t")

# a walrus has no statement to hang a `del` on, so a temporary can outlive its
# lowering. What it may never be is a name the surrounding namespace *records* —
# an ordinary one in an `enum` body is a variant, and one with two leading
# underscores and no trailing pair is name-mangled onto the class
def recorded(names: object) -> list[str]:
    return [n for n in machinery(names) if not (n.startswith("__") and n.endswith("__"))]

# a plain class body: every temporary would be a class attribute
class Holder:
    let Point(hx, hy) := origin() else:
        raise TypeError

    if let Point(ix, iy) := origin():
        pass

    match origin():
        case Point(mx, my) and object():
            pass

    if let Point(x=int() and 0, y=ay) := origin():
        pass

    for Point(fx, fy) in one_point():
        pass

    with borrow(origin()) as Point(wx, wy):
        pass

    seen.append(maybe() ?? 0)

    if True:
        chosen = if True:
            1
        else:
            2

assert (Holder.hx, Holder.hy) == (0, 0), "a class-body `let` binds its captures"
assert (Holder.ix, Holder.iy) == (0, 0), "so does an `if let`"
assert (Holder.mx, Holder.my) == (0, 0), "so does a `match` with a conjunction"
assert Holder.ay == 0, "so does a hoisted conjunction"
assert (Holder.fx, Holder.fy) == (1, 2), "so does a `for` pattern"
assert (Holder.wx, Holder.wy) == (0, 0), "so does a `with` pattern"
assert Holder.chosen == 1, "a class-body statement expression still yields"
assert seen == [7], "a class-body `??` still evaluates"
assert not recorded(vars(Holder)), "no temporary is an ordinary class attribute"

# an `enum class` body: every temporary would be a *member*, and `enum` records
# the name on assignment, so a `del` afterwards cannot take it back
enum class Colour:
    case Red, Green

    let Point(ex, ey) := origin() else:
        raise TypeError

    if let Point(bx, by) := origin():
        pass

    match origin():
        case Point(cx, cy) and object():
            pass

    if let Point(x=int() and 0, y=ny) := origin():
        pass

    for Point(gx, gy) in one_point():
        pass

    with borrow(origin()) as Point(vx, vy):
        pass

    seen.append(maybe() ?? 0)

    if True:
        picked = if True:
            1
        else:
            2

assert Colour.Red.name == "Red" and Colour.Green.name == "Green", (
    "the declared variants are still the enum's own"
)
assert not recorded(vars(Colour)), "no temporary is an ordinary name in the enum"
assert not machinery(Colour.__members__), "and none of them became a member"
assert not machinery(n.name for n in Colour), "or a variant the enum iterates"

print("ok")
"#;

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn generated_names_stay_out_of_the_namespace() {
    let Some(python) = python() else {
        eprintln!("skipping generated-name runtime test: no `python3` interpreter found");
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
        "transpiled program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
