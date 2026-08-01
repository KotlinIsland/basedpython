//! Runtime test for anonymous-named-tuple field types.
//!
//! The synthesized `NamedTuple` class is hoisted out of the statement its field
//! types were written in, so no sibling transform's edit reaches them — a field
//! type has to be lowered where the class body is built. When it is not, a
//! `dynamic` field reaches the output verbatim and the class raises `NameError`
//! the moment the module is imported, with `by check` clean and `by transpile`
//! exiting 0. That is exactly what an mdtest cannot see, so it is checked here.

mod common;

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

/// every field type here is basedpython surface syntax that has no python
/// spelling, so an unlowered one is either a `NameError` at class construction
/// or a syntax error. each also lowers to something the preamble binds above
/// the hoisted class — a field type naming a *later* module binding is a
/// separate, pre-existing gap
const PROGRAM: &str = r#"
def use(p: (m: dynamic, n: int?, o: (int) -> str)) -> None:
    assert p.m == 1, "m"
    assert p.n is None, "n"
    assert p.o(1) == "1", "o"

v: (m: dynamic, n: int?, o: (int) -> str) = (1, None, str)
use(v)
print("ok")
"#;

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn anon_named_tuple_field_types_run() {
    let Some(python) = common::python() else {
        eprintln!("skipping anon named tuple runtime test: no interpreter found");
        return;
    };

    let config = Config {
        min_version: PythonVersion::PY313,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");

    let output = Command::new(&python)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled anon named tuple program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
