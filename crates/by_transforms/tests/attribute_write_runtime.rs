//! Runtime test for a write to a declared attribute in a `.by` file.
//!
//! The value is read as what it is checked as: the declaration is its expected type, as it is
//! for an annotated assignment. A construction of a reified generic class is told the
//! specialization it builds from that, where without it the class could not say which one.

mod common;

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

const PROGRAM: &str = r#"
class Box[reified T]:
    def kind(self) -> str:
        return T.__name__


class Holder:
    box: Box[int]

    def __init__(self):
        self.box = Box()


holder = Holder()
assert holder.box.kind() == "int", "construction in a method"
holder.box = Box()
assert holder.box.kind() == "int", "construction through an instance"
print("ok")
"#;

#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
#[test]
fn a_reified_construction_takes_the_declared_specialization() {
    let Some(python) = common::python() else {
        eprintln!("skipping attribute write runtime test: no interpreter found");
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
        "transpiled program failed on {python}:\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
