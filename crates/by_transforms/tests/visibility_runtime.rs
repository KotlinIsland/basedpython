//! Runtime test for the `__all__` the visibility modifiers synthesize.
//!
//! `__all__` is the one visibility artefact with teeth at runtime: `from m
//! import *` reads it and looks every entry up, so a name in it that the module
//! does not have raises `AttributeError` at import. That makes it the place
//! where a disagreement between the list and the emitted names is fatal rather
//! than cosmetic — `private` renames its symbol, so a symbol marked both
//! `export` and `private` listed a name nothing answers to.
//!
//! The transform unit tests pin the text of the emitted `__all__`. This test
//! closes the loop by importing the module the way `__all__` is for.
//!
//! No third-party packages are needed, so any `python3` will do; if none is
//! found the test skips rather than fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

/// Every visibility shape that reaches `__all__`, in both keyword orders.
const PROGRAM: &str = r#"
export private def helper() -> int:
    return 1

private export class Hidden:
    pass

export def api() -> int:
    return helper()

public class Shown:
    pass

def unmarked() -> int:
    return 2
"#;

/// Import the transpiled module the way `import *` does, and report what the
/// module actually offers so a mismatch names both sides.
const IMPORTER: &str = r#"
import importlib, sys
m = importlib.import_module("emitted")
missing = [n for n in m.__all__ if not hasattr(m, n)]
assert not missing, f"__all__ names {missing}, which the module does not have"
from emitted import *
assert sorted(m.__all__) == ["Shown", "api"], f"unexpected __all__: {m.__all__}"
assert not hasattr(m, "helper") and hasattr(m, "_helper"), "`private` still renames"
assert not hasattr(m, "Hidden") and hasattr(m, "_Hidden"), "in either keyword order"
assert hasattr(m, "unmarked"), "an unmarked symbol is emitted, just not exported"
print("ok")
"#;

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn every_exported_name_exists_at_runtime() {
    let Some(python) = python() else {
        eprintln!("skipping visibility runtime test: no `python3` interpreter found");
        return;
    };

    let config = Config {
        min_version: PythonVersion::PY313,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");

    // `import *` needs a real module on the path, not a `-c` script
    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    std::fs::write(dir.path().join("emitted.py"), &transpiled).expect("failed to write the module");

    let output = Command::new(&python)
        .arg("-c")
        .arg(IMPORTER)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "importing the transpiled module failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
