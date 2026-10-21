//! Runtime test for forward references in annotations python evaluates as the definition runs,
//! and in the types written outside an annotation that it evaluates where they stand
//!
//! basedpython resolves every annotation as deferred, so a signature may name a class defined
//! further down, the class it sits in, or a name imported only for the checker. Before 3.14
//! python evaluates those annotations as the `def` or the class body runs, and an unquoted
//! forward reference raises `NameError` at import — which asserting on the lowered text cannot
//! see.
//!
//! The annotation test needs an interpreter older than 3.14: from 3.14 annotations are deferred
//! natively, so there is nothing to observe, and it skips rather than pass without checking
//! anything
//!
//! Below 3.10 the output defers every annotation with `from __future__ import annotations`
//! instead, and there a quote would reach `__annotations__` as a string inside a string, which
//! a library reading the annotation as text takes for something else.

use std::process::Command;

use by_transforms::{Config, PythonVersion, reverse_transpile, transpile};

mod common;
use common::python;

mod interpreters;

/// every place an annotation runs with the definition, naming something bound after it
const PROGRAM: &str = r#"
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections import OrderedDict


def make() -> Later:
    return Later()


class Node:
    parent: Node?

    def child(self, other: Node) -> list[Node]:
        return [other]

    def later(self) -> Later?:
        return None


value: Later


def ordered(counts: OrderedDict[str, int]) -> None: ...


class Later: ...


assert type(make()) == Later, "a function above the class it returns runs"
assert len(Node().child(Node())) == 1, "so does a method naming its own class"
print("ok")
"#;

/// whether `python` still evaluates annotations as the definition runs
fn evaluates_annotations_eagerly(python: &str) -> bool {
    Command::new(python)
        .args([
            "-c",
            "import sys; raise SystemExit(sys.version_info >= (3, 14))",
        ])
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn a_forward_reference_does_not_raise_at_import() {
    let Some(python) = python() else {
        return;
    };
    if !evaluates_annotations_eagerly(&python) {
        eprintln!("skipping: {python} defers annotations natively, so nothing is evaluated early");
        return;
    }
    let config = Config {
        min_version: PythonVersion::PY310,
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

/// a forward reference in a module transpiled for 3.9, which defers every annotation with
/// `from __future__ import annotations`. quoted as well, `ClassVar[Later]` reached
/// `__annotations__` as `"'ClassVar[Later]'"`, which `dataclasses` does not read as a class
/// variable, so it became a field the constructor required
const DEFERRED_BY_THE_FUTURE_IMPORT: &str = r#"
from dataclasses import dataclass, fields
from typing import ClassVar, TypedDict, get_type_hints


def make(x: Later) -> Later:
    return x


class Holder:
    item: Later


@dataclass
class Record:
    shared: ClassVar[Later]
    item: Later


class Shape(TypedDict):
    item: Later


class Later: ...


assert make.__annotations__ == {"x": "Later", "return": "Later"}, make.__annotations__
assert Holder.__annotations__ == {"item": "Later"}, Holder.__annotations__
assert [field.name for field in fields(Record)] == ["item"], fields(Record)
for target, name in ((make, "x"), (Holder, "item"), (Record, "item"), (Shape, "item")):
    assert get_type_hints(target)[name] == Later, get_type_hints(target)
print("ok")
"#;

#[test]
fn a_forward_reference_under_the_future_import_is_not_quoted() {
    let target = PythonVersion::PY39;
    let Some(interpreter) = interpreters::oldest(target, "", "") else {
        return;
    };
    let config = Config {
        min_version: target,
        ..Config::default()
    };
    let transpiled =
        transpile(DEFERRED_BY_THE_FUTURE_IMPORT, &config).expect("transpile should succeed");
    let output = Command::new(&interpreter.command)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "program transpiled for {target} failed on {interpreter}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    interpreters::ran(&interpreter, target);
}

/// a type written outside an annotation that python evaluates where it stands, naming classes
/// and an import bound only further down. the checker reads the type arguments as deferred, so
/// the transpiler quotes them, and a name a quote reads is bound where it is imported rather
/// than when the module first reads it, since a quote is evaluated against the module's
/// namespace
const OUTSIDE_AN_ANNOTATION: &str = r#"
from typing import Callable, NamedTuple, TypeVar, Union, get_args, get_type_hints

items = list[Later]()
Alias = dict[str, Later]
Maybe = Union[Later, None]
Handler = Callable[[Later], Later]
T = TypeVar("T", bound=Later)
Pair = NamedTuple("Pair", [("first", Later)])
Counts = list[OrderedDict]
Pairs = list[(Later, int)]
pair_map = dict[str, (Later, int)]()
P = TypeVar("P", bound=(Later, int))
MaybeList = list[Later?]
maybes = list[Later?]()
HandlerList = list[(Later) -> None]
Method = list[int.() -> Later]
Fields = list[(first: Later, count: int)]
TupleParams = Callable[(Later, int), Later]
annotated: list[(first: Later, count: int)] = []


class Base[S]: ...


class Bar(Base[Later]): ...


class PairBar(Base[(Later, int)]): ...


class MaybeBar(Base[Later?]): ...


class Later: ...


from collections import OrderedDict


# a builtin generic keeps a string argument as it is, where `typing` makes it a `ForwardRef`
def resolve(reference: object) -> object:
    text = reference if isinstance(reference, str) else reference.__forward_arg__
    return eval(text, globals())


assert resolve(get_args(Alias)[1]) == Later, get_args(Alias)
assert resolve(get_args(Maybe)[0]) == Later, get_args(Maybe)
assert resolve(get_args(Handler)[1]) == Later, get_args(Handler)
assert resolve(T.__bound__) == Later, T.__bound__
assert resolve(get_args(Bar.__orig_bases__[0])[0]) == Later, Bar.__orig_bases__
assert resolve(get_args(Counts)[0]) == OrderedDict, get_args(Counts)
assert resolve(get_args(Pairs)[0]) == tuple[Later, int], get_args(Pairs)
assert resolve(P.__bound__) == tuple[Later, int], P.__bound__
assert resolve(get_args(PairBar.__orig_bases__[0])[0]) == tuple[Later, int], PairBar.__orig_bases__
assert resolve(get_args(MaybeList)[0]) == Later | None, get_args(MaybeList)
assert resolve(get_args(MaybeBar.__orig_bases__[0])[0]) == Later | None, MaybeBar.__orig_bases__
assert resolve(get_args(HandlerList)[0]) == Callable[[Later], None], get_args(HandlerList)
assert resolve(get_args(Method)[0]) == Callable[[int], Later], get_args(Method)
params, returns = get_args(TupleParams)
assert [resolve(params[0]), params[1], resolve(returns)] == [Later, int, Later], get_args(TupleParams)
fields = resolve(get_args(Fields)[0])
assert get_type_hints(fields) == {"first": Later, "count": int}, get_type_hints(fields)
print("ok")
"#;

#[test]
fn a_forward_reference_outside_an_annotation_resolves() {
    let Some(python) = python() else {
        return;
    };
    let config = Config {
        min_version: PythonVersion::PY310,
        ..Config::default()
    };
    let transpiled = transpile(OUTSIDE_AN_ANNOTATION, &config).expect("transpile should succeed");
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

/// python that spells a forward reference outside an annotation as a string, in each place the
/// checker reads one as deferred: an alias's type argument, a union's, a callable's, an
/// optional's, a type variable's bound and a base's. the reverse transpile writes each as the
/// name it spells, and the forward transpile quotes it again, so python sees the same types
const QUOTED_OUTSIDE_AN_ANNOTATION: &str = r#"
from typing import Callable, Generic, List, Optional, TypeVar, Union, get_args, get_type_hints

T = TypeVar("T")


class Foo(Generic[T]):
    pass


Alias = List["Later"]
UAlias = Union["Later", int]
CAlias = Callable[["Later"], "Later"]
MaybeLater = Optional["Later"]
B = TypeVar("B", bound="Later")


class Bar(Foo["Later"]):
    pass


class Later:
    pass


def uses(a: Alias, u: UAlias, c: CAlias, m: MaybeLater, b: B) -> Bar: ...


print(get_args(Alias), get_args(UAlias), get_args(CAlias), get_args(MaybeLater))
print(B.__bound__, Bar.__orig_bases__)
print(get_type_hints(uses))
"#;

/// what `program` prints on `python`
fn printed(python: &str, program: &str) -> String {
    let output = Command::new(python)
        .arg("-c")
        .arg(program)
        .output()
        .expect("failed to spawn python");
    assert!(
        output.status.success(),
        "program failed on {python}:\n--- stderr ---\n{}\n--- program ---\n{program}",
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_forward_reference_outside_an_annotation_survives_the_round_trip() {
    let Some(python) = python() else {
        return;
    };
    let config = Config {
        min_version: PythonVersion::PY310,
        ..Config::default()
    };
    let by = reverse_transpile(QUOTED_OUTSIDE_AN_ANNOTATION, &config)
        .expect("reverse transpile should succeed");
    assert!(
        !by.contains("\"Later\""),
        "a forward reference should be the name it spells:\n{by}"
    );
    let back = transpile(&by, &config).expect("transpile should succeed");
    assert_eq!(
        printed(&python, &back),
        printed(&python, QUOTED_OUTSIDE_AN_ANNOTATION),
        "--- basedpython ---\n{by}\n--- transpiled back ---\n{back}"
    );
}
