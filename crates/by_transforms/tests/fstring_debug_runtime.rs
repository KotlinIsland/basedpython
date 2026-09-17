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
    # a backslash, which python prints as the two characters it was written as
    assert f"{('a\n'!) = }" == "('a\\n'!) = 'a\\n'", "backslash"
    # the string's own quote, nested the way 3.12 allows
    assert f"{(d["k"]!) = }" == '(d["k"]!) = 5', "nested quote"
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
    assert f"{('a\n') = }" == "('a\\n') = 'a\\n'", "backslash"
    assert f"{(d["k"]) = }" == '(d["k"]) = 5', "nested quote"
    assert f"{b = }" == "b = 7", "unlowered"
    assert f"{ {1: 2}[1] = }" == " {1: 2}[1] = 2", "unlowered braces"

show(3, 7, {"k": 5})
print("ok")
"#;

fn run(python: &str, source: &str, what: &str) {
    let output = Command::new(python)
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

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn a_debug_field_prints_the_source_the_author_wrote() {
    let Some(python) = common::python() else {
        eprintln!("skipping f-string `=` field runtime test: no interpreter found");
        return;
    };
    // a replacement field may nest the string's own quote from 3.12 on, and hold a
    // backslash from the same release
    let probe = Command::new(&python)
        .args([
            "-c",
            "import sys; sys.exit(0 if sys.version_info >= (3, 12) else 1)",
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !probe {
        eprintln!("skipping f-string `=` field runtime test: needs python 3.12 or later");
        return;
    }

    run(&python, PYTHON_REFERENCE, "the plain-python reference");

    let config = Config {
        min_version: PythonVersion::PY313,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");
    run(&python, &transpiled, "the transpiled program");
}
