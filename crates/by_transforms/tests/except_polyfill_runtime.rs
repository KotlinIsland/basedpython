//! Runtime divergence test for the bracketless `except` of PEP 758.
//!
//! python 3.14 reads `except A, B:` as a handler for either type, and an older python
//! cannot parse it. The transpiler parenthesizes the types for a target before 3.14, and
//! this runs the result to show the handler still catches each type it names, and only
//! those.
//!
//! No third-party packages are needed, so the program is transpiled for whichever
//! interpreter runs it; if none is found the test skips, and says so, rather than
//! fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
mod interpreters;
use common::python;

const PROGRAM: &str = r#"
def caught(error: Exception) -> str:
    try:
        raise error
    except KeyError, IndexError:
        return "lookup"
    except ValueError, TypeError:
        return "value"
    except Exception:
        return "other"

assert caught(KeyError()) == "lookup"
assert caught(IndexError()) == "lookup"
assert caught(TypeError()) == "value"
assert caught(RuntimeError()) == "other"
print("ok")
"#;

/// an exception group's members are matched by an `except*` written the same way
const EXCEPT_STAR: &str = r#"
seen: list[str] = []
try:
    raise ExceptionGroup("both", [KeyError(), ValueError()])
except* KeyError, IndexError:
    seen.append("lookup")
except* ValueError, TypeError:
    seen.append("value")

assert seen == ["lookup", "value"]
print("ok")
"#;

fn run(program: &str, at_least: PythonVersion) {
    let Some(python) = python(at_least) else {
        return;
    };
    let config = Config {
        min_version: python.version,
        ..Config::default()
    };
    let transpiled = transpile(program, &config).expect("transpile should succeed");
    let output = Command::new(&python.command)
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
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

#[test]
fn a_bracketless_except_catches_each_type_it_names() {
    run(PROGRAM, PythonVersion::PY39);
}

#[test]
fn a_bracketless_except_star_catches_each_type_it_names() {
    run(EXCEPT_STAR, PythonVersion::PY311);
}
