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
    let config = Config {
        min_version: PythonVersion::PY313,
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
        "transpiled conversion program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
