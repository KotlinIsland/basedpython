//! Runtime test for a t-string field's `expression`, whose text is the author's own.
//!
//! Python 3.14 hands every replacement field of a `t"..."` to the program as a
//! `string.templatelib.Interpolation` whose `expression` is the *source text* of
//! that field, so the text a lowered field reports is a claim about the *source*,
//! not about the lowering. Only an interpreter can settle it: the transform's unit
//! tests can say what was emitted, but not what python makes of it — and the exact
//! span python reports (its leading whitespace, its grouping parentheses, its
//! comments) is python's own rule, which is easy to state wrongly from the outside.
//!
//! The rule changed within 3.14: from 3.14.8 a field's text runs up to the `=`,
//! conversion, format spec or `}` that ends it, trailing whitespace and all, where
//! 3.14.0 to 3.14.7 drop that whitespace (python/cpython#154719). A lowered field
//! reports what the interpreter it runs on would have reported for it, so the
//! expectations here are written for both, chosen by the version that runs them.
//!
//! Every assertion is written twice over: the basedpython program asserts what the
//! field reports, and [`PYTHON_REFERENCE`] asserts that the same expectation holds
//! for the plain-python spelling of the same field. So a wrong expectation — the
//! version split included — fails rather than being blessed.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
mod interpreters;
use interpreters::Interpreter;

/// the field text an interpreter reports, chosen by its version: `ends(kept, dropped)` is
/// `kept` where a field's trailing whitespace is kept and `dropped` where it is not. an
/// interpreter older than 3.14 has no t-strings, and the polyfill a string tag lowers to there
/// keeps it, as python does now
const ENDS: &str = r#"
import sys

def ends(kept: str, dropped: str) -> str:
    if (3, 14) <= sys.version_info < (3, 14, 8):
        return dropped
    return kept
"#;

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
    assert text(t"{  (a!)  }") == [ends("  (a!)  ", "  (a!)")], text(t"{  (a!)  }")
    # a conversion and a format spec end the reported text where the brace would
    assert text(t"{ (a!) !r:>6}") == [ends(" (a!) ", " (a!)")], text(t"{ (a!) !r:>6}")
    # a field nothing lowered keeps the interpolation python built for it
    assert text(t"{b} {a!} { b }") == ["b", "a!", ends(" b ", " b")], text(t"{b} {a!} { b }")
    # a comment is not part of the text, though the line it ends is
    assert text(t"""{a! # why
}""") == [ends("a! \n", "a!")], text(t"""{a! # why
}""")
    assert text(t"""{(a! # why
    ?? b)}""") == ["(a! \n    ?? b)"], "a comment inside the parentheses"
    # the values and the rendering are the ones the lowering produced
    assert t"{a!}".values == (3,), t"{a!}".values
    assert t"{ (a!) !r:>6}".values == (3,), "spec values"
    assert t"{a!}".strings == ("", ""), t"{a!}".strings
    # a `=` field, which python itself splits into literal text and a field beside it
    assert list(t"{(a!) = }")[0] == "(a!) = ", list(t"{(a!) = }")[0]
    assert text(t"{(a!) = }") == [ends("(a!) ", "(a!)")], text(t"{(a!) = }")
    # a template nothing lowered is left exactly as written
    assert text(t"{b}") == ["b"], text(t"{b}")
    # a string tag is handed the template with the same texts
    assert text(same"{a!}") == ["a!"], text(same"{a!}")
    assert text(same"{b} {a ?? b }") == ["b", ends("a ?? b ", "a ?? b")], text(same"{b} {a ?? b }")
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
    assert text(t"{  (a)  }") == [ends("  (a)  ", "  (a)")], text(t"{  (a)  }")
    assert text(t"{ (a) !r:>6}") == [ends(" (a) ", " (a)")], text(t"{ (a) !r:>6}")
    assert text(t"{b} {a} { b }") == ["b", "a", ends(" b ", " b")], text(t"{b} {a} { b }")
    assert text(t"""{a # why
}""") == [ends("a \n", "a")], text(t"""{a # why
}""")
    assert text(t"""{(a # why
    or b)}""") == ["(a \n    or b)"], "a comment inside the parentheses"
    assert t"{a}".values == (3,), t"{a}".values
    assert t"{a}".strings == ("", ""), t"{a}".strings
    assert list(t"{(a) = }")[0] == "(a) = ", list(t"{(a) = }")[0]
    assert text(t"{(a) = }") == [ends("(a) ", "(a)")], text(t"{(a) = }")
    assert text(t"{b}") == ["b"], text(t"{b}")
    assert text(t"{b} {a or b }") == ["b", ends("a or b ", "a or b")], text(t"{b} {a or b }")

show(3, 7, {"k": 5})
print("ok")
"#;

/// string tags, which are t-strings from 3.14 and a polyfilled template below it, so they run on
/// every version
const TAGS: &str = r#"
def text(t: object) -> list[str]:
    return [i.expression for i in t.interpolations]

def same(t: object) -> object:
    return t

def show(a: int?, b: int) -> None:
    assert text(same"{a!}") == ["a!"], text(same"{a!}")
    assert text(same"{  (a!)  }") == [ends("  (a!)  ", "  (a!)")], text(same"{  (a!)  }")
    assert text(same"{ (a!) !r:>6}") == [ends(" (a!) ", " (a!)")], text(same"{ (a!) !r:>6}")
    assert text(same"{b} {a ?? b} { b }") == ["b", "a ?? b", ends(" b ", " b")], (
        text(same"{b} {a ?? b} { b }")
    )
    assert same"{a!}".values == (3,), same"{a!}".values

show(3, 7)
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

/// `program` transpiled for `target`, with [`ENDS`] ahead of it
fn transpiled(program: &str, target: PythonVersion) -> String {
    let config = Config {
        min_version: target,
        ..Config::default()
    };
    transpile(&format!("{ENDS}{program}"), &config).expect("transpile should succeed")
}

#[test]
fn a_field_reports_the_source_the_author_wrote() {
    // an interpreter that has t-strings
    let Some(python) = common::python(PythonVersion::PY314) else {
        return;
    };

    run(
        &python,
        &format!("{ENDS}{PYTHON_REFERENCE}"),
        "the plain-python reference",
    );
    run(
        &python,
        &transpiled(PROGRAM, python.version),
        "the transpiled program",
    );
}

/// a string tag below 3.14 builds its template from the polyfill, whose fields report the same
/// text — and on an interpreter that has t-strings, the text that interpreter would report
#[test]
fn a_string_tag_field_reports_the_source_the_author_wrote() {
    let Some(python) = common::python(PythonVersion::PY39) else {
        return;
    };
    for target in [python.version, PythonVersion::PY313] {
        if target <= python.version {
            run(
                &python,
                &transpiled(TAGS, target),
                &format!("the tags transpiled for {target}"),
            );
        }
    }
}
