//! runtime half of the django lookup-expression contract.
//!
//! `filter(author.name == "x")` lowers to `filter(author__name="x")`, and what
//! matters is not the text but what django is handed: the right keyword names,
//! the right values, once each. this transpiles a project against a minimal
//! django whose manager records the keywords it receives, executes the output,
//! and asserts on the record.
//!
//! the recorded keywords are the same ones a real django builds its SQL from —
//! `docs/basedpython/frameworks/django.md` covers the query each spells.

use std::fs;
use std::path::Path;
use std::process::Command;

/// a django small enough to read and complete enough for both halves: ty
/// recognizes the model, the fields and the manager from it (the `KnownClass`
/// names have to be *defined* in django's own module paths, on a third-party
/// search path, hence the site-packages layout), and it runs, recording the
/// keywords a lookup call is handed
const MOCK_DJANGO: &[(&str, &str)] = &[
    ("django/__init__.py", ""),
    // `from django.db import models` reads `models` as an attribute of the
    // package, which only exists once the submodule is imported
    (
        "django/db/__init__.py",
        "from django.db import models as models\n",
    ),
    (
        "django/db/models/__init__.py",
        "from django.db.models.base import Model as Model\n\
         from django.db.models.fields import CharField as CharField, DateField as DateField\n\
         from django.db.models.fields.json import JSONField as JSONField\n\
         from django.db.models.fields.related import CASCADE as CASCADE, ForeignKey as ForeignKey\n\
         from django.db.models.manager import Manager as Manager\n\
         from django.db.models.query import Q as Q, QuerySet as QuerySet\n",
    ),
    (
        "django/db/models/base.py",
        "from typing import Any, ClassVar, Self\n\
         \n\
         from django.db.models.manager import Manager\n\
         \n\
         \n\
         class Model:\n\
         \x20   objects: ClassVar[Manager[Self]]\n\
         \x20   pk: Any\n\
         \n\
         \x20   def __init_subclass__(cls, **kwargs: Any) -> None:\n\
         \x20       super().__init_subclass__(**kwargs)\n\
         \x20       cls.objects = Manager()\n",
    ),
    (
        "django/db/models/manager.py",
        "from typing import Any\n\
         \n\
         from django.db.models.query import QuerySet\n\
         \n\
         \n\
         class BaseManager[_M]:\n\
         \x20   def filter(self, *args: Any, **kwargs: Any) -> QuerySet[_M, _M]:\n\
         \x20       return QuerySet(kwargs)\n\
         \n\
         \x20   def exclude(self, *args: Any, **kwargs: Any) -> QuerySet[_M, _M]:\n\
         \x20       return QuerySet(kwargs)\n\
         \n\
         \n\
         class Manager[_M](BaseManager[_M]):\n\
         \x20   pass\n",
    ),
    (
        "django/db/models/query.py",
        "from typing import Any\n\
         \n\
         \n\
         class Q:\n\
         \x20   def __init__(self, **kwargs: Any) -> None: ...\n\
         \n\
         \n\
         class QuerySet[_M, _Row = _M]:\n\
         \x20   def __init__(self, recorded: dict[str, Any] | None = None) -> None:\n\
         \x20       self.recorded = recorded or {}\n",
    ),
    (
        "django/db/models/fields/__init__.py",
        "from typing import Any\n\
         \n\
         \n\
         class Field[_ST, _GT]:\n\
         \x20   _pyi_private_set_type: Any\n\
         \x20   _pyi_private_get_type: Any\n\
         \n\
         \x20   def __init__(self, **kwargs: Any) -> None: ...\n\
         \x20   def __get__(self, instance: Any, owner: Any = None) -> _GT: ...\n\
         \x20   def __set__(self, instance: Any, value: _ST) -> None: ...\n\
         \n\
         \n\
         class CharField[_ST, _GT](Field[_ST, _GT]):\n\
         \x20   _pyi_private_set_type: str\n\
         \x20   _pyi_private_get_type: str\n\
         \n\
         \n\
         class DateField[_ST, _GT](Field[_ST, _GT]):\n\
         \x20   _pyi_private_set_type: Any\n\
         \x20   _pyi_private_get_type: Any\n",
    ),
    // the key/index transforms live on `JSONField`, which is recognized by the
    // module it is declared in, so the mock has to declare it in that module too
    (
        "django/db/models/fields/json.py",
        "from typing import Any\n\
         \n\
         from django.db.models.fields import Field\n\
         \n\
         \n\
         class JSONField[_ST, _GT](Field[_ST, _GT]):\n\
         \x20   _pyi_private_set_type: Any\n\
         \x20   _pyi_private_get_type: Any\n",
    ),
    (
        "django/db/models/fields/related.py",
        "from typing import Any\n\
         \n\
         from django.db.models.fields import Field\n\
         \n\
         CASCADE = \"cascade\"\n\
         \n\
         \n\
         class ForeignKey[_ST, _GT](Field[_ST, _GT]):\n\
         \x20   _pyi_private_set_type: Any\n\
         \x20   _pyi_private_get_type: Any\n\
         \n\
         \x20   def __init__(self, to: Any, on_delete: Any = None, **kwargs: Any) -> None: ...\n",
    ),
];

const MODELS: &str = "\
from django.db import models


class Author(models.Model):
    name = models.CharField(max_length=100)


class Book(models.Model):
    title = models.CharField(max_length=100)
    author = models.ForeignKey(Author, on_delete=models.CASCADE)


class Doc(models.Model):
    name = models.CharField(max_length=100)
    data = models.JSONField()
";

const MAIN: &str = r#"
from django.db.models import Q

from models import Book, Doc


def label(name, queryset):
    print(name, sorted(queryset.recorded.items()))


calls = 0


def once(value):
    global calls
    calls += 1
    return value


label("exact", Book.objects.filter(title == "Left Hand"))
label("relation", Book.objects.filter(author.name == "Ursula"))
label("pk", Book.objects.filter(pk == 1))
label("membership", Book.objects.filter(title in ["a", "b"]))
label("combined", Book.objects.filter(author.name == "Ursula", title == "Left Hand"))
label("excluded", Book.objects.exclude(title == "x"))
label("after positional", Book.objects.filter(Q(), title == "x"))
label("beside keyword", Book.objects.filter(title == "a", author__name="b"))
label("evaluated once", Book.objects.filter(title == once("v")))
label("json key", Doc.objects.filter(data["key"] == 1))
label("json nested", Doc.objects.filter(data["a"]["b"] == 1))
label("json index", Doc.objects.filter(data[0] == 1))
label("json key operator", Doc.objects.filter(data["key"] > 1))
label("json key and field", Doc.objects.filter(name == "n", data["key"] == 1))
print("calls", calls)
"#;

/// a CPython 3.13, provisioned through uv the way the divergence harness does:
/// the mock django uses pep 695 generics and pep 696 defaults, whose runtime
/// floor is 3.13
#[cfg(not(windows))]
fn python() -> Option<String> {
    if let Ok(path) = std::env::var("PYTHON") {
        return Some(path);
    }
    let find = || {
        let output = Command::new("uv")
            .args(["python", "find", "3.13"])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    if let Some(path) = find() {
        return Some(path);
    }
    Command::new("uv")
        .args(["python", "install", "3.13"])
        .output()
        .ok()?;
    find()
}

#[cfg(windows)]
fn python() -> Option<String> {
    std::env::var("PYTHON").ok()
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("a relative path has a parent")).expect("create dir");
    fs::write(path, contents).expect("write file");
}

fn transpile(project: &Path, module: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["transpile", module])
        .current_dir(project)
        .output()
        .expect("failed to spawn by");
    assert!(
        output.status.success(),
        "by transpile {module} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("transpiled output is utf-8")
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn lookup_expressions_hand_django_the_keywords_they_spell() {
    let Some(python) = python() else {
        eprintln!(
            "skipping django lookup runtime test: no python 3.13 found \
             (set PYTHON to one, or make `uv` available)"
        );
        return;
    };

    let project = tempfile::tempdir().expect("tempdir");
    let root = project.path();
    // ty only reads django's `KnownClass` names off a third-party search path,
    // so the mock lives in a venv's site-packages rather than beside the source
    let site_packages = "\
.venv/lib/python3.13/site-packages";
    for (relative, contents) in MOCK_DJANGO {
        write(root, &format!("{site_packages}/{relative}"), contents);
    }
    write(
        root,
        ".venv/pyvenv.cfg",
        "home = /usr/bin\nversion = 3.13.0\n",
    );
    write(
        root,
        "pyproject.toml",
        "[project]\nname = \"probe\"\nversion = \"0\"\nrequires-python = \">=3.13\"\n\n\
         [tool.ty.environment]\npython = \".venv\"\npython-version = \"3.13\"\n",
    );
    write(root, "models.by", MODELS);
    write(root, "main.by", MAIN);

    let lowered = project.path().join("lowered");
    fs::create_dir_all(&lowered).expect("create dir");
    for module in ["models", "main"] {
        fs::write(
            lowered.join(format!("{module}.py")),
            transpile(root, &format!("{module}.by")),
        )
        .expect("write lowered module");
    }

    let output = Command::new(&python)
        .arg("main.py")
        .current_dir(&lowered)
        .env("PYTHONPATH", root.join(site_packages).as_os_str())
        .output()
        .expect("failed to spawn python");
    assert!(
        output.status.success(),
        "lowered program failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "\
exact [('title', 'Left Hand')]
relation [('author__name', 'Ursula')]
pk [('pk', 1)]
membership [('title__in', ['a', 'b'])]
combined [('author__name', 'Ursula'), ('title', 'Left Hand')]
excluded [('title', 'x')]
after positional [('title', 'x')]
beside keyword [('author__name', 'b'), ('title', 'a')]
evaluated once [('title', 'v')]
json key [('data__key', 1)]
json nested [('data__a__b', 1)]
json index [('data__0', 1)]
json key operator [('data__key__gt', 1)]
json key and field [('data__key', 1), ('name', 'n')]
calls 1"
    );
}
