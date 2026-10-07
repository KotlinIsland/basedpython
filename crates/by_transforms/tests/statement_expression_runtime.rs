//! runtime test for the early exits a statement expression lowers to
//!
//! `x = value or return`, `x = value ?? raise E()` and `x = a if c else raise E()`
//! lower to statements: each operand is evaluated into a temporary on a line of its
//! own, and the diverging branch becomes the body of an `if`. the transform's unit
//! tests assert on the lowered *text*; this test runs it. an operand the source wrote
//! over several lines, inside parentheses that are not part of its own range, has to
//! keep a grouping when it moves onto its own line — without one, its second line
//! either fails to parse or parses as a statement of its own, and the program quietly
//! computes something else
//!
//! no third-party packages are needed, so any `python3` will do; if none is found the
//! test skips rather than fails

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

/// operands written over several lines, one per early-exit form
const MULTI_LINE_OPERANDS: &str = r#"
class Units:
    def __init__(self, n: int):
        self.n = n

    def of_type(self, *types: int) -> Units:
        return self

    def excluding(self, n: int) -> Units:
        return Units(self.n - n)

    def __bool__(self) -> bool:
        return self.n > 0

# a parenthesised method chain, one link per line
def army(units: Units, types: list[int]) -> int:
    var chosen = (
        units.of_type(*types)
        .excluding(1)
        .excluding(1)
    ) or return -1
    return chosen.n

assert army(Units(5), [1]) == 3, "a present chain is the value"
assert army(Units(2), [1]) == -1, "an empty chain returns"

# a continuation line that reads as a statement on its own: `+ 1` is a unary plus
def doubled(a: int) -> int:
    x = (
        a
        + 1
    ) or return 0
    return x * 2

assert doubled(1) == 4, "the whole operand is the value, not its first line"
assert doubled(-1) == 0

# `and return` takes the same path
def zero(a: int) -> str:
    x = (
        a
        - 1
    ) and return "nonzero"
    return f"zero {x}"

assert zero(1) == "zero 0" and zero(3) == "nonzero"

# `or raise`
def size(xs: list[int]) -> int:
    n = (len(xs)
         + 0) or raise ValueError("empty")
    return n

assert size([1, 2]) == 2
try:
    _ = size([])
except ValueError:
    pass
else:
    raise AssertionError("an empty list raises")

# `?? raise` with an operand that is a chain of attribute reads
class Node:
    def __init__(self, value: int?):
        self.value = value
        self.inner = self

def value_of(node: Node) -> int:
    v = (
        node
        .inner
        .value
    ) ?? raise KeyError("absent")
    return v

assert value_of(Node(4)) == 4
try:
    _ = value_of(Node(None))
except KeyError:
    pass
else:
    raise AssertionError("an absent value raises")

# a conditional expression whose test and value both span lines
def positive(a: int) -> int:
    x = (
        a
        + 1
    ) if (
        a
        > 0
    ) else raise ValueError(a)
    return x

assert positive(1) == 2, "the value is the whole operand"
try:
    _ = positive(-1)
except ValueError:
    pass
else:
    raise AssertionError("a failed test raises")

print("ok")
"#;

fn run(python: &str, program: &str) {
    let config = Config {
        min_version: PythonVersion::PY313,
        ..Config::default()
    };
    let transpiled = transpile(program, &config).expect("transpile should succeed");
    let output = Command::new(python)
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
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn an_operand_written_over_several_lines_keeps_its_grouping() {
    let Some(python) = python() else {
        eprintln!("skipping statement expression runtime test: no `python3` interpreter found");
        return;
    };
    run(&python, MULTI_LINE_OPERANDS);
}
