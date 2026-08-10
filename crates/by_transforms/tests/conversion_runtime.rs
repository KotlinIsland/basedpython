//! Runtime test for the conversion dunders — the half the text-level and
//! type-level tests cannot reach.
//!
//! The unit tests check the *lowered text* and the mdtest checks the *types*;
//! what neither can show is that the emitted call actually runs and produces the
//! converted value. Every claim exercised here is one a wrong lowering would
//! break while the emitted text still looked plausible:
//!
//! - `T.__from__(x)` really binds `cls` to `T` and `x` to the value — a
//!   `__from__` emitted for a non-classmethod would silently swap them
//! - `x.__into__()` runs against the value itself, and the parentheses hold for
//!   an operand of any precedence
//! - `T.__of__(literal)` converts the written-out value, including a display
//!   whose own elements are computed
//! - every conversion site converts at runtime: a call argument, an annotated
//!   assignment, a plain assignment to a name declared elsewhere, an attribute
//!   assignment, a `return`, and element-wise inside a literal collection

use std::process::{Command, Stdio};

use by_transforms::{Config, PythonVersion, transpile};

/// basedpython whose module-level `assert`s exercise the three dunders end to end
const PROGRAM: &str = r#"
class Celsius:
    def __init__(self, degrees: float):
        self.degrees = degrees

class Kelvin:
    def __init__(self, degrees: float):
        self.degrees = degrees

class Fahrenheit:
    def __init__(self, degrees: float):
        self.degrees = degrees

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls(value.degrees * 9 / 5 + 32)

class Meters:
    def __init__(self, value: float):
        self.value = value

    @classmethod
    def __of__(cls, value: int | float) -> Self:
        return cls(float(value))

class Sized:
    def __init__(self, count: int):
        self.count = count

    @classmethod
    def __of__(cls, value: list[int]) -> Self:
        return cls(len(value))

class Rankine:
    def __init__(self, degrees: float):
        self.degrees = degrees

class Reaumur:
    def __init__(self, degrees: float):
        self.degrees = degrees

    def __into__(self) -> Rankine:
        return Rankine(self.degrees * 9 / 4 + 491.67)

def report(t: Fahrenheit) -> float:
    return t.degrees

def rankine(r: Rankine) -> float:
    return r.degrees

# `__from__`, at a call argument: `cls` is the target, the value is the argument
assert report(Celsius(100.0)) == 212.0, report(Celsius(100.0))

# an annotated assignment, and the converted object really is the target type
boiling: Fahrenheit = Celsius(100.0)
assert type(boiling).__name__ == "Fahrenheit"
assert boiling.degrees == 212.0

# a plain assignment to a name declared in an earlier statement
later: Fahrenheit = Fahrenheit(0.0)
later = Celsius(100.0)
assert type(later).__name__ == "Fahrenheit"
assert later.degrees == 212.0

# a `return`
def to_f(c: Celsius) -> Fahrenheit:
    return c

assert to_f(Celsius(0.0)).degrees == 32.0

# an attribute assignment
class Reading:
    temperature: Fahrenheit

reading = Reading()
reading.temperature = Celsius(0.0)
assert reading.temperature.degrees == 32.0

# `__into__`, on an operand that needs the parentheses the lowering emits
assert rankine(Reaumur(0.0)) == 491.67, rankine(Reaumur(0.0))

# `__of__` on a scalar literal, and on a display whose elements are computed
def compute() -> int:
    return 7

length: Meters = 5
assert type(length).__name__ == "Meters"
assert length.value == 5.0

counted: Sized = [1, 2, compute()]
assert type(counted).__name__ == "Sized"
assert counted.count == 3

# element-wise inside a literal collection: each element converts where it stands
lengths: list[Meters] = [1, 2, 3]
assert [m.value for m in lengths] == [1.0, 2.0, 3.0]
assert all(type(m).__name__ == "Meters" for m in lengths)

temperatures: list[Fahrenheit] = [Celsius(0.0), Celsius(100.0)]
assert [t.degrees for t in temperatures] == [32.0, 212.0]

mapping: dict[str, Fahrenheit] = {"boiling": Celsius(100.0)}
assert mapping["boiling"].degrees == 212.0

print("ok")
"#;

/// the frozen container displays, whose conversion comes from the prelude rather
/// than from a dunder the program declares.
///
/// The text-level test shows the wrap; only running it shows that the wrap
/// produces a *frozen* object rather than the display's own kind, which is the
/// one thing a plausible-looking lowering would get wrong. `frozendict` is left
/// to [`FROZENDICT_PROGRAM`] because it is a 3.15 builtin
const FROZEN_PROGRAM: &str = r#"
class Path:
    def __init__(self, raw: str):
        self.raw = raw

extension Path:
    class def __of__(cls, value: str) -> Path:
        return Path(value)

def takes(fs: frozenset[str]) -> int:
    return len(fs)

# a set display, at an annotated assignment and at a call argument
b: frozenset[int] = {1, 2}
assert type(b).__name__ == "frozenset", type(b).__name__
assert sorted(b) == [1, 2]
assert takes({"a", "b"}) == 2

# `{}` is the empty set where a set is asked for, and stays a dict where one is
e: frozenset[int] = {}
assert type(e).__name__ == "frozenset", type(e).__name__
assert len(e) == 0

d: set[int] = {}
assert type(d).__name__ == "set", type(d).__name__
assert len(d) == 0

plain: dict[str, int] = {}
assert type(plain).__name__ == "dict", type(plain).__name__

# a `return`, and an element of another display
def gives() -> frozenset[int]:
    return {1, 2}

assert type(gives()).__name__ == "frozenset"

# an empty display in a return position, for both the mutable and frozen kinds
def empty_set() -> set[int]:
    return {}

def empty_frozen() -> frozenset[int]:
    return {}

assert type(empty_set()).__name__ == "set", type(empty_set()).__name__
assert len(empty_set()) == 0
assert type(empty_frozen()).__name__ == "frozenset", type(empty_frozen()).__name__
assert len(empty_frozen()) == 0

nested: list[frozenset[int]] = [{1}, {2}]
assert all(type(f).__name__ == "frozenset" for f in nested)
assert [sorted(f) for f in nested] == [[1], [2]]

# an empty display is constructed with no argument at all, and must still work
# where it sits beside a wrapped one in the same edit
mixed: list[frozenset[int]] = [{}, {1}]
assert all(type(f).__name__ == "frozenset" for f in mixed)
assert [sorted(f) for f in mixed] == [[], [1]]

# an `extension` supplying `__of__` for a type it does not own: the lowered call
# is the backing function, and it must receive the class as its `cls`
p: Path = "/tmp/y"
assert type(p).__name__ == "Path", type(p).__name__
assert p.raw == "/tmp/y"

# the prelude's dunder written out by hand: it is not a runtime attribute, so
# leaving the call alone would raise `AttributeError` here
h = frozenset.__of__({1, 2})
assert type(h).__name__ == "frozenset", type(h).__name__
assert sorted(h) == [1, 2]

print("ok")
"#;

/// the same for `frozendict`, which only exists on python 3.15
const FROZENDICT_PROGRAM: &str = r#"
a: frozendict[str, str] = {}
assert type(a).__name__ == "frozendict", type(a).__name__
assert len(a) == 0

c: frozendict[str, int] = {"x": 1}
assert type(c).__name__ == "frozendict", type(c).__name__
assert c["x"] == 1

print("ok")
"#;

/// the first interpreter that accepts `probe`
fn python_supporting(probe: &str) -> Option<String> {
    let mut candidates = Vec::new();
    if let Ok(p) = std::env::var("PYTHON") {
        candidates.push(p);
    }
    candidates.extend(["python3.13", "python3"].map(String::from));

    candidates.into_iter().find(|py| {
        Command::new(py)
            .args(["-c", probe])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// transpile `program` and run it, asserting it exits cleanly
fn run_transpiled(python: &str, program: &str, min_version: PythonVersion) {
    let config = Config {
        min_version,
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
        "transpiled conversion program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn conversions_run_correctly() {
    let Some(python) = python_supporting("import sys; sys.exit(0)") else {
        eprintln!("skipping conversion runtime test: no python interpreter found");
        return;
    };
    run_transpiled(&python, PROGRAM, PythonVersion::PY313);
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn frozen_displays_run_correctly() {
    let Some(python) = python_supporting("import sys; sys.exit(0)") else {
        eprintln!("skipping frozen display runtime test: no python interpreter found");
        return;
    };
    run_transpiled(&python, FROZEN_PROGRAM, PythonVersion::PY313);
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn frozendict_displays_run_correctly() {
    let Some(python) = python_supporting("frozendict") else {
        eprintln!("skipping frozendict runtime test: no interpreter with `frozendict` (3.15+)");
        return;
    };
    run_transpiled(&python, FROZENDICT_PROGRAM, PythonVersion::PY315);
}
