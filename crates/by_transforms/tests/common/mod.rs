//! Shared helpers for the runtime divergence tests.
//!
//! These tests transpile basedpython and run the output on a real interpreter,
//! so they all need to locate one. Variants that need more than a bare
//! interpreter — a third-party package, a specific feature probe — keep their
//! own locator next to the test that needs it.

use std::process::Command;

/// Locate a usable interpreter: `$PYTHON` first, then common names, newest
/// first. Returns `None` (the caller skips) when none is found.
pub(crate) fn python() -> Option<String> {
    let mut candidates = Vec::new();
    if let Ok(p) = std::env::var("PYTHON") {
        candidates.push(p);
    }
    candidates.extend(["python3.13", "python3", "python"].map(String::from));

    candidates.into_iter().find(|py| {
        Command::new(py)
            .args(["-c", ""])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}
