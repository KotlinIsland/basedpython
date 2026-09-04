//! Runtime test for `build:` stamps.
//!
//! The block lowers to a class whose members hold literals the build supplied,
//! so what matters is that the emitted python *is* python: a stamp value is
//! text a build system handed over, and text that reached the output unescaped
//! is a `SyntaxError` at import rather than anything `by check` or an mdtest
//! would notice. The awkward values are the point of this test.

mod common;

use std::collections::BTreeMap;
use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

fn config(stamps: &[(&str, &str)]) -> Config {
    Config {
        min_version: PythonVersion::PY313,
        stamps: stamps
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>(),
        ..Config::default()
    }
}

#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn run(program: &str, stamps: &[(&str, &str)]) {
    let Some(python) = common::python() else {
        eprintln!("skipping build stamp runtime test: no interpreter found");
        return;
    };

    let transpiled = transpile(program, &config(stamps)).expect("transpile should succeed");
    let output = Command::new(&python)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "stamped program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

#[test]
fn a_stamp_reaches_the_program_as_its_declared_type() {
    run(
        r#"
build:
    GIT_SHA: str
    GIT_DIRTY: bool
    BUILD_NUMBER: int

assert build.GIT_SHA == "e6f9ac1d", build.GIT_SHA
assert build.GIT_DIRTY is True, build.GIT_DIRTY
assert build.BUILD_NUMBER == 417, build.BUILD_NUMBER
print("ok")
"#,
        &[
            ("GIT_SHA", "e6f9ac1d"),
            ("GIT_DIRTY", "true"),
            ("BUILD_NUMBER", "417"),
        ],
    );
}

#[test]
fn a_value_holding_quotes_and_newlines_survives() {
    // `git log -1 --format=%s` hands over whatever the commit message says, and
    // a subject line with a quote in it is not unusual
    run(
        r#"
build:
    SUBJECT: str

assert build.SUBJECT == "fix the \"one\" case\nand a second line", repr(build.SUBJECT)
print("ok")
"#,
        &[("SUBJECT", "fix the \"one\" case\nand a second line")],
    );
}

#[test]
fn a_value_that_looks_like_code_is_not_code() {
    // whatever a build hands over is data. this one would be an import and a
    // call if it were ever spliced in unescaped
    run(
        r#"
build:
    DESCRIBE: str

assert build.DESCRIBE == "\" + __import__('os').getcwd() + \"", repr(build.DESCRIBE)
print("ok")
"#,
        &[("DESCRIBE", "\" + __import__('os').getcwd() + \"")],
    );
}

#[test]
fn an_unsupplied_stamp_runs_on_its_default() {
    run(
        r#"
build:
    GIT_SHA: str = "unreleased"
    DIRTY: bool = False

assert build.GIT_SHA == "unreleased", build.GIT_SHA
assert build.DIRTY is False, build.DIRTY
print("ok")
"#,
        &[],
    );
}
