//! Runtime test for a t-string field's `expression`, whose text is the author's own.
//!
//! Python 3.14 hands every replacement field of a `t"..."` to the program as a
//! `string.templatelib.Interpolation` whose `expression` is the *source text* of
//! that field, so the text a lowered field reports is a claim about the *source*,
//! not about the lowering. Only an interpreter can settle it: the transform's unit
//! tests can say what was emitted, but not what python makes of it — and the exact
//! span python reports (its leading whitespace, its grouping parentheses, its
//! dropped trailing whitespace) is python's own rule, which is easy to state
//! wrongly from the outside.
//!
//! Every assertion here is written twice over: the basedpython program asserts what
//! the field reports, and [`PYTHON_REFERENCE`] asserts that the same expectation
//! holds for the plain-python spelling of the same field. So a wrong expectation
//! fails rather than being blessed.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

/// basedpython whose t-string fields hold expressions that lower
const PROGRAM: &str = r#"
from string.templatelib import Template

def show(a: int?, b: int, d: dict[str, int]) -> None:
    def text(t) -> list:
        return [i.expression for i in t.interpolations]

    # a force-unwrap
    assert text(t"{a!}") == ["a!"], text(t"{a!}")
    # a `??`
    assert text(t"{a ?? b}") == ["a ?? b"], text(t"{a ?? b}")
    # an optional chain
    assert text(t"{a?.bit_length()}") == ["a?.bit_length()"], text(t"{a?.bit_length()}")
    # `typeof`, which is folded in the syntax tree rather than emitted as an edit
    assert text(t"{typeof(b)}") == ["typeof(b)"], text(t"{typeof(b)}")
    # the author's own spacing and grouping parentheses, which python keeps
    assert text(t"{  (a!)  }") == ["  (a!)"], text(t"{  (a!)  }")
    # a conversion and a format spec end the reported text where the brace would
    assert text(t"{ (a!) !r:>6}") == [" (a!)"], text(t"{ (a!) !r:>6}")
    # a field nothing lowered keeps the interpolation python built for it
    assert text(t"{b} {a!} { b }") == ["b", "a!", " b"], text(t"{b} {a!} { b }")
    # the values and the rendering are the ones the lowering produced
    assert t"{a!}".values == (3,), t"{a!}".values
    assert t"{ (a!) !r:>6}".values == (3,), "spec values"
    assert t"{a!}".strings == ("", ""), t"{a!}".strings
    # a `=` field, which python itself splits into literal text and a field beside it
    assert list(t"{(a!) = }")[0] == "(a!) = ", list(t"{(a!) = }")[0]
    assert text(t"{(a!) = }") == ["(a!)"], text(t"{(a!) = }")
    # a template nothing lowered is left exactly as written
    assert text(t"{b}") == ["b"], text(t"{b}")
    # a string tag is handed the template with the same texts
    assert text(same"{a!}") == ["a!"], text(same"{a!}")
    assert text(same"{b} {a ?? b}") == ["b", "a ?? b"], text(same"{b} {a ?? b}")
    assert text(same"{b}") == ["b"], text(same"{b}")

def same(t: Template) -> Template:
    return t

show(3, 7, {"k": 5})
print("ok")
"#;

/// the same fields with the basedpython operators taken out, so the expectations above
/// are checked against python's own reporting rather than against themselves
const PYTHON_REFERENCE: &str = r#"
def show(a, b, d):
    def text(t):
        return [i.expression for i in t.interpolations]

    assert text(t"{a}") == ["a"], text(t"{a}")
    assert text(t"{  (a)  }") == ["  (a)"], text(t"{  (a)  }")
    assert text(t"{ (a) !r:>6}") == [" (a)"], text(t"{ (a) !r:>6}")
    assert text(t"{b} {a} { b }") == ["b", "a", " b"], text(t"{b} {a} { b }")
    assert t"{a}".values == (3,), t"{a}".values
    assert t"{a}".strings == ("", ""), t"{a}".strings
    assert list(t"{(a) = }")[0] == "(a) = ", list(t"{(a) = }")[0]
    assert text(t"{(a) = }") == ["(a)"], text(t"{(a) = }")
    assert text(t"{b}") == ["b"], text(t"{b}")

show(3, 7, {"k": 5})
print("ok")
"#;

/// An interpreter that has t-strings. `$PYTHON` is taken only when it is one, because
/// the suite's usual interpreter is older and would skip every assertion here silently.
fn python_314() -> Option<String> {
    let mut candidates = Vec::new();
    if let Ok(python) = std::env::var("PYTHON") {
        candidates.push(python);
    }
    candidates.extend(["python3.14", "python3"].map(String::from));
    candidates.into_iter().find(|python| {
        Command::new(python)
            .args([
                "-c",
                "import sys; sys.exit(0 if sys.version_info >= (3, 14) else 1)",
            ])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

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
fn a_field_reports_the_source_the_author_wrote() {
    let Some(python) = python_314() else {
        eprintln!("skipping t-string `expression` runtime test: needs python 3.14 or later");
        return;
    };

    run(&python, PYTHON_REFERENCE, "the plain-python reference");

    let config = Config {
        min_version: PythonVersion::PY314,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");
    run(&python, &transpiled, "the transpiled program");
}
