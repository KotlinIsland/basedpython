//! runtime test for the `?.` (optional chain) lowering
//!
//! `a?.b` lowers to a conditional expression that evaluates each receiver once,
//! binding a compound one to a temporary, and stops at the first `None`. the
//! transform's unit tests assert on the lowered *text*; this test runs it, with the
//! chain written where the lowering has to compose with what surrounds it: behind a
//! call on another chain, whose receiver it passes through as source, and where python
//! refuses the `:=` that binds the temporary — a comprehension's iterable, and any
//! part of a comprehension in a class body
//!
//! no third-party packages are needed, so the program is transpiled for whichever interpreter
//! runs it; if none is found the test skips, and says so, rather than fails

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
mod interpreters;
use common::python;

const CHAINS: &str = r#"
calls: list[str] = []

class Node:
    def __init__(self, value: int, items: list[int]):
        self.value = value
        self.items = items

    def me(self) -> Node?:
        calls.append("me")
        return self

    def nothing(self) -> Node?:
        return None

def make(present: bool) -> Node?:
    calls.append("make")
    return Node(1, [1, 2]) if present else None

# a chain behind a call on another chain
def through_a_call(n: Node?) -> int?:
    return n?.me()?.value

assert through_a_call(Node(3, [])) == 3 and through_a_call(None) is None
assert make(True)?.nothing()?.value is None

# in a comprehension's iterable
def iterable(present: bool) -> list[int]:
    return [x for x in make(present)?.items ?? []]

calls.clear()
assert iterable(True) == [1, 2] and calls == ["make"], "the receiver is evaluated once"
assert iterable(False) == []

def nested_iterable() -> list[int]:
    return [x for _ in [0] for x in make(True)?.me()?.items ?? []]

calls.clear()
assert nested_iterable() == [1, 2] and calls == ["make", "me"]

def in_a_display() -> list[list[int]?]:
    return [y for y in [make(True)?.items, make(False)?.items]]

assert in_a_display() == [[1, 2], None]

class Holder:
    let element: list[int?] = [make(present)?.value for present in [True, False]]
    let condition: list[int] = [k for k in [1, 2] if make(k == 1)?.value]

assert Holder.element == [1, None]
assert Holder.condition == [1]

# from a class body comprehension's first iterable, the chain's call still reads a
# builtin and an enclosing function's local, which the comprehension it moves into sees
class Picker:
    def __init__(self, items: list[int]):
        self.items = items

    def pick(self, f: (list[int]) -> int) -> list[int]:
        return [f(self.items)]

def picker() -> Picker?:
    return Picker([4, 5])

class Lengths:
    let picked: list[int] = [n for n in picker()?.pick(len) ?? []]

assert Lengths.picked == [2]

def outer() -> list[int]:
    def first(xs: list[int]) -> int:
        return xs[0]
    class Inner:
        let picked: list[int] = [n for n in picker()?.pick(first) ?? []]
    return Inner.picked

assert outer() == [4]

print("ok")
"#;

#[test]
fn optional_chains_run_correctly() {
    let Some(python) = python(PythonVersion::PY39) else {
        return;
    };

    let config = Config {
        min_version: python.version,
        ..Config::default()
    };
    let transpiled = transpile(CHAINS, &config).expect("transpile should succeed");
    let output = Command::new(&python.command)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled optional chain program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
