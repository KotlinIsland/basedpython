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
//! A conformance written in another module than its interface is run too, transpiled
//! through a project database as `by build` transpiles it, since the lowering reads the
//! other module's types.
//!
//! Each program is transpiled for whichever interpreter runs it; if none is found the
//! test skips, and says so, rather than fails. The cross-module programs write
//! `override`, which python has from 3.12, so an older interpreter needs
//! `typing_extensions`.

use std::fmt::Write as _;
use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile, transpile_typed};
use ruff_db::files::system_path_to_file;
use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
use ty_project::{ProjectMetadata, TestDb};

mod common;
mod interpreters;
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
fn conformances_run_correctly() {
    let Some(python) = python(PythonVersion::PY39) else {
        return;
    };

    let config = Config {
        min_version: python.version,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");

    let output = Command::new(&python.command)
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

/// a project database over `files`, as the language server or `by build` would hold
fn project_db(files: &[(&str, &str)]) -> TestDb {
    let mut db = TestDb::new(ProjectMetadata::new(
        ruff_python_ast::name::Name::new_static(""),
        SystemPathBuf::from("/"),
    ));
    db.init_program().expect("program init failed");
    for (path, source) in files {
        db.write_file(path, source).expect("write file failed");
    }
    db
}

/// transpile every module of `files` for the interpreter that runs it, run `main`, and assert
/// it prints `expected`
///
/// a conformance registers in one module and is read in another, so only executing the modules
/// together proves the two halves meet. two tests asserting each module's text once passed while
/// the program they described raised `AttributeError`
fn run_project(files: &[(&str, &str)], expected: &str) {
    // an `override` is `typing_extensions`' before 3.12
    let Some(python) = common::python_with(
        PythonVersion::PY39,
        "import sys\nif sys.version_info < (3, 12): import typing_extensions",
        "with `typing_extensions` before 3.12",
    ) else {
        return;
    };
    let config = Config {
        min_version: python.version,
        ..Config::default()
    };
    let db = project_db(files);
    // a second database over the same files, for a pre-pass that rewrites a module's source
    let rebuild = || Some(Box::new(project_db(files)) as Box<dyn ty_python_semantic::Db>);
    let dir = tempfile::Builder::new()
        .prefix("conformance")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create case dir");
    let mut transpiled = String::new();
    for (path, _) in files {
        let file = system_path_to_file(&db, path).expect("file not in db");
        let out = transpile_typed(&db, file, &config, Some(&rebuild))
            .unwrap_or_else(|error| panic!("transpile of {path} should succeed: {error}"));
        let relative = path.trim_start_matches('/').replace(".by", ".py");
        writeln!(transpiled, "--- {relative} ---\n{out}").expect("write to a string");
        std::fs::write(dir.path().join(relative), out).expect("write module");
    }
    let output = Command::new(&python.command)
        .arg("main.py")
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn python");
    assert!(
        output.status.success(),
        "transpiled project failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
}

/// the interface comes from one module, the conformance from another, and the consumer sees
/// both only through its imports
#[test]
fn an_imported_conformance_dispatches() {
    run_project(
        &[
            ("/iface.by", "protocol Show:\n    def show(self) -> str\n"),
            (
                "/adapters.by",
                "from iface import Show\n\nextension str(Show):\n    override def show(self) -> str:\n        return \"OWN:\" + self\n",
            ),
            (
                "/main.by",
                "import adapters\nfrom iface import Show\n\ndef render(value: Show) -> str:\n    return value.show()\n\nprint(render(\"hi\"))\n",
            ),
        ],
        "OWN:hi",
    );
}

/// the module that declares the protocol-typed function is upstream of the conformance by
/// construction, so it can never see one. it still has to emit a dispatch
#[test]
fn a_conformance_declared_downstream_of_its_use_dispatches() {
    run_project(
        &[
            (
                "/lib.by",
                "protocol Show:\n    def show(self) -> str\n\ndef render(value: Show) -> str:\n    return value.show()\n",
            ),
            (
                "/main.by",
                "from lib import Show, render\n\nextension str(Show):\n    override def show(self) -> str:\n        return \"OWN:\" + self\n\nprint(render(\"hi\"))\n",
            ),
        ],
        "OWN:hi",
    );
}
