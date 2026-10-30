//! Runtime test for a `raise` written in the middle of an expression.
//!
//! The text-level tests show the call it becomes; what they cannot show is that the call behaves
//! like the statement: the same exception with the same cause, a bare `raise` still re-raising what
//! is being handled, and what was written before it evaluated while what comes after it is not.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
mod interpreters;

/// basedpython whose module-level `assert`s exercise `raise` in each kind of expression
const PROGRAM: &str = r#"
evaluated: list[str] = []

def note(name: str) -> str:
    evaluated.append(name)
    return name

def caught(fn: () -> object) -> BaseException:
    try:
        fn()
    except BaseException as error:
        return error
    raise AssertionError("did not raise")

def in_argument() -> None:
    print(note("before"), raise ValueError("v"), note("after"))

error = caught(in_argument)
assert error is ValueError and error.args == ("v",)
assert evaluated == ["before"], evaluated

def a_class() -> None:
    print(raise KeyError)

assert caught(a_class) is KeyError

def with_a_cause() -> None:
    print(raise ValueError("v") from TypeError("t"))

error = caught(with_a_cause)
assert error is ValueError and error.__cause__ is TypeError

def with_no_cause() -> None:
    try:
        raise TypeError("t")
    except TypeError:
        print(raise ValueError("v") from None)

error = caught(with_no_cause)
assert error.__cause__ is None and error.__suppress_context__

def re_raises() -> None:
    try:
        raise ValueError("v")
    except ValueError:
        print(raise)

assert caught(re_raises) is ValueError

def nothing_to_re_raise() -> None:
    print(raise)

assert caught(nothing_to_re_raise) is RuntimeError

def not_an_exception() -> None:
    print(raise None)

assert caught(not_an_exception) is TypeError

in_lambda = lambda: raise ValueError("lambda")
assert caught(in_lambda).args == ("lambda",)

def in_comprehension(items: list[int | None]) -> list[int]:
    return [item ?? raise ValueError("comprehension") for item in items]

assert in_comprehension([1, 2]) == [1, 2]
assert caught(lambda: in_comprehension([1, None])).args == ("comprehension",)

def in_condition() -> None:
    if raise ValueError("condition"):
        pass

assert caught(in_condition).args == ("condition",)

def in_fstring() -> None:
    print(f"{raise ValueError('fstring')}")

assert caught(in_fstring).args == ("fstring",)

def nested_under_coalesce(a: int | None) -> object:
    return a ?? print(raise ValueError("nested"))

assert nested_under_coalesce(1) == 1
assert caught(lambda: nested_under_coalesce(None)).args == ("nested",)

print("ok")
"#;

#[test]
fn raise_in_an_expression_behaves_like_the_statement() {
    let Some(python) = common::python(PythonVersion::PY39) else {
        return;
    };
    let config = Config {
        min_version: python.version,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");

    let output = Command::new(&python.command)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled raise-expression program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
