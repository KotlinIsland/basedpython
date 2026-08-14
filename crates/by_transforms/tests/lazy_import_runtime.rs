//! Runtime tests for the lazy-import polyfill (`--min-version` < 3.15, which
//! has no PEP 810 `lazy` keyword and so binds `from x import y` to a
//! `_LazyAttr` proxy).
//!
//! These have to be *runtime* tests spanning two modules: the proxy only
//! misbehaves once a real interpreter applies an operator to it across a real
//! import, so neither the transform unit tests (which assert lowered text) nor
//! the mdtests (which assert types) can see it. The proxy previously forwarded
//! only `__getattr__` / `__call__` / `__getitem__`, which meant every
//! *unforwarded* dunder silently fell back to `object`'s version — `a == b`
//! compared proxy identity and answered `False` for equal values, and `a + 1`
//! raised `TypeError`. Python looks special methods up on the type and never
//! routes them through `__getattr__`, so that class of bug is invisible until
//! executed.
//!
//! [`import_stays_lazy_until_first_use`] guards the other side of the trade:
//! the proxy exists to defer the imported module's execution, so a "fix" that
//! resolved eagerly would pass every forwarding assertion while quietly
//! destroying the feature.

#![expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use by_transforms::{Config, transpile};

/// module whose values the importer exercises. every binding is a plain value
/// (not a callable), which is exactly the case the proxy used to break
const VALUES_MODULE: &str = r#"
num: int = 5
text: str = "hi"
items: list[int] = [1, 2, 3]

class Point:
    def __init__(self, x: int):
        self.x = x
"#;

/// every operator here reaches the proxy as a dunder looked up on its *type*,
/// so each assertion fails unless the proxy forwards that specific method
const VALUES_MAIN: &str = r#"
from by_values_mod import num, text, items, Point

assert num == 5, "eq"
assert not (num != 5), "ne"
assert num < 9 and num > 1 and num <= 5 and num >= 5, "ordering"
assert num + 1 == 6 and 1 + num == 6, "add / radd"
assert num * 2 == 10 and 10 - num == 5, "mul / rsub"
assert num % 3 == 2 and 2 ** num == 32, "mod / rpow"
assert -num == -5 and abs(-num) == 5, "unary"
assert text + "!" == "hi!" and "oh " + text == "oh hi", "concat / rconcat"
assert len(items) == 3, "len"
assert [i * 2 for i in items] == [2, 4, 6], "iter"
assert 2 in items, "contains"
assert items[0] == 1 and text[1] == "i", "getitem"
assert bool(num) and hash(num) == hash(5), "bool / hash"
assert str(text) == "hi" and repr(text) == "'hi'", "str / repr"
assert f"{num:03d}" == "005", "format"
assert int(num) == 5 and float(num) == 5.0, "int / float"
assert sorted([3, num, 1]) == [1, 3, 5], "sorting uses forwarded comparisons"
assert list(reversed(items)) == [3, 2, 1], "reversed"
assert isinstance(num, int), "isinstance via __class__"
assert isinstance(Point(1), Point), "isinstance against a lazily-imported class"
assert Point(7).x == 7, "call + attribute"
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
# `===` is identity here — in basedpython `is` means isinstance. and identity is
# read off `__class__` rather than `type()`: `made_here` is a lazily-imported
# proxy whose `type()` is `_LazyAttr` by design (a documented limit of the
# < 3.15 polyfill), while `__class__` is both forwarded by the proxy and what
# `isinstance` itself consults
assert made_here.__class__ === made_there.__class__, "one Character class per process"
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

assert parse.quote("a b") == "a%20b", "a submodule resolves through the proxy"
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

/// an interpreter to run the transpiled output on. `$PYTHON` first, then the
/// usual names; `None` (test skips) when none is found
fn python() -> Option<String> {
    let mut candidates = Vec::new();
    if let Ok(p) = std::env::var("PYTHON") {
        candidates.push(p);
    }
    candidates.extend(["python3", "python"].map(String::from));

    candidates.into_iter().find(|py| {
        Command::new(py)
            .args(["-c", ""])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
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
        fs::write(dir.join(format!("{stem}.py")), transpiled).expect("write module");
    }
    dir
}

/// run `main.py` in `dir`, asserting it exits cleanly and prints `ok`
fn run_main(python: &str, dir: &Path) {
    let output = Command::new(python)
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
}

#[test]
fn proxy_forwards_value_operations() {
    let Some(python) = python() else {
        eprintln!("skipping lazy-import runtime test: no python interpreter found");
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
        eprintln!("skipping lazy-import runtime test: no python interpreter found");
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
        eprintln!("skipping lazy-import runtime test: no python interpreter found");
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
        eprintln!("skipping lazy-import runtime test: no python interpreter found");
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
        eprintln!("skipping lazy-import runtime test: no python interpreter found");
        return;
    };
    let dir = build_case("lazy_mixed", &[("main", MIXED_MAIN)]);
    run_main(&python, &dir);
}

#[test]
fn a_submodule_resolves_through_the_proxy() {
    let Some(python) = python() else {
        eprintln!("skipping lazy-import runtime test: no python interpreter found");
        return;
    };
    let dir = build_case("lazy_submodule", &[("main", SUBMODULE_MAIN)]);
    run_main(&python, &dir);
}
