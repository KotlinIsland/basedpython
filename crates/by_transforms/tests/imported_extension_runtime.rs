//! Runtime test for the names a module binds the backing functions of an imported extension
//! under.
//!
//! An extension member lowers to a module-level backing function, `_by_ext__Foo__bar`, which a
//! module using the member imports from the module declaring the extension. The declaring module
//! names its backing functions clear of what it spells itself, and knows nothing of the modules
//! importing them: one of those may spell the same name, declare a backing function of its own
//! under it — an extension of a different class that happens to be called `Foo` — or emit a
//! `private` symbol under it. Imported under its own name, the function is then shadowed, or
//! shadows the importing module's binding, and a call reaches the wrong function with the
//! checker satisfied. Every place a lowering names an imported backing function is exercised: a
//! member call, a member reached through a trailing lambda's implicit receiver, a conversion an
//! extension supplies, and a conformance's witness table.
//!
//! The lowering needs cross-module type information, so the project is built through a typed db
//! rather than the single-file `transpile`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile_typed};
use ruff_db::files::system_path_to_file;
use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
use ty_project::{ProjectMetadata, TestDb};

mod interpreters;

const EXTENSIONS: &str = r#"
class Foo:
    def __init__(self, tag: str):
        self.tag = tag

extension Foo:
    def bar(self) -> str:
        return "a.bar:" + self.tag

    class def __of__(cls, value: str) -> Foo:
        return Foo("a:" + value)

protocol Show:
    def show(self) -> str

extension Show:
    def show(self) -> str:
        return "a.show"
"#;

/// spells the name of each backing function it imports, declares a backing function of its
/// own under one of them, and emits a `private` symbol under another
const MAIN: &str = r#"
import extensions
from extensions import Show

_by_ext__Foo____of__ = "spelled"
_by_ext__Show__show = "spelled"

class Foo: ...

extension Foo:
    def bar(self) -> str:
        return "main.bar"

private def by_ext__Foo__bar2() -> str:
    return "private"

class Widget: ...

extension Widget(Show): ...

def render(value: Show) -> str:
    return value.show()

def apply(fn: extensions.Foo.() -> None):
    fn(extensions.Foo("block"))

seen: str = ""
apply:
    seen = bar()

converted: extensions.Foo = "x"

assert extensions.Foo("call").bar() == "a.bar:call", "a member call"
assert Foo().bar() == "main.bar", "the module's own member of the same name"
assert seen == "a.bar:block", "a member reached through an implicit receiver"
assert converted.tag == "a:x", "a conversion an imported extension supplies"
assert render(Widget()) == "a.show", "a witness an imported extension supplies"
assert (_by_ext__Foo____of__, _by_ext__Show__show) == ("spelled", "spelled")
assert by_ext__Foo__bar2() == "private"
print("ok")
"#;

const FILES: &[(&str, &str)] = &[("/extensions.by", EXTENSIONS), ("/main.by", MAIN)];

/// transpile every file of the project for `target` into a fresh directory under the cargo
/// temp dir
fn build(target: PythonVersion) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("imported_extension");
    // a stale directory from an earlier run would mask a transpile failure
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create case dir");

    let mut db = TestDb::new(ProjectMetadata::new(
        ruff_python_ast::name::Name::new_static(""),
        SystemPathBuf::from("/"),
    ));
    db.init_program().expect("program init failed");
    for (path, source) in FILES {
        db.write_file(path, source).expect("write file failed");
    }
    let config = Config {
        min_version: target,
        ..Config::default()
    };
    for (path, _) in FILES {
        let file = system_path_to_file(&db, path).expect("file not in db");
        let transpiled = transpile_typed(&db, file, &config, None)
            .unwrap_or_else(|error| panic!("transpile of {path} should succeed: {error}"));
        let relative = path.trim_start_matches('/').replace(".by", ".py");
        fs::write(dir.join(relative), transpiled).expect("write module");
    }
    dir
}

#[test]
fn an_imported_backing_function_is_bound_clear_of_the_modules_own_names() {
    let target = PythonVersion::PY310;
    let Some(interpreter) = interpreters::oldest(target, "", "") else {
        return;
    };
    let dir = build(target);
    let output = Command::new(&interpreter.command)
        .arg("main.py")
        .current_dir(&dir)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled program failed on {interpreter}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- main.py ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(dir.join("main.py")).unwrap_or_default(),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    interpreters::ran(&interpreter, target);
}
