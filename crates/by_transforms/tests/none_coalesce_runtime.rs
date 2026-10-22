//! runtime test for the `??` (none-coalesce) lowering
//!
//! `a ?? b` lowers to a conditional expression that evaluates `a` once, binding it to
//! a temporary when it is more than a name, and evaluates `b` only when `a` is `None`.
//! the transform's unit tests assert on the lowered *text*; this test runs it, with
//! the `??` written where the lowering has to compose with what surrounds it:
//!
//! - inside an operand of another `??` — a receiver, an argument, a subscript, an
//!   f-string — where it is lowered by an edit of its own
//! - as an operand of `and` / `or`, or the test or value of a conditional
//!   expression, where python reads an unparenthesized conditional as something else
//! - where python refuses `:=`: a comprehension's iterable, and any part of a
//!   comprehension in a class body
//!
//! no third-party packages are needed, so the program is transpiled for whichever interpreter
//! runs it; if none is found the test skips, and says so, rather than fails

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
mod interpreters;
use common::python;
use interpreters::Interpreter;

/// a `??` inside an operand of another one
const NESTED: &str = r#"
calls: list[str] = []

def probe(tag: str, value: int?) -> int?:
    calls.append(tag)
    return value

def lookup(a: dict[int, list[int]]?, k: int) -> list[int]:
    return (a ?? {}).get(k) ?? []

assert lookup(None, 1) == [] and lookup({1: [2]}, 1) == [2] and lookup({}, 1) == []

def right(a: int?, b: int?) -> int:
    return a ?? int(str(probe("b", b) ?? 4))

assert right(None, None) == 4 and right(1, None) == 1

def indexed(xs: list[int]?, i: int?) -> int:
    return (xs ?? [9])[i ?? 0]

assert indexed(None, None) == 9 and indexed([1, 2], 1) == 2

def add(a: int, b: int) -> int:
    return a + b

def argument(x: dict[int, int]?, y: int?) -> int:
    return add((x ?? {}).get(1) ?? 0, b=y ?? 0)

assert argument(None, None) == 0 and argument({1: 5}, 2) == 7

def formatted(x: int?, d: dict[int, int]?) -> str:
    return f"{x ?? 1}-{(d ?? {}).get(1) ?? 2}"

assert formatted(None, None) == "1-2"

# a chain evaluates each left operand once, in order, and stops at the first that
# is not `None`
def chained(a: int?, b: int?, c: int?) -> int:
    return probe("a", a) ?? probe("b", b) ?? probe("c", c) ?? 0

calls.clear()
assert chained(None, 2, 3) == 2 and calls == ["a", "b"]
calls.clear()
assert chained(None, None, None) == 0 and calls == ["a", "b", "c"]

# a unary operator is a call of its operand's method
class Negated:
    def __init__(self):
        self.calls = 0

    def __neg__(self) -> int?:
        self.calls += 1
        return 5

negated = Negated()
assert (-negated ?? 3) == 5 and negated.calls == 1, "the left operand is evaluated once"

print("ok")
"#;

/// a `??` where python reads an unparenthesized conditional expression as something
/// else
const GROUPING: &str = r#"
def body(a: int?, c: bool) -> int:
    return a ?? 1 if c else 2

assert body(None, True) == 1
assert body(5, False) == 2, "the condition picks between the `??` and 2"

def operand(a: int?, b: int) -> int:
    return a ?? 0 or b

assert operand(5, 9) == 5
assert operand(0, 9) == 9, "`or` applies to the coalesced value"

def test(a: bool?) -> int:
    return 1 if a ?? True else 2

assert test(None) == 1 and test(False) == 2

def kept(d: dict[int, bool], ks: list[int]) -> list[int]:
    return [k for k in ks if d.get(k) ?? True]

assert kept({1: False}, [1, 2]) == [2]

def unpacked(a: list[int]?, b: list[int], d: dict[str, int]?) -> tuple[list[int], dict[str, int]]:
    return [*a ?? b], {**d ?? {"k": 1}}

assert unpacked(None, [2], None) == ([2], {"k": 1})

print("ok")
"#;

/// a `??` where python refuses `:=`, and next to it the places it accepts one
const BINDING: &str = r#"
from collections.abc import Callable

table: dict[int, list[int]] = {1: [2]}
flags: dict[int, bool] = {1: False}
values: dict[int, int?] = {1: 5}

def iterable(k: int) -> list[int]:
    return [x for x in table.get(k) ?? []]

assert iterable(1) == [2] and iterable(3) == []

def nested_iterable(ks: list[int]) -> list[int]:
    return [x for k in ks for x in table.get(k) ?? [0]]

assert nested_iterable([1, 3]) == [2, 0]

def generator(k: int) -> int:
    return sum(x for x in table.get(k) ?? [])

assert generator(1) == 2 and generator(3) == 0

def displays(k: int) -> tuple[set[int], dict[int, int]]:
    return {x for x in table.get(k) ?? []}, {x: x for x in table.get(k) ?? [7]}

assert displays(3) == (set(), {7: 7})

def lambda_in_iterable() -> list[int]:
    return [x for x in (lambda: table.get(3) ?? [1])()]

assert lambda_in_iterable() == [1]

def parts(ks: list[int]) -> list[int]:
    return [values.get(k) ?? -1 for k in ks if flags.get(k) ?? True]

assert parts([1, 2]) == [-1]

def counted(ks: list[int]) -> int:
    return sum(values.get(k) ?? 0 for k in ks)

assert counted([1, 2]) == 5

class Holder:
    let direct: int = values.get(2) ?? 3
    let element: list[int] = [values.get(k) ?? 0 for k in [1, 2]]
    let condition: list[int] = [k for k in [1, 2] if flags.get(k) ?? True]
    let from_iterable: list[int] = [x for x in table.get(9) ?? [direct]]
    let in_lambda: list[int] = [(lambda: values.get(k) ?? 0)() for k in [1, 2]]

assert Holder.direct == 3
assert Holder.element == [5, 0]
assert Holder.condition == [2]
assert Holder.from_iterable == [3], "the fallback still sees the class body's names"
assert Holder.in_lambda == [5, 0]

def scaled(n: int) -> Callable[[Callable[[], int]], Callable[[], int]]:
    def wrap(fn: Callable[[], int]) -> Callable[[], int]:
        return lambda: fn() * n
    return wrap

@scaled(values.get(2) ?? 10)
def decorated(x: int = values.get(2) ?? 1) -> int:
    return x

assert decorated() == 10

print("ok")
"#;

/// a `??` in an annotation and in the scope of a generic definition's type parameters,
/// where python refuses `:=` too
const ANNOTATION_SCOPES: &str = r#"
from typing import Annotated

table: dict[str, int] = {}

class Base:
    pass

def base(n: int) -> type[Base]:
    return Base

class Generic[T](base(table.get("a") ?? 0)):
    pass

annotated: Annotated[int, table.get("b") ?? 1] = 1

def parameter(y: Annotated[int, table.get("c") ?? 2]):
    pass

type Alias = Annotated[int, table.get("d") ?? 3]

assert issubclass(Generic, Base) and annotated == 1

print("ok")
"#;

fn run(python: &Interpreter, program: &str) {
    let config = Config {
        min_version: python.version,
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
        "transpiled program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

#[test]
fn a_coalesce_inside_an_operand_is_lowered() {
    let Some(python) = python(PythonVersion::PY39) else {
        return;
    };
    run(&python, NESTED);
}

#[test]
fn a_coalesce_keeps_its_grouping() {
    let Some(python) = python(PythonVersion::PY39) else {
        return;
    };
    run(&python, GROUPING);
}

#[test]
fn a_coalesce_binds_its_value_where_python_refuses_an_assignment_expression() {
    let Some(python) = python(PythonVersion::PY39) else {
        return;
    };
    run(&python, BINDING);
}

#[test]
fn a_coalesce_binds_its_value_in_an_annotation_and_a_type_parameter_scope() {
    // below 3.12 the `type` statement is written by the generics polyfill, which does not yet
    // carry a `??` lowered inside the alias's value: the transpile is refused as a transform
    // conflict
    let Some(python) = python(PythonVersion::PY312) else {
        return;
    };
    run(&python, ANNOTATION_SCOPES);
}
