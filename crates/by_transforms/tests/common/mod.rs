//! Shared helpers for the runtime divergence tests.
//!
//! These tests transpile basedpython and run the output on a real interpreter,
//! so they all need to locate one. Variants that need more than a version of
//! their interpreter — a third-party package, a specific feature probe — find it
//! through [`crate::interpreters`] next to the test that needs it.

use std::io::Write;
use std::process::{Command, Output};

use by_transforms::PythonVersion;

use crate::interpreters::{self, Interpreter, oldest, probe};

/// the interpreter a test runs on, which is also the version it transpiles for. see
/// [`python_with`]
#[allow(
    dead_code,
    reason = "each test binary compiles this module, and one whose tests all need more of their \
              interpreter than a version calls only `python_with`"
)]
pub(crate) fn python(at_least: PythonVersion) -> Option<Interpreter> {
    python_with(at_least, "", "")
}

/// the interpreter a test runs on, which is also the version it transpiles for, said on stderr:
/// `$PYTHON` when it is `at_least` or later and `probe`, python source, runs on it, which is the
/// interpreter the run was pinned to, then `python3` from the path, as the native backend's tests
/// take it, and otherwise the oldest that is. none is a skip, said so, with `needs` saying what
/// else the test needed of it when it needed more than a version
///
/// a test transpiles for the interpreter it runs on rather than for a fixed version, so a run
/// pinned to an older python executes the lowerings that python gets. `at_least` is the oldest
/// version the test's program can be written for at all
pub(crate) fn python_with(
    at_least: PythonVersion,
    probe_source: &str,
    needs: &str,
) -> Option<Interpreter> {
    let ambient = std::env::var("PYTHON").unwrap_or_else(|_| "python3".to_owned());
    let interpreter = match probe(ambient) {
        Some((interpreter, _))
            if interpreter.version >= at_least && interpreter.runs(probe_source) =>
        {
            interpreter
        }
        _ => oldest(at_least, probe_source, needs)?,
    };
    interpreters::ran(&interpreter, interpreter.version);
    Some(interpreter)
}

/// `command`, an interpreter, run on `source` as `python -c` would run it. windows limits a
/// command line to 32 KiB, which a program holding the runtime helpers can pass, so the source
/// is read from a file
#[allow(
    dead_code,
    reason = "each test binary compiles this module, and most pass their program on the \
              command line"
)]
pub(crate) fn run_source(command: &mut Command, source: &str) -> Output {
    let mut file = tempfile::NamedTempFile::new().expect("failed to create the source file");
    file.write_all(source.as_bytes())
        .expect("failed to write the source file");
    command
        .args([
            "-c",
            "import sys; exec(compile(open(sys.argv[1], encoding='utf-8').read(), '<string>', 'exec'))",
        ])
        .arg(file.path())
        .output()
        .expect("failed to spawn python")
}
