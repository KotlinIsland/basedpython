//! The interpreters the runtime tests run transpiled python on, and how a test says which one
//! ran it.
//!
//! A test that needs something of its interpreter — a version, a module — skips when none here
//! has it, and a skip passes. Nothing on a passing test's line tells the two apart, so every test
//! built on this says which interpreter it ran on, or why it ran nothing, and the nextest
//! configuration shows what these binaries print when they pass.

use std::process::{Command, Stdio};

use by_transforms::PythonVersion;

/// an interpreter found under one of the names the tests look for
pub(crate) struct Interpreter {
    pub(crate) command: String,
    pub(crate) version: PythonVersion,
    pub(crate) typing_extensions: bool,
}

impl std::fmt::Display for Interpreter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.command, self.version)?;
        if self.typing_extensions {
            write!(f, " with `typing_extensions`")?;
        }
        Ok(())
    }
}

impl Interpreter {
    /// whether `probe`, python source, runs to completion here
    fn runs(&self, probe: &str) -> bool {
        Command::new(&self.command)
            .args(["-c", probe])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

/// what an interpreter says about itself: its version, whether it has `typing_extensions`, and
/// the file it runs from, which tells two names for one interpreter apart
const PROBE: &str = "import sys, importlib.util; \
print(sys.version_info[0], sys.version_info[1], \
int(importlib.util.find_spec('typing_extensions') is not None), sys.executable)";

/// every interpreter this machine has under `$PYTHON`, `python3.N` or `python3`, oldest first,
/// each listed once however many of the names reach it
///
/// `python3` alone is whichever one the path finds first, which need not be one a test can use,
/// so every name is asked
pub(crate) fn interpreters() -> Vec<Interpreter> {
    let mut candidates: Vec<String> = std::env::var("PYTHON").into_iter().collect();
    candidates.extend((8..=15).map(|minor| format!("python3.{minor}")));
    candidates.push("python3".to_owned());

    let mut seen = std::collections::HashSet::new();
    let mut found: Vec<Interpreter> = candidates
        .into_iter()
        .filter_map(|command| {
            let output = Command::new(&command)
                .args(["-c", PROBE])
                .stderr(Stdio::null())
                .output()
                .ok()
                .filter(|output| output.status.success())?;
            let stdout = String::from_utf8(output.stdout).ok()?;
            let mut fields = stdout.trim_end().splitn(4, ' ');
            let major = fields.next()?.parse().ok()?;
            let minor = fields.next()?.parse().ok()?;
            let typing_extensions = fields.next()? == "1";
            let executable = fields.next()?;
            let executable = std::fs::canonicalize(executable)
                .unwrap_or_else(|_| std::path::PathBuf::from(executable));
            seen.insert(executable).then(|| Interpreter {
                command,
                version: PythonVersion::from((major, minor)),
                typing_extensions,
            })
        })
        .collect();
    found.sort_by_key(|interpreter| interpreter.version);
    found
}

/// the oldest interpreter of `at_least` or later on which `probe`, python source, runs. none is a
/// skip, said so, with `needs` saying what else the test needed of it when it needed more than a
/// version
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
pub(crate) fn oldest(at_least: PythonVersion, probe: &str, needs: &str) -> Option<Interpreter> {
    let found = interpreters()
        .into_iter()
        .find(|interpreter| interpreter.version >= at_least && interpreter.runs(probe));
    if found.is_none() {
        let needs = if needs.is_empty() {
            String::new()
        } else {
            format!(" {needs}")
        };
        eprintln!("skipping: no interpreter of python {at_least} or later{needs} found");
    }
    found
}

/// say that `interpreter` ran the test, on python transpiled for `target`
#[expect(
    clippy::print_stderr,
    reason = "a test that can skip says what it ran on, so a pass is told apart from a skip"
)]
pub(crate) fn ran(interpreter: &Interpreter, target: PythonVersion) {
    eprintln!("ran, transpiled for {target}, on {interpreter}");
}
