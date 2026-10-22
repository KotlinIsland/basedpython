//! Runtime test for the f-string `=` field, whose text is the author's own.
//!
//! Python renders `f"{expr = }"` by printing the source text between the `{` and
//! the `=` and then the value, so the text a lowered field prints is a claim about
//! the *source*, not about the lowering. Only an interpreter can settle it: the
//! transform's unit tests can say what text was emitted, but not what python makes
//! of it — a literal whose braces or backslashes were escaped wrongly still looks
//! plausible in the emitted source and prints something else.
//!
//! Every assertion here is written twice over: the basedpython program asserts what
//! the field prints, and [`PYTHON_REFERENCE`] asserts that the same expectation
//! holds for the plain-python spelling of the same field. So a wrong expectation
//! fails rather than being blessed.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
mod interpreters;
use interpreters::Interpreter;

/// basedpython whose `=` fields all hold an expression that lowers
const PROGRAM: &str = r#"
def show(a: int?, b: int, d: dict[str, int]) -> None:
    # a force-unwrap
    assert f"{(a!) = }" == "(a!) = 3", f"force-unwrap: {f'{(a!) = }'}"
    # a `??`
    assert f"{a ?? b = }" == "a ?? b = 3", "coalesce"
    # an optional chain
    assert f"{a?.bit_length() = }" == "a?.bit_length() = 2", "optional chain"
    # the author's own spacing, which python reproduces byte for byte
    assert f"{  (a!)   =  }" == "  (a!)   =  3", "spacing"
    # a format spec, which python prints instead of the repr
    assert f"{(a!)=:>6}" == "(a!)=     3", "format spec"
    # a conversion the field names for itself
    assert f"{(a!)=!s}" == "(a!)=3", "conversion"
    # braces in the author's text, which are not a field of their own
    assert f"{({1: a}[1]!) = }" == "({1: a}[1]!) = 3", "braces"
    # and a field nothing lowered, which keeps python's own compact rendering
    assert f"{b = }" == "b = 7", "unlowered"
    assert f"{ {1: 2}[1] = }" == " {1: 2}[1] = 2", "unlowered braces"

show(3, 7, {"k": 5})
print("ok")
"#;

/// the same fields with the basedpython operators taken out, so the expectations above
/// are checked against python's own rendering rather than against themselves
const PYTHON_REFERENCE: &str = r#"
def show(a, b, d):
    assert f"{(a) = }" == "(a) = 3", "force-unwrap"
    assert f"{  (a)   =  }" == "  (a)   =  3", "spacing"
    assert f"{(a)=:>6}" == "(a)=     3", "format spec"
    assert f"{(a)=!s}" == "(a)=3", "conversion"
    assert f"{({1: a}[1]) = }" == "({1: a}[1]) = 3", "braces"
    assert f"{b = }" == "b = 7", "unlowered"
    assert f"{ {1: 2}[1] = }" == " {1: 2}[1] = 2", "unlowered braces"

show(3, 7, {"k": 5})
print("ok")
"#;

/// fields only 3.12 lets a replacement field hold: a backslash, and the string's own quote
const PEP_701_PROGRAM: &str = r#"
def show(a: int?, d: dict[str, int]) -> None:
    # a backslash, which python prints as the two characters it was written as
    assert f"{('a\n'!) = }" == "('a\\n'!) = 'a\\n'", "backslash"
    # the string's own quote, nested
    assert f"{(d["k"]!) = }" == '(d["k"]!) = 5', "nested quote"

show(3, {"k": 5})
print("ok")
"#;

/// [`PEP_701_PROGRAM`] with the basedpython operators taken out
const PEP_701_REFERENCE: &str = r#"
def show(a, d):
    assert f"{('a\n') = }" == "('a\\n') = 'a\\n'", "backslash"
    assert f"{(d["k"]) = }" == '(d["k"]) = 5', "nested quote"

show(3, {"k": 5})
print("ok")
"#;

fn run(python: &Interpreter, source: &str, what: &str) {
    let output = Command::new(&python.command)
        .arg("-c")
        .arg(source)
        .output()
        .expect("failed to spawn python");
    assert!(
        output.status.success(),
        "{what} failed on {python}:\n--- stderr ---\n{}\n--- source ---\n{source}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

/// run `reference` on `python`, then `program` transpiled for it
fn run_both(python: &Interpreter, program: &str, reference: &str) {
    run(python, reference, "the plain-python reference");

    let config = Config {
        min_version: python.version,
        ..Config::default()
    };
    let transpiled = transpile(program, &config).expect("transpile should succeed");
    run(python, &transpiled, "the transpiled program");
}

#[test]
fn a_debug_field_prints_the_source_the_author_wrote() {
    let Some(python) = common::python(PythonVersion::PY39) else {
        return;
    };
    run_both(&python, PROGRAM, PYTHON_REFERENCE);
}

/// a replacement field may nest the string's own quote from 3.12 on, and hold a backslash from
/// the same release, so this program is written for 3.12. but until 3.12.9 and 3.13.2 python
/// printed a debug field's backslash decoded rather than as it was written (gh-124363), so the
/// reference is only python's word on a release that has the fix
#[test]
fn a_debug_field_holding_a_backslash_or_its_own_quote_prints_the_source() {
    let Some(python) = common::python_with(
        PythonVersion::PY312,
        "import sys; v = sys.version_info[:3]; assert v >= (3, 13, 2) or (3, 12, 9) <= v < (3, 13)",
        "that prints a debug field's text as it was written (3.12.9 or 3.13.2)",
    ) else {
        return;
    };
    run_both(&python, PEP_701_PROGRAM, PEP_701_REFERENCE);
}
