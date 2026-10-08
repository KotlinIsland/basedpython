//! Runtime test for a `.by` function read back out of where it was stored.
//!
//! A `.by` function can carry what a call has to act on — a `context` parameter the call fills,
//! reified type parameters the call specializes by subscript. The checker keeps the function's
//! own type when a list, a dict, a tuple or an attribute stores it, and the transpiler reads that
//! type to write the call: the filled `context` argument is only in the output because the
//! checker still sees the function. This test runs the result to see both arrive.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod interpreters;

const STORED: &str = r#"
def greet(a: int, context b: str) -> str:
    return f"{b} {a}"

def name[reified T]() -> str:
    return T.__name__

class Holder:
    def __init__(self):
        self.greet = greet
        self.name = name

context salutation = "hello"

greeters = [greet]
greeter_by_key = {"a": greet}
greeter_pair = (greet, 1)
held = Holder()
appended_greeters = []
appended_greeters.append(greet)

assert greeters[0](1) == "hello 1", greeters[0](1)
assert greeter_by_key["a"](2) == "hello 2", greeter_by_key["a"](2)
assert greeter_pair[0](3) == "hello 3", greeter_pair[0](3)
assert held.greet(4) == "hello 4", held.greet(4)
assert appended_greeters[0](5) == "hello 5", appended_greeters[0](5)

names = [name]
name_by_key = {"a": name}
name_pair = (name, 1)
appended_names = []
appended_names.append(name)

assert names[0][int]() == "int", names[0][int]()
assert name_by_key["a"][str]() == "str", name_by_key["a"][str]()
assert name_pair[0][bytes]() == "bytes", name_pair[0][bytes]()
assert held.name[float]() == "float", held.name[float]()
assert appended_names[0][int]() == "int", appended_names[0][int]()

print("ok")
"#;

#[test]
fn a_stored_function_is_called_with_what_it_carries() {
    // a reified generic is a pep 695 function. `oldest` says why when it skips
    let Some(interpreter) = interpreters::oldest(PythonVersion::PY312, "", "") else {
        return;
    };
    let target = interpreter.version.min(PythonVersion::PY313);
    let config = Config {
        min_version: target,
        ..Config::default()
    };
    // `transpile` analyses the module with the checker, which is what fills the `context`
    // arguments
    let transpiled = transpile(STORED, &config).expect("transpile should succeed");

    let output = Command::new(&interpreter.command)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "the transpiled program failed on {interpreter}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    interpreters::ran(&interpreter, target);
}
