//! Runtime test for anonymous-named-tuple field types.
//!
//! The synthesized `NamedTuple` class is hoisted out of the statement its field
//! types were written in, so no sibling transform's edit reaches them — a field
//! type has to be lowered where the class body is built. When it is not, a
//! `dynamic` field reaches the output verbatim and the class raises `NameError`
//! the moment the module is imported, with `by check` clean and `by transpile`
//! exiting 0. That is exactly what an mdtest cannot see, so it is checked here.

mod common;
mod interpreters;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use by_transforms::{Config, PythonVersion, SoundnessPositions, transpile, transpile_typed};
use ruff_db::files::system_path_to_file;
use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
use ty_project::{ProjectMetadata, TestDb};

/// every field type here is basedpython surface syntax that has no python
/// spelling, so an unlowered one is either a `NameError` at class construction
/// or a syntax error. each also lowers to something the preamble binds above
/// the hoisted class — a field type naming a *later* module binding is a
/// separate, pre-existing gap
const PROGRAM: &str = r#"
def use(p: (m: dynamic, n: int?, o: (int) -> str)) -> None:
    assert p.m == 1, "m"
    assert p.n is None, "n"
    assert p.o(1) == "1", "o"

v: (m: dynamic, n: int?, o: (int) -> str) = (1, None, str)
use(v)
print("ok")
"#;

/// the checker types a name bound to an anonymous named tuple as that tuple, so
/// a plain literal under it passes `by check` — but only the written-out
/// spelling used to be coerced, leaving an alias's value a bare tuple and every
/// field access an `AttributeError`. all three coercion sites, both spellings
const ALIAS_PROGRAM: &str = r#"
P = (name: str, age: int)
type Q = (name: str, age: int)


def make() -> P:
    return ("ada", 36)


v: P = ("bob", 1)
w: Q = ("cy", 2)
xs: list[P] = [("dee", 3)]

assert v.name == "bob", "assignment"
assert w.age == 2, "pep695 alias"
assert make().name == "ada", "return"
assert [x.name for x in xs] == ["dee"], "list"
print("ok")
"#;

/// the checker takes a plain tuple for the named tuple wherever one is expected — not only
/// in a return or an annotated assignment, but as a call's argument, a parameter's default, a
/// later assignment to a declared name, a dict's value, under a union, and inside another such
/// tuple — so each is one at runtime too, or the field access the checker passed raises
/// `AttributeError`
const EXPECTED_PROGRAM: &str = r#"
def f(v: (p: int, q: int)) -> int:
    return v.p

def g(v: (p: int, q: int) = (1, 2)) -> int:
    return v.q

def h() -> (p: int, q: int) | None:
    return (3, 4)

def nested(v: (inner: (a: int, b: int), q: int)) -> int:
    return v.inner.b

x: (p: int, q: int)
x = (5, 6)
d: dict[str, (p: int, q: int)] = {"k": (7, 8)}

assert f((1, 2)) == 1, "argument"
assert f(v=(9, 0)) == 9, "keyword argument"
assert g() == 2, "default"
assert h()?.q == 4, "union return"
assert x.q == 6, "declared"
assert d["k"].p == 7, "dict value"
assert nested(((1, 2), 3)) == 2, "nested"
print("ok")
"#;

/// both spellings of a construction against the same annotation build the one
/// class the transpiler emits, and that class's fields carry the *declared*
/// types — which is why the checker gives the named spelling the declared shape
/// rather than the literal one it is written with
const SPELLING_PROGRAM: &str = r#"
type P = (name: str, age: int)

a: P = ("ada", 36)
b: P = (name="ada", age=36)

assert type(a).__name__ == type(b).__name__, "one class"
assert a == b, "equal"
assert a._replace(name="bob").name == "bob", "positional spelling"
assert b._replace(name="bob").name == "bob", "named spelling"
print("ok")
"#;

/// the checker reads a plain tuple literal standing where an anonymous named tuple is
/// expected as an instance of it, wherever the expectation comes from. each one used to
/// stay a plain tuple at runtime unless it was an annotated assignment, a `return` or an
/// element of a list or set literal, so `by check` was clean and every field access below
/// raised `AttributeError`
const POSITIONS_PROGRAM: &str = r#"
from typing import Iterator

type P = (a: int, b: int)


def take(p: (a: int, b: int)) -> int:
    return p.a


def keyword(*, p: (a: int, b: int)) -> int:
    return p.b


def star(*ps: (a: int, b: int)) -> int:
    return sum(p.a for p in ps)


def default(p: (a: int, b: int) = (5, 6)) -> int:
    return p.a


def pairs() -> Iterator[(a: int, b: int)]:
    yield (1, 2)


def through_alias() -> P:
    return (9, 10)


class Holder:
    init(let pair: (a: int, b: int))


class Box:
    var pair: (a: int, b: int) = (0, 0)


assert take((7, 8)) == 7, "argument"
assert keyword(p=(7, 8)) == 8, "keyword argument"
assert star((1, 2), (3, 4)) == 4, "*args"
assert default() == 5, "default"
assert next(pairs()).b == 2, "yield"
assert through_alias().a == 9, "return through an alias"
assert Holder((3, 4)).pair.b == 4, "init parameter"

items: list[(a: int, b: int)] = []
items.append((1, 2))
assert items[0].a == 1, "method argument"

by_name: dict[str, (a: int, b: int)] = {"k": (1, 2)}
assert by_name["k"].b == 2, "dict value"

unique: set[(a: int, b: int)] = {(1, 2)}
assert next(iter(unique)).a == 1, "set element"

nested: list[list[(a: int, b: int)]] = [[(1, 2)]]
assert nested[0][0].b == 2, "nested list"

box = Box()
assert box.pair.a == 0, "class default"
box.pair = (3, 4)
assert box.pair.b == 4, "attribute write"
print("ok")
"#;

/// a field that is itself an anonymous named tuple builds the inner literal too, a union
/// builds the one arm that accepts the literal, and a write through a union receiver whose
/// members would each build the same anonymous named tuple builds it
const NESTED_AND_UNION_PROGRAM: &str = r#"
def outer(p: (inner: (x: int, y: int), label: str)) -> int:
    return p.inner.y


class Left:
    var pair: (a: int, b: int) = (0, 0)


class Right:
    var pair: (a: int, b: int) = (0, 0)


class Maybe:
    var pair: (a: int, b: int) | None = None


def assign(target: Left | Right | Maybe) -> int:
    target.pair = (1, 2)
    return target.pair.b


assert outer(((1, 2), "a")) == 2, "nested field"

maybe: (a: int) | None = (1,)
assert maybe.a == 1, "optional"

either: (a: int) | (b: str) = ("x",)
assert either.b == "x", "the arm whose fields accept the elements"

assert assign(Left()) == 2, "union receiver"
assert assign(Right()) == 2, "union receiver"
assert assign(Maybe()) == 2, "a member that may also hold None builds the same class"
print("ok")
"#;

/// a value that already is the named tuple passes through untouched, and a literal where a
/// plain tuple is expected stays a plain tuple
const UNCHANGED_PROGRAM: &str = r#"
def same(p: (a: int, b: int)) -> (a: int, b: int):
    return p


def kind(t: tuple[int, int]) -> str:
    return type(t).__name__


built: (a: int, b: int) = (1, 2)
assert same(built) === built, "an existing named tuple is not rebuilt"
assert kind((1, 2)) == "tuple", "a plain tuple stays a tuple"
assert kind(built) == type(built).__name__, "a named tuple passed as a tuple stays itself"
print("ok")
"#;

/// a named field after more than one positional field, or after a modifier, makes the same
/// anonymous named tuple a named field right after the first does: the annotation is the class
/// a literal under it is built as, not a plain tuple
const NAMED_AFTER_POSITIONALS_PROGRAM: &str = r#"
def later(p: (int, str, name: bytes)) -> bytes:
    return p.name


def borrowed(p: (int, local b: str)) -> str:
    return p.b


assert later.__annotations__["p"]._fields == ("arg0", "arg1", "name"), "after two positional fields"
assert borrowed.__annotations__["p"]._fields == ("arg0", "b"), "after a modifier"
assert later((1, "a", b"x")) == b"x", "built"
assert borrowed((1, "y")) == "y", "built"
print("ok")
"#;

fn run(program: &str) {
    run_with(program, SoundnessPositions::defaults());
}

/// `program` transpiled for the interpreter that runs it, with the soundness checks at
/// `soundness`
fn run_with(program: &str, soundness: SoundnessPositions) {
    let Some(python) = common::python(PythonVersion::PY39) else {
        return;
    };

    let config = Config {
        min_version: python.version,
        soundness,
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
        "transpiled anon named tuple program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

#[test]
fn anon_named_tuple_field_types_run() {
    run(PROGRAM);
}

#[test]
fn a_plain_tuple_under_an_alias_is_coerced() {
    run(ALIAS_PROGRAM);
}

#[test]
fn a_plain_tuple_is_the_named_tuple_wherever_one_is_expected() {
    run(EXPECTED_PROGRAM);
}

#[test]
fn both_construction_spellings_build_one_class() {
    run(SPELLING_PROGRAM);
}

/// a wrapped-optional field lowers to the runtime's `Optional`, which a
/// `NamedTuple` evaluates as the class is created — so the hoisted class has to
/// come after the runtime's definitions, not ahead of them
const WRAPPED_OPTIONAL_PROGRAM: &str = r#"
def g() -> (a: int??, b: int):
    return (a=None, b=2)

assert g().b == 2, "b"
print("ok")
"#;

#[test]
fn a_hoisted_class_sees_the_runtime() {
    run(WRAPPED_OPTIONAL_PROGRAM);
}

#[test]
fn a_plain_tuple_is_built_wherever_one_is_expected() {
    run(POSITIONS_PROGRAM);
}

#[test]
fn a_nested_field_and_a_union_arm_are_built() {
    run(NESTED_AND_UNION_PROGRAM);
}

#[test]
fn a_named_field_anywhere_makes_an_anonymous_named_tuple() {
    run(NAMED_AFTER_POSITIONALS_PROGRAM);
}

#[test]
fn a_named_tuple_and_a_plain_tuple_are_left_as_they_are() {
    run(UNCHANGED_PROGRAM);
}

/// every runtime soundness check on, a parameter's entry check among them: each checks
/// the value against the class the literal was built as
#[test]
fn built_values_pass_every_soundness_check() {
    for program in [
        POSITIONS_PROGRAM,
        NESTED_AND_UNION_PROGRAM,
        UNCHANGED_PROGRAM,
    ] {
        run_with(program, SoundnessPositions::all());
    }
}

/// transpile every one of `files` as one project for `target`, into a fresh directory under the
/// cargo temp dir, or the first transpile error
fn transpile_project(
    name: &str,
    target: PythonVersion,
    files: &[(&str, &str)],
) -> Result<PathBuf, String> {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    // a stale directory from an earlier run would mask a transpile failure
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create case dir");
    let mut db = TestDb::new(ProjectMetadata::new(
        ruff_python_ast::name::Name::new_static(""),
        SystemPathBuf::from("/"),
    ));
    db.init_program().expect("program init failed");
    for (path, source) in files {
        db.write_file(path, source).expect("write file failed");
    }
    let config = Config {
        min_version: target,
        ..Config::default()
    };
    for (path, _) in files {
        let file = system_path_to_file(&db, path).expect("file not in db");
        let transpiled =
            transpile_typed(&db, file, &config, None).map_err(|error| error.message)?;
        let relative = path.trim_start_matches('/').replace(".by", ".py");
        fs::write(dir.join(relative), transpiled).expect("write module");
    }
    Ok(dir)
}

/// a literal standing where another module's anonymous named tuple is expected is built in
/// this module, as a class declared from the checker's field types: the other module's class
/// is named by its own transpilation, which this one cannot see
#[test]
fn a_literal_for_another_modules_named_tuple_is_built() {
    let Some(python) = common::python(PythonVersion::PY39) else {
        return;
    };
    let dir = transpile_project(
        "anon_named_tuple_cross_module",
        python.version,
        &[
            (
                "/shapes.by",
                "def take(p: (a: int, b: list[str])) -> str:\n    return p.b[p.a]\n",
            ),
            (
                "/main.by",
                "from shapes import take\n\nassert take((1, [\"x\", \"y\"])) == \"y\"\nprint(\"ok\")\n",
            ),
        ],
    )
    .expect("transpile should succeed");
    let output = Command::new(&python.command)
        .arg("main.py")
        .current_dir(&dir)
        .output()
        .expect("failed to spawn python");
    assert!(
        output.status.success(),
        "transpiled program failed on {python}:\n--- stderr ---\n{}\n--- main.py ---\n{}",
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(dir.join("main.py")).unwrap_or_default(),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

/// a field type the module building the literal has no python spelling for is refused, rather
/// than declared in a class that would not run
#[test]
fn a_field_the_module_cannot_spell_is_refused() {
    let error = transpile_project(
        "anon_named_tuple_unspellable_field",
        PythonVersion::PY313,
        &[
            (
                "/callbacks.by",
                "def run(p: (f: (int) -> str)) -> str:\n    return p.f(1)\n",
            ),
            ("/main.by", "from callbacks import run\n\nrun((str,))\n"),
        ],
    )
    .expect_err("the field type has no spelling");
    assert_eq!(
        error,
        "the tuple at offset 31 is built as an anonymous named tuple whose field `f` has no \
         type the emitted python can name; construct it explicitly, or give it a `NamedTuple` \
         class of its own"
    );
}
