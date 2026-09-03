//! Runtime test for the return-value markers.
//!
//! The markers are pure declarations, so the whole claim about the emitted
//! python is a negative one: nothing of them is left. Nothing else can show it —
//! a marker that survived is valid python, so the reparse the transpile ends
//! with accepts it, and the failure only turns up when the module is imported
//! and `ignorable_return_value` is an undefined name.
//!
//! The declarations below are shaped to reach both halves of the erasure. A
//! marked function whose body nothing rewrites keeps its source bytes, and the
//! deletion edit is what removes the decorator; a marked function whose body
//! holds something a rewriting pass replaces has its whole statement rebuilt
//! from the syntax tree, which ignores that edit and would put the decorator
//! back if it were not stripped from the tree as well.
//!
//! The program deliberately never spells either marker in a string: the test
//! asserts the transpiled text does not mention them, and a literal would
//! satisfy that search without a decorator surviving at all.

use std::process::{Command, Stdio};

use by_transforms::{Config, PythonVersion, transpile};

/// basedpython whose module-level `assert`s run the marked declarations
const PROGRAM: &str = r#"
@ignorable_return_value
def plain() -> int:
    return 1

# `??` is rewritten by a pass that rebuilds the whole statement from the syntax
# tree, so this marker only disappears if it was taken out of the tree too
@ignorable_return_value
def rewritten(value: int?) -> int:
    return value ?? 2

@ignorable_return_value
class Query:
    def where(self, clause: str) -> Query:
        return self

    @must_use_return_value
    def rows(self) -> list[str]:
        return [self.clause]

    def __init__(self) -> None:
        self.clause = ""

assert plain() == 1
assert rewritten(None) == 2
assert rewritten(5) == 5
assert Query().where("id = 1").rows() == [""]

print("ok")
"#;

/// the first interpreter that accepts `probe`
fn python_supporting(probe: &str) -> Option<String> {
    let mut candidates = Vec::new();
    if let Ok(p) = std::env::var("PYTHON") {
        candidates.push(p);
    }
    candidates.extend(["python3.13", "python3"].map(String::from));

    candidates.into_iter().find(|py| {
        Command::new(py)
            .args(["-c", probe])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn marked_declarations_run_with_no_marker_left() {
    let Some(python) = python_supporting("import sys; sys.exit(0)") else {
        eprintln!("skipping return-value-marker runtime test: no python interpreter found");
        return;
    };
    let config = Config {
        min_version: PythonVersion::PY313,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");

    assert!(
        !transpiled.contains("ignorable_return_value"),
        "the marker survived the transpile:\n{transpiled}"
    );
    assert!(
        !transpiled.contains("must_use_return_value"),
        "the marker survived the transpile:\n{transpiled}"
    );

    let output = Command::new(&python)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
