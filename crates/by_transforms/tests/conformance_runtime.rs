//! Runtime divergence test for conformance extensions (`extension str(A):`).
//!
//! The mdtests verify what the *checker* believes about a conformance and the
//! transform unit tests verify the lowered text. This closes the loop: the whole
//! point of a conformance is that a value the interpreter knows nothing about
//! answers an interface at runtime, and only running it proves that. A witness
//! table registered under the wrong key, a dispatcher that forgot its `getattr`
//! fallback, or a registration emitted before the interface's own class
//! statement would all type-check and fail here.
//!
//! No third-party packages are needed, so any `python3` will do; if none is
//! found the test skips rather than fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

/// basedpython whose module-level `assert`s exercise the conformance runtime
/// contract end to end.
const PROGRAM: &str = r#"
protocol Show:
    def show(self) -> str

extension Show:
    def shout(self) -> str:
        return self.show().upper()

extension str(Show):
    override def show(self) -> str:
        return "str:" + self

class Widget:
    def __init__(self, tag: str) -> None:
        self.tag = tag

extension Widget(Show):
    override def show(self) -> str:
        return "widget:" + self.tag

# a class that answers the requirement itself needs no member in its block
class Native:
    def show(self) -> str:
        return "native"

extension Native(Show): ...


def render(value: Show) -> str:
    return value.show()


def render_via_extension(value: Show) -> str:
    return value.shout()


# dispatch through an interface-typed parameter reaches each conformance
assert render("hi") == "str:hi", "a builtin conforms"
assert render(Widget("w")) == "widget:w", "a first-party class conforms"
assert render(Native()) == "native", "a native member answers with no table entry"

# a protocol extension's member is statically dispatched, and its own call of
# the requirement goes back through the table
assert render_via_extension("hi") == "STR:HI", "a default body dispatches"
assert render_via_extension(Widget("w")) == "WIDGET:W", "and for every conformer"

# reached on the concrete type, the same members resolve with no table lookup
assert "hi".show() == "str:hi", "the block's member is inherent on the type"
assert "hi".shout() == "STR:HI", "the protocol's extension reaches the type"

# the value is never wrapped: identity, equality and hashing are untouched
original = Widget("w")
assert render(original) == "widget:w"
assert original.tag == "w", "the conforming value is the value itself"

# an `is`-test answers from the registry, which `isinstance` could not do
def describe(value: object) -> str:
    if value is Show:
        return value.show()
    return "no"

assert describe("hi") == "str:hi", "a conforming builtin tests positive"
assert describe(Widget("w")) == "widget:w", "so does a conforming class"
assert describe(Native()) == "native", "and a native answer"
assert describe(3) == "no", "a value that answers nothing tests negative"

# a subclass's own member beats a conformance registered on its base: one object
# must not answer two ways depending on the static type it is viewed through
class Base: ...

extension Base(Show):
    override def show(self) -> str:
        return "base"

class Derived(Base):
    def show(self) -> str:
        return "derived"

assert render(Derived()) == "derived", "a subclass's own member wins"
assert Derived().show() == "derived", "and the static path agrees"
assert render(Base()) == "base", "the base still uses its conformance"

# conforming to an interface conforms to everything it derives, so a receiver
# typed as a supertype dispatches to the same witness
protocol Loud(Show):
    def yell(self) -> str

class Siren: ...

extension Siren(Loud):
    override def show(self) -> str:
        return "siren"

    override def yell(self) -> str:
        return "SIREN"

assert render(Siren()) == "siren", "a supertype-typed receiver dispatches"

# a `class def` requirement is handed the class, not the instance
protocol Species:
    class def species(cls) -> str

class Cat: ...

extension Cat(Species):
    override class def species(cls) -> str:
        return cls.__name__

def name_of(value: Species) -> str:
    return value.species()

assert name_of(Cat()) == "Cat", "a class-def requirement receives the class"

# a data-member requirement is read rather than called
protocol Named:
    @property
    def name(self) -> str

extension int(Named):
    @property
    override def name(self) -> str:
        return "int-" + str(self)

def label(value: Named) -> str:
    return value.name

assert label(7) == "int-7", "a property requirement reads through the table"

print("ok")
"#;

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn conformances_run_correctly() {
    let Some(python) = python() else {
        eprintln!("skipping conformance runtime test: no `python3` interpreter found");
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
        "transpiled conformance program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
