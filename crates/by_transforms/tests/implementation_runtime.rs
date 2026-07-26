//! Runtime test for `implementation A for B:` — the half the text-level and
//! type-level tests cannot reach.
//!
//! The unit tests check the *lowered text* and the mdtest checks the *types*;
//! what neither can show is that the witness object actually behaves like the
//! design says at runtime. Every claim exercised here is one a wrong
//! `_by_Implementation` would break while the emitted text still looked right:
//!
//! - a default method body on the interface is inherited by the witness
//! - `self.<member>` inside a block reaches the implemented object's state, and a
//!   write through the witness is visible on the original (shared, not copied)
//! - `super()` in a block member reaches the interface's default
//! - `==` and `hash` delegate, so a witness and its object are interchangeable as
//!   dict keys and in sets
//! - `isinstance(witness, A)` is true while `isinstance(b, A)` is false — the
//!   asymmetry the type checker models
//! - `__implemented__` hands back the real object, and its `type()` is unchanged
//! - an interface's own `__repr__` wins over the delegating one
//! - every conversion site really converts: a `return`, an attribute assignment,
//!   an annotated assignment, and element-wise in a literal and a comprehension

use std::process::{Command, Stdio};

use by_transforms::{Config, PythonVersion, transpile};

/// basedpython whose module-level `assert`s exercise a witness end to end
const PROGRAM: &str = r#"
abstract class Shape:
    abstract def describe(self) -> str:
        return f"shape with area {self.area()}"
    abstract def area(self) -> int: ...

class Rect:
    def __init__(self, w: int, h: int):
        self.w = w
        self.h = h

implementation Shape for Rect as RectAsShape:
    override def area(self) -> int:
        return self.w * self.h

    override def describe(self) -> str:
        # the interface's default body, reached through the witness's own MRO
        return "rect: " + super().describe()

def render(shape: Shape) -> str:
    return shape.describe()

# an interface that defines its own `__repr__`: the witness must leave it alone
abstract class Keyed:
    abstract def key(self) -> int: ...
    def __repr__(self) -> str:
        return f"Keyed({self.key()})"

class Three:
    n: int = 3

implementation Keyed for Three as Keyed3AsKeyed:
    override def key(self) -> int:
        return self.n

r = Rect(3, 4)

# a conversion at the call site, and the inherited default body underneath
assert render(r) == "rect: shape with area 12", render(r)

w = RectAsShape(r)
assert w.area() == 12

# state is shared, not copied: a write through the witness lands on the object
w.w = 5
assert r.w == 5
assert w.area() == 20

# reads forward too, including attributes the witness never declares
assert w.h == 4

# `__implemented__` is the real object, with its own type. `===` is identity;
# a bare `is` would lower to `isinstance` (see identity-swap)
assert w.__implemented__ === r
assert type(w.__implemented__) === Rect

# equality and hashing delegate, so a witness and its object are interchangeable
assert w == r
assert r == w
assert hash(w) == hash(r)
assert {r: "held"}[w] == "held"
assert len({r, w}) == 1

# the asymmetry: the witness is a Shape, the object is not
assert isinstance(w, Shape)
assert not isinstance(r, Shape)

# an interface's own dunder wins over the delegating one: the witness must not
# carry a `__repr__` that shadows what the interface defines
assert repr(Keyed3AsKeyed(Three())) == "Keyed(3)", repr(Keyed3AsKeyed(Three()))

# every conversion site, converting at runtime: a return, an attribute, an
# annotated assignment, and element-wise inside a literal and a comprehension
class Holder:
    field: Shape

def ret() -> Shape:
    return Rect(2, 2)

def areas(shapes: list[Shape]) -> int:
    return sum(s.area() for s in shapes)

assert ret().area() == 4

holder = Holder()
holder.field = Rect(2, 3)
assert holder.field.area() == 6

single: Shape = Rect(3, 3)
assert single.area() == 9

literal: list[Shape] = [Rect(1, 2), Rect(2, 2)]
assert areas(literal) == 6, areas(literal)

sizes = [1, 2, 3]
comprehended: list[Shape] = [Rect(n, n) for n in sizes]
assert areas(comprehended) == 14, areas(comprehended)

mapping: dict[str, Shape] = {"a": Rect(2, 5)}
assert mapping["a"].area() == 10

print("ok")
"#;

/// the first interpreter that accepts `probe`
fn python_supporting(probe: &str) -> Option<String> {
    let mut candidates = Vec::new();
    if let Ok(p) = std::env::var("PYTHON") {
        candidates.push(p);
    }
    candidates.extend(["python3.13", "python3"].map(String::from));

    candidates.into_iter().find(|py| {
        Command::new(py)
            .args(["-c", probe])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
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
fn witness_behaves_at_runtime() {
    let Some(python) = python_supporting("import sys; sys.exit(0)") else {
        eprintln!("skipping implementation runtime test: no python interpreter found");
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
        "transpiled implementation program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
