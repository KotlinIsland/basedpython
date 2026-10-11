//! Runtime tests for the lazy-import polyfill (`--min-version` < 3.15, which has no PEP 810
//! `lazy` keyword). A `from x import y` there binds nothing where it is written: every read of
//! `y` is written as a read through the module's `_LazyFrom`, which resolves `x.y` on first use
//! and stores it in the module's namespace
//!
//! These have to be *runtime* tests spanning several modules: what a read evaluates to, what
//! another module importing the name sees, and what a patch of the name reaches only show once
//! a real interpreter runs the emitted python across a real import, so neither the transform
//! unit tests (which assert lowered text) nor the mdtests (which assert types) can see them
//!
//! [`import_stays_lazy_until_first_use`] guards the other side of the trade: the rewrite exists
//! to defer the imported module's execution, so a "fix" that imported eagerly would pass every
//! other assertion while quietly destroying the feature

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use by_transforms::{Config, transpile, transpile_typed};
use ruff_db::files::system_path_to_file;
use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
use ty_project::{ProjectMetadata, TestDb};

mod interpreters;
use interpreters::Interpreter;

/// a soundness check in a function on a class the module imports further down is made once
/// the import has run. the import is deferred, so the name is not in the module's namespace
/// then, and the check asks the module's `_LazyFrom` whether its statement has run
const ONCE_BOUND: &[(&str, &str)] = &[
    (
        "/by_once_mod.by",
        r#"
class Thing:
    pass

def things() -> list[Thing]:
    wrong: dynamic = [1]
    return wrong
"#,
    ),
    (
        "/main.by",
        r#"
def first[T](xs: list[T]) -> T:
    return xs[0]

def use() -> object:
    return first(things())

from by_once_mod import Thing, things

try:
    use()
except TypeError as error:
    assert "type soundness violation" in str(error), str(error)
else:
    raise AssertionError("the check is made once the import has run")
print("ok")
"#,
    ),
];

/// `from . import x` names `x` in the package itself: a function its `__init__` defines,
/// or a submodule nothing has imported yet. python reads the attribute first and imports
/// the submodule only when there is none
const RELATIVE_PACKAGE: &str = r#"
def helper() -> str:
    return "helper"
"#;

const RELATIVE_SUBMODULE: &str = r#"
NAME = "other"
"#;

const RELATIVE_USER: &str = r#"
from . import helper
from . import other

def run() -> str:
    return helper() + " " + other.NAME
"#;

const RELATIVE_MAIN: &str = r#"
from by_relative_pkg.user import run

assert run() == "helper other", run()
print("ok")
"#;

/// a package whose `__init__` imports its own submodule, relatively and by its absolute name
///
/// the submodule is read off the package, which is the module doing the reading, and has
/// to be imported as python's fromlist imports it
const INIT_SUBMODULE: &[(&str, &str)] = &[
    (
        "by_init_rel/__init__",
        "from . import helper\n\ndef use() -> str:\n    return helper.NAME\n",
    ),
    ("by_init_rel/helper", "NAME = \"relative\"\n"),
    (
        "by_init_abs/__init__",
        "from by_init_abs import helper\n\ndef use() -> str:\n    return helper.NAME\n",
    ),
    ("by_init_abs/helper", "NAME = \"absolute\"\n"),
    (
        "main",
        r#"
import by_init_rel
import by_init_abs
from by_init_rel import helper as outside

print(outside.NAME)
print(by_init_rel.use(), by_init_abs.use())
"#,
    ),
];

/// two modules re-exporting a name each takes from the other, which neither binds
const REEXPORT_CYCLE: &[(&str, &str)] = &[
    ("by_rx_a", "from by_rx_b import x\n"),
    ("by_rx_b", "from by_rx_a import x\n"),
    (
        "main",
        r#"
try:
    import by_rx_a
    by_rx_a.x
except ImportError as error:
    print(type(error).__name__)
"#,
    ),
];

/// a name an import binds only on some paths reads the builtin of that name where the
/// import did not run, as python's global lookup does, and is a `NameError` where there is
/// no builtin
const BUILTIN_FALLBACK: &[(&str, &str)] = &[(
    "main",
    r#"
import sys

if sys.version_info < (3, 0):
    from itertools import izip as zip

if "--pretty" in sys.argv:
    from pprint import pprint as print

if len(sys.argv) > 5:
    from json import dumps

print(list(zip([1, 2], [3, 4])))
try:
    dumps
except NameError as error:
    print(str(error))
"#,
)];

/// a reload runs a module again over the namespace it had, so its imports have to bind what
/// their modules hold now, not what the first run read
const RELOAD: &[(&str, &str)] = &[
    ("by_reload_src", "VALUE = 1\n"),
    ("by_reload_user", "from by_reload_src import VALUE\n"),
    (
        "main",
        r#"
import importlib
import sys

sys.dont_write_bytecode = True
import by_reload_src
import by_reload_user

first = by_reload_user.VALUE
with open(by_reload_src.__file__, "w") as source:
    source.write("VALUE = 2\n")
importlib.invalidate_caches()
importlib.reload(by_reload_src)
importlib.reload(by_reload_user)
print(first, by_reload_user.VALUE)
"#,
    ),
];

/// a failing `from` import reports the error python reports: a submodule's own missing
/// dependency, an `AttributeError` the module's body raises, the name that is not there, and
/// the name a module still being imported has not bound yet
/// each case is a module of its own that imports and reads the name at its top level, where
/// python raises at the import and the polyfill at the read — both while the case module is
/// being imported, inside the `try` that catches it
const IMPORT_ERRORS: &[(&str, &str)] = &[
    ("by_err_pkg/__init__", ""),
    (
        "by_err_pkg/broken",
        "import by_not_installed_dependency\n\nX = 1\n",
    ),
    (
        "by_err_body",
        "raise AttributeError(\"raised by the body\")\n",
    ),
    (
        "by_err_case_dependency",
        "from by_err_pkg import broken\n\nbroken.X\n",
    ),
    (
        "by_err_case_body",
        "from by_err_body import anything\n\nanything\n",
    ),
    (
        "by_err_case_name",
        "from json import by_no_such_name\n\nby_no_such_name\n",
    ),
    (
        "by_err_case_submodule",
        "from urllib import by_no_such_submodule\n\nby_no_such_submodule\n",
    ),
    // a module whose name is read while it is still being imported
    (
        "by_err_case_circular",
        "from by_err_circular_other import f\n\nf()\nx = 1\n",
    ),
    (
        "by_err_circular_other",
        "from by_err_case_circular import x\n\ndef f() -> None:\n    pass\n\nx\n",
    ),
    (
        "main",
        r#"
import importlib

for case in ("dependency", "body", "name", "submodule", "circular"):
    try:
        importlib.import_module("by_err_case_" + case)
    except ImportError as error:
        print(case, type(error).__name__, str(error), error.name, error.path)
        print(getattr(error, "name_from", None))
    except AttributeError as error:
        # the attribute read the polyfill resolves the name in gives the error the name it
        # was read under, which an import raising it does not
        print(case, type(error).__name__, str(error))
"#,
    ),
];

/// module whose values the importer exercises
const VALUES_MODULE: &str = r#"
num: int = 5
text: str = "hi"
items: list[int] = [1, 2, 3]
ratio: float = 200.0
nothing: int? = None

class Point:
    def __init__(self, x: int):
        self.x = x

ORIGIN = Point(0)

def origin() -> Point:
    return ORIGIN
"#;

/// an imported name is the object it names, so everything that asks *which* object it is —
/// identity, `type()`, `json`, a library deciding how to treat a value by what its type
/// defines — gets the answer the eager import gives
const VALUES_MAIN: &str = r#"
import json
import by_values_mod as values
from by_values_mod import num, text, items, ratio, nothing, Point, ORIGIN, origin

def approx_like(expected: object) -> str:
    # `pytest.approx` reads anything whose type has `__iter__` as a collection
    return "collection" if hasattr(type(expected), "__iter__") else "scalar"

assert origin() === ORIGIN, "identity across the module boundary"
assert ORIGIN === values.ORIGIN, "the name is the module's object"
assert type(num) === int and type(text) === str and type(ORIGIN) === Point, "type()"
assert json.dumps({"n": num, "t": text, "i": items}) == '{"n": 5, "t": "hi", "i": [1, 2, 3]}', "json"
assert approx_like(ratio) == "scalar" and approx_like(items) == "collection", "approx"
assert nothing === None, "identity with None"
assert num + 1 == 6 and 1 + num == 6 and text + "!" == "hi!" and len(items) == 3, "operators"
assert isinstance(Point(1), Point) and Point(7).x == 7, "a class constructs and tests"
print("ok")
"#;

/// a module whose one value is told apart by identity
const TOKEN_MODULE: &str = r#"
class Token:
    pass

TOKEN = Token()
"#;

/// every scope a read can reach the module's binding from
const SCOPES_MAIN: &str = r#"
import by_token_mod as m
from by_token_mod import TOKEN

def nested() -> object:
    def inner() -> object:
        return TOKEN
    return inner()

class Body:
    seen = TOKEN

def default(value: object = TOKEN) -> object:
    return value

def reads_global() -> object:
    global TOKEN
    return TOKEN

def outer() -> object:
    TOKEN = "local"
    class Inner:
        seen = TOKEN
        TOKEN = "class"
    return Inner.seen

later = lambda: TOKEN
listed = [TOKEN for _ in range(1)]
text = f"{TOKEN!r}"

assert nested() === m.TOKEN, "a nested function"
assert Body.seen === m.TOKEN, "a class body"
assert default() === m.TOKEN, "a default value"
assert reads_global() === m.TOKEN, "a `global` read"
assert outer() === m.TOKEN, "a class body reads the module, not the function around it"
assert later() === m.TOKEN, "a lambda"
assert listed[0] === m.TOKEN, "a comprehension"
assert text == repr(m.TOKEN), "an f-string"
print("ok")
"#;

/// a name bound again keeps its import as written, and every read finds what python's would
const REBOUND_MAIN: &str = r#"
import by_token_mod as m
from by_token_mod import TOKEN

first = TOKEN

def read() -> object:
    return TOKEN

def rebind() -> None:
    global TOKEN
    TOKEN = "rebound"

assert first === m.TOKEN and read() === m.TOKEN, "before the rebinding, the import"
rebind()
assert read() == "rebound", "after it, the new binding"
print("ok")
"#;

/// a read made before the import statement has run fails as python's does
const EARLY_MAIN: &str = r#"
def early() -> object:
    return TOKEN

try:
    early()
except NameError as error:
    assert str(error) == "name 'TOKEN' is not defined", str(error)
else:
    raise AssertionError("a read before the import ran must raise")

from by_token_mod import TOKEN
import by_token_mod as m

assert early() === m.TOKEN, "and once it has run, reads the import"
print("ok")
"#;

const REEXPORT_SOURCE: &str = r#"
class Marker:
    pass

VALUE = Marker()
"#;

/// imports `VALUE` and never reads it, so only another module can
const REEXPORT_MIDDLE: &str = r#"
from by_reexport_src import VALUE
"#;

const REEXPORT_LISTED: &str = r#"
from by_reexport_src import VALUE

__all__ = ["VALUE"]
"#;

const REEXPORT_STAR: &str = r#"
from by_reexport_listed import *

def got() -> object:
    return VALUE
"#;

/// a module that imports a name passes it on, as python's would, through every way another
/// module asks for it. until something reads it, the name is not in the module's namespace
const REEXPORT_MAIN: &str = r#"
import by_reexport_src as src
import by_reexport_middle as middle
import by_reexport_star as star

assert "VALUE" not in vars(middle), "an unread name is not in the namespace yet"
assert "VALUE" in dir(middle), "dir() lists it"
assert hasattr(middle, "VALUE") and middle.VALUE === src.VALUE, "an attribute read"
assert vars(middle)["VALUE"] === src.VALUE, "which leaves the object in the namespace"
assert not hasattr(middle, "NOT_THERE"), "a name the module never bound is still missing"
assert star.got() === src.VALUE, "a star import of a module with `__all__`"

from by_reexport_middle import VALUE
assert VALUE === src.VALUE, "a `from` import of it"
print("ok")
"#;

/// two modules importing each other's names: neither runs the other until a name is read
const CYCLE_A: &str = r#"
from by_cycle_b import pong

def ping(n: int) -> int:
    return 0 if n == 0 else pong(n - 1)
"#;

const CYCLE_B: &str = r#"
from by_cycle_a import ping

def pong(n: int) -> int:
    return 1 if n == 0 else ping(n - 1)

PARTNER = ping
"#;

/// the cycle the other way round: one side's name is read in an annotation, which keeps that
/// import eager, and the other side's import of it is deferred
const CYCLE_C: &str = r#"
from by_cycle_d import Maker

class Thing:
    def maker(self) -> Maker:
        return Maker()
"#;

const CYCLE_D: &str = r#"
from by_cycle_c import Thing

class Maker:
    def thing(self) -> object:
        return Thing()
"#;

const CYCLE_MAIN: &str = r#"
import by_cycle_a as a
import by_cycle_b as b
from by_cycle_a import ping
from by_cycle_c import Thing

assert ping(3) == 1, "a call crosses the cycle"
assert b.PARTNER === a.ping, "and the name is the other module's object"
assert isinstance(Thing().maker().thing(), Thing), "an eager side and a deferred side"
print("ok")
"#;

const PATCH_DEPENDENCY: &str = r#"
def fetch() -> str:
    return "real"
"#;

const PATCH_SERVICE: &str = r#"
from by_patch_dep import fetch

def run() -> str:
    return fetch()
"#;

/// a test patches a name where it is used, in the module that imported it. the patch writes
/// that module's namespace, which is where each of its reads looks first
const PATCH_MAIN: &str = r#"
from unittest import mock
import by_patch_svc as svc

with mock.patch("by_patch_svc.fetch", lambda: "patched"):
    assert svc.run() == "patched", "a patch made before the name was read"
assert svc.run() == "real", "and undone"

with mock.patch.object(svc, "fetch", lambda: "object"):
    assert svc.run() == "object", "a patch made after"
assert svc.run() == "real", "and undone"

original = svc.fetch
setattr(svc, "fetch", lambda: "set")
assert svc.run() == "set", "what `monkeypatch.setattr` does"
setattr(svc, "fetch", original)
assert svc.run() == "real", "and undoes"
print("ok")
"#;

/// writes a marker file the moment it executes, so the importer can observe
/// *when* execution happened
const LAZY_MODULE: &str = r#"
_marker = open("executed.marker", "w")
_marker.write("ran")
_marker.close()
val: int = 42
"#;

const LAZY_MAIN: &str = r#"
import os
from by_lazy_mod import val

assert not os.path.exists("executed.marker"), "module must not execute at the import statement"
assert val + 0 == 42, "value resolves on use"
assert os.path.exists("executed.marker"), "module must execute on first use"
print("ok")
"#;

/// defines a `Character` and exports it, so the importer can check that a
/// `Character` built *here* is recognised by the `Character` *there*
const CHAR_MODULE: &str = r#"
made_here: Character = "a"
"#;

/// `isinstance` tests class identity, so a `Character` defined per-module would
/// make each module's class a distinct object and fail this across the import
/// boundary — even though `==` (plain `str` equality) would still pass
const CHAR_MAIN: &str = r#"
from ty_extensions import Character
from by_char_mod import made_here

made_there: Character = "a"

assert made_here == made_there, "str equality holds regardless of class identity"
assert isinstance(made_there, Character), "same-module Character"
assert isinstance(made_here, Character), "Character identity must survive an import"
# `===` is identity here — in basedpython `is` means isinstance
assert type(made_here) === type(made_there), "one Character class per process"
assert made_here.__class__.__name__ == "Character", "still named Character"
print("ok")
"#;

/// `from pkg import sub` where `sub` is a *submodule* rather than an attribute of the
/// package. `urllib/__init__.py` does not import `parse`, so a plain attribute read
/// finds nothing — cpython binds it only because `__import__` is handed a fromlist and
/// `_handle_fromlist` imports the submodule on the package's behalf
const SUBMODULE_MAIN: &str = r#"
from urllib import parse
from urllib import by_not_there as missing

assert parse.quote("a b") == "a%20b", "a submodule resolves on first use"
assert parse.urlparse("http://h/p").path == "/p", "and keeps working after the first use"

try:
    missing.anything
except ImportError as e:
    assert "by_not_there" in str(e), "names the attribute that was not found"
except AttributeError:
    raise AssertionError("a name that is neither attribute nor submodule is an ImportError")
else:
    raise AssertionError("a missing name must raise")
print("ok")
"#;

/// a module the watcher below lazifies. it has to be a real file for
/// `find_spec` to reach the `LazyLoader` path at all
const WATCHED_MODULE: &str = r#"
val: int = 7
"#;

/// making a module lazy is not a quiet operation: through 3.12
/// `LazyLoader.exec_module` opens with `import threading`, which reaches
/// `functools`, which asks `from collections import namedtuple`. anything the
/// name is claimed for before that runs is visible to those imports as a module
/// nothing has executed, and lazifying `collections` used to hand exactly that
/// shell back — `ImportError: cannot import name 'namedtuple'`
///
/// so the name must not be in `sys.modules` yet when `exec_module` is called,
/// and it must be the module we made once it returns
const WINDOW_MAIN: &str = r#"
import sys
import importlib.util as ilu

_claimed: list[bool] = []
_real = ilu.LazyLoader.exec_module


def _watch(loader, module) -> object:
    _claimed.append(module.__spec__.name in sys.modules)
    return _real(loader, module)


ilu.LazyLoader.exec_module = _watch

import by_watched_mod

assert _claimed == [False], "a module nothing has executed was published: " + repr(_claimed)
assert sys.modules["by_watched_mod"] === by_watched_mod, "the name must end up bound to the module we made"
assert by_watched_mod.val == 7, "and it still resolves"
print("ok")
"#;

/// a single `import` statement mixing modules that can be lazified with ones
/// that can't. the rewrite replaces the whole statement, so every name it does
/// not lazify has to be re-emitted as a plain import — dropping one leaves it
/// unbound and the module dies with `NameError` at first use.
///
/// two kinds of alias can't be lazified: a bootstrap module (`sys`,
/// `importlib`), which the polyfill preamble imports under a private name for
/// its own use, and a dotted `import a.b` without `as`, which binds `a` rather
/// than `a.b`
const MIXED_MAIN: &str = r#"
import math, sys, time
import os.path, json as j
import sys as system, textwrap

assert math.floor(1.5) == 1, "a lazifiable module before an eager one still binds"
assert sys.maxsize > 0, "a bootstrap module in the middle stays bound"
assert time.gmtime(0).tm_year == 1970, "and the alias after it survives too"
assert os.path.basename("a/b") == "b", "a dotted import binds its top package"
assert j.dumps([1]) == "[1]", "an aliased lazifiable module beside it still works"
assert system.maxsize == sys.maxsize, "an aliased bootstrap module keeps its alias"
assert textwrap.indent("a", " ") == " a", "and the alias after that one binds too"
print("ok")
"#;

/// the readers of an annotation, each asserting the answer python gives for the names it
/// finds. an annotation is read back from the module's namespace — `typing.get_type_hints`
/// evaluates a string annotation against it — where a deferred name is not until something
/// reads it, and `dataclasses` tells a `KW_ONLY` annotation from a field by identity as the
/// class is made. so a name an annotation reads is imported eagerly
const ANNOTATIONS_MAIN: &str = r#"
from dataclasses import dataclass, fields, KW_ONLY, InitVar
from fractions import Fraction
from functools import singledispatch
import fractions
import typing

@dataclass
class Row:
    name: str
    _: KW_ONLY
    extra: int = 0
    seed: InitVar[int] = 0

assert [field.name for field in fields(Row)] == ["name", "extra"], "`KW_ONLY` and `InitVar` are not fields"
assert Row("a", extra=1, seed=2).extra == 1, "the row constructs with its keyword-only field"

@singledispatch
def describe(value: object) -> str:
    return "object"

@describe.register
def _(value: Fraction) -> str:
    return "fraction"

assert describe(Fraction(1, 2)) == "fraction", "an annotation registers a class"

def half(value: Fraction) -> Fraction:
    return value / Fraction(2)

assert typing.get_type_hints(half)["value"] === fractions.Fraction, "the hint is the class itself"
print("ok")
"#;

/// under `from __future__ import annotations` every annotation is a string, and
/// `typing.get_type_hints` evaluates it against the module's globals
const FUTURE_ANNOTATIONS_MAIN: &str = r#"
from __future__ import annotations
from fractions import Fraction
import fractions
import typing

def half(value: Fraction) -> None:
    pass

assert typing.get_type_hints(half)["value"] === fractions.Fraction, "the string resolves to the class"
print("ok")
"#;

/// a module that binds names of builtins the polyfill reads when it resolves a name, and
/// the one the module's `_LazyFrom` is made with. the runtime is pasted into the module and
/// reads the builtins through its globals, so it reads them under names of its own
const BUILTIN_NAMES_MAIN: &str = r#"
from decimal import Decimal
from math import pi
from string import digits

def getattr(o: object, name: str) -> int:
    return 42

def format(value: object, spec: str) -> str:
    return "shadowed"

def iter(o: object) -> int:
    return 0

def globals() -> int:
    return 7

assert Decimal(1) + 1 == Decimal(2), "resolving a name calls `getattr`"
assert f"{pi:.2f}" == "3.14", "formatting the value calls `format`"
assert [c for c in digits][:2] == ["0", "1"], "iterating the value calls `iter`"
assert getattr(1, "x") == 42 and format(1, "") == "shadowed" and iter(1) == 0 and globals() == 7
print("ok")
"#;

/// the interpreter to run the transpiled output on: the oldest found that runs what the
/// transpiler targets by default, 3.10 — a dataclass's `KW_ONLY` among it. none is a skip, said so
fn python() -> Option<Interpreter> {
    interpreters::oldest(Config::default().min_version, "", "")
}

/// transpile `sources` (`module stem` → basedpython source) into a fresh
/// directory under the cargo temp dir
fn build_case(case: &str, sources: &[(&str, &str)]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(case);
    // a stale directory from an earlier run would mask a transpile failure
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create case dir");

    for (stem, source) in sources {
        let transpiled = transpile(source, &Config::default()).expect("transpile should succeed");
        let path = dir.join(format!("{stem}.py"));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create package dir");
        }
        fs::write(path, transpiled).expect("write module");
    }
    dir
}

/// run `sources` both transpiled and as the python they are written in, asserting the two
/// print the same, and something. the sources are plain python, so python running them is
/// the reference
fn agrees_with_python(python: &Interpreter, case: &str, sources: &[(&str, &str)]) {
    let transpiled = build_case(case, sources);
    let reference = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{case}_python"));
    let _ = fs::remove_dir_all(&reference);
    for (stem, source) in sources {
        let path = reference.join(format!("{stem}.py"));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create package dir");
        }
        fs::write(path, source).expect("write module");
    }
    let run = |dir: &Path| {
        let output = Command::new(&python.command)
            .arg("main.py")
            .current_dir(dir)
            .output()
            .expect("failed to spawn python");
        assert!(
            output.status.success(),
            "{} failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
            dir.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        // the two trees are in directories of their own, which a message naming a file says
        String::from_utf8_lossy(&output.stdout).replace(&dir.display().to_string(), "<dir>")
    };
    let expected = run(&reference);
    assert!(!expected.trim().is_empty(), "the reference printed nothing");
    assert_eq!(
        run(&transpiled),
        expected,
        "the transpiled program disagrees with python"
    );
    interpreters::ran(python, Config::default().min_version);
}

/// run `main.py` in `dir`, asserting it exits cleanly and prints `ok`
fn run_main(python: &Interpreter, dir: &Path) {
    let output = Command::new(&python.command)
        .arg("main.py")
        .current_dir(dir)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled program failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    interpreters::ran(python, Config::default().min_version);
}

#[test]
fn an_imported_value_is_the_object_itself() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case(
        "lazy_values",
        &[("by_values_mod", VALUES_MODULE), ("main", VALUES_MAIN)],
    );
    run_main(&python, &dir);
}

#[test]
fn character_identity_survives_an_import() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case(
        "char_identity",
        &[("by_char_mod", CHAR_MODULE), ("main", CHAR_MAIN)],
    );
    run_main(&python, &dir);
}

#[test]
fn import_stays_lazy_until_first_use() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case(
        "lazy_defer",
        &[("by_lazy_mod", LAZY_MODULE), ("main", LAZY_MAIN)],
    );
    run_main(&python, &dir);
}

#[test]
fn a_name_is_claimed_only_once_the_module_is_lazy() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case(
        "lazy_window",
        &[("by_watched_mod", WATCHED_MODULE), ("main", WINDOW_MAIN)],
    );
    run_main(&python, &dir);
}

#[test]
fn an_unlazifiable_name_in_a_multi_name_import_stays_bound() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case("lazy_mixed", &[("main", MIXED_MAIN)]);
    run_main(&python, &dir);
}

#[test]
fn a_submodule_resolves_on_first_use() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case("lazy_submodule", &[("main", SUBMODULE_MAIN)]);
    run_main(&python, &dir);
}

#[test]
fn an_annotation_reads_the_imported_object() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case("lazy_annotations", &[("main", ANNOTATIONS_MAIN)]);
    run_main(&python, &dir);
}

#[test]
fn a_string_annotation_resolves_to_the_imported_object() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case(
        "lazy_future_annotations",
        &[("main", FUTURE_ANNOTATIONS_MAIN)],
    );
    run_main(&python, &dir);
}

#[test]
fn the_polyfill_reads_builtins_the_module_binds_for_itself() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case("lazy_builtin_names", &[("main", BUILTIN_NAMES_MAIN)]);
    run_main(&python, &dir);
}

#[test]
fn a_read_in_every_scope_is_the_object_itself() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case(
        "lazy_scopes",
        &[("by_token_mod", TOKEN_MODULE), ("main", SCOPES_MAIN)],
    );
    run_main(&python, &dir);
}

#[test]
fn a_name_bound_again_reads_as_python_does() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case(
        "lazy_rebound",
        &[("by_token_mod", TOKEN_MODULE), ("main", REBOUND_MAIN)],
    );
    run_main(&python, &dir);
}

#[test]
fn a_read_before_the_import_runs_raises_python_s_name_error() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case(
        "lazy_early",
        &[("by_token_mod", TOKEN_MODULE), ("main", EARLY_MAIN)],
    );
    run_main(&python, &dir);
}

#[test]
fn a_deferred_name_is_passed_on_to_other_modules() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case(
        "lazy_reexport",
        &[
            ("by_reexport_src", REEXPORT_SOURCE),
            ("by_reexport_middle", REEXPORT_MIDDLE),
            ("by_reexport_listed", REEXPORT_LISTED),
            ("by_reexport_star", REEXPORT_STAR),
            ("main", REEXPORT_MAIN),
        ],
    );
    run_main(&python, &dir);
}

#[test]
fn modules_importing_each_other_resolve_on_use() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case(
        "lazy_cycle",
        &[
            ("by_cycle_a", CYCLE_A),
            ("by_cycle_b", CYCLE_B),
            ("by_cycle_c", CYCLE_C),
            ("by_cycle_d", CYCLE_D),
            ("main", CYCLE_MAIN),
        ],
    );
    run_main(&python, &dir);
}

#[test]
fn a_patch_of_the_importing_module_reaches_its_reads() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case(
        "lazy_patch",
        &[
            ("by_patch_dep", PATCH_DEPENDENCY),
            ("by_patch_svc", PATCH_SERVICE),
            ("main", PATCH_MAIN),
        ],
    );
    run_main(&python, &dir);
}

#[test]
fn a_check_on_a_deferred_class_is_made_once_its_import_has_run() {
    let Some(python) = python() else {
        return;
    };
    let mut db = TestDb::new(ProjectMetadata::new(
        ruff_python_ast::name::Name::new_static(""),
        SystemPathBuf::from("/"),
    ));
    db.init_program().expect("program init failed");
    for (path, source) in ONCE_BOUND {
        db.write_file(path, source).expect("write file failed");
    }
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("lazy_once_bound");
    // a stale directory from an earlier run would mask a transpile failure
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create case dir");
    for (path, _) in ONCE_BOUND {
        let file = system_path_to_file(&db, path).expect("file not in db");
        let transpiled = transpile_typed(&db, file, &Config::default(), None)
            .unwrap_or_else(|error| panic!("transpile of {path} should succeed: {error}"));
        if path.ends_with("main.by") {
            // the check this is about, on a name the polyfill defers
            assert!(
                transpiled.contains("_soundness_once_bound((\"Thing\",)")
                    && transpiled.contains("lambda: _by_lazy.Thing"),
                "{transpiled}"
            );
        }
        let relative = path.trim_start_matches('/').replace(".by", ".py");
        fs::write(dir.join(relative), transpiled).expect("write module");
    }
    run_main(&python, &dir);
}

#[test]
fn a_relative_import_of_the_package_reads_its_attribute_or_its_submodule() {
    let Some(python) = python() else {
        return;
    };
    let dir = build_case(
        "lazy_relative",
        &[
            ("by_relative_pkg/__init__", RELATIVE_PACKAGE),
            ("by_relative_pkg/other", RELATIVE_SUBMODULE),
            ("by_relative_pkg/user", RELATIVE_USER),
            ("main", RELATIVE_MAIN),
        ],
    );
    run_main(&python, &dir);
}

#[test]
fn a_package_importing_its_own_submodule_agrees_with_python() {
    let Some(python) = python() else {
        return;
    };
    agrees_with_python(&python, "lazy_init_submodule", INIT_SUBMODULE);
}

#[test]
fn a_re_export_cycle_is_python_s_import_error() {
    let Some(python) = python() else {
        return;
    };
    agrees_with_python(&python, "lazy_reexport_cycle", REEXPORT_CYCLE);
}

#[test]
fn a_name_whose_import_did_not_run_reads_the_builtin() {
    let Some(python) = python() else {
        return;
    };
    agrees_with_python(&python, "lazy_builtin_fallback", BUILTIN_FALLBACK);
}

#[test]
fn a_reloaded_module_binds_what_its_modules_hold_now() {
    let Some(python) = python() else {
        return;
    };
    agrees_with_python(&python, "lazy_reload", RELOAD);
}

#[test]
fn a_failing_import_reports_python_s_error() {
    let Some(python) = python() else {
        return;
    };
    agrees_with_python(&python, "lazy_import_errors", IMPORT_ERRORS);
}
