//! Runtime test for keyword argument names python cannot spell.
//!
//! The transform's unit tests assert on the lowered *text*. What actually
//! matters is what the call does: the key the callee receives has to be the
//! name as written, the arguments have to arrive in the order the source put
//! them in, and the values have to be evaluated in the order python would have
//! evaluated them. A lowering that gets any of those wrong still produces
//! python that parses and runs.
//!
//! No third-party packages are needed, so any `python3` will do; if none is
//! found the test skips rather than fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

/// the keys a call actually delivers, for every shape of name
const KEYS: &str = r#"
def collect(**kwargs):
    return kwargs

assert collect(foo.bar=1) == {"foo.bar": 1}, "a dotted path is the key"
assert collect("content-type"=1) == {"content-type": 1}, "a string is the key"
assert collect("timeout"=1) == {"timeout": 1}, "so is one python could spell"
assert collect("a\tb"=1) == {"a\tb": 1}, "escapes are decoded"
assert collect(r"a\nb"=1) == {"a\\nb": 1}, "and a raw string's are not"
assert collect(""=1) == {"": 1}, "an empty name is a key like any other"
assert collect("a'b\"c"=1) == {"a'b\"c": 1}, "quotes in a key are escaped again"
assert collect("class"=1) == {"class": 1}, "a python keyword is only a key here"

# a name a parameter already has binds that parameter
def named(timeout, **kwargs):
    return timeout, kwargs

assert named("timeout"=1) == (1, {}), "a quoted name binds the parameter it names"

print("ok")
"#;

/// arguments keep the order the source wrote them in, and their values are
/// evaluated in the order python would have evaluated them
const ORDER: &str = r#"
log = []

def note(x):
    log.append(x)
    return x

def collect(*args, **kwargs):
    return list(args), list(kwargs)

# a mapping is spliced where the name was written, so the keys stay in order
args, keys = collect(note(1), a=note(2), b.c=note(3), d=note(4))
assert args == [1] and keys == ["a", "b.c", "d"], f"out of order: {keys}"
assert log == [1, 2, 3, 4], f"out of order: {log}"

# python evaluates a starred argument before every keyword argument, so moving
# it ahead of the mapping changes nothing
log.clear()
args, keys = collect(a=note(1), b.c=note(2), *note(["rest"]))
assert args == ["rest"] and keys == ["a", "b.c"], f"lost an argument: {args} {keys}"
assert log == [["rest"], 1, 2], f"out of order: {log}"

# and the same call written without a flexible name evaluates it the same way
log.clear()
collect(a=note(1), b=note(2), *note(["rest"]))
assert log == [["rest"], 1, 2], f"the plain call disagrees: {log}"

print("ok")
"#;

/// a lowering written inside a flexible argument's value has to survive being
/// re-emitted as a mapping entry
const COMPOSITION: &str = r#"
class Holder:
    def __init__(self) -> None:
        self.inner: int = 5

def collect(**kwargs):
    return kwargs

assert collect(a.b=Holder()?.inner) == {"a.b": 5}, "an optional chain is kept"
assert collect(a.b=None?.inner) == {"a.b": None}, "an absent one too"
assert collect(a.b=Holder().inner!) == {"a.b": 5}, "a force unwrap is kept whole"
assert collect(a.b=1 is int) == {"a.b": True}, "an `is` test keeps its lowering"
assert collect(a.b=(7, 8).0, *[]) == {"a.b": 7}, "and so does a reordered one"

print("ok")
"#;

fn run(python: &str, program: &str) {
    let config = Config {
        min_version: PythonVersion::PY313,
        ..Config::default()
    };
    let transpiled = transpile(program, &config).expect("transpile should succeed");
    let output = Command::new(python)
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
fn a_flexible_name_arrives_as_the_key_it_was_written_as() {
    let Some(python) = python() else {
        return;
    };
    run(&python, KEYS);
}

#[test]
fn a_lowered_call_keeps_its_argument_and_evaluation_order() {
    let Some(python) = python() else {
        return;
    };
    run(&python, ORDER);
}

#[test]
fn a_lowering_inside_a_flexible_arguments_value_still_applies() {
    let Some(python) = python() else {
        return;
    };
    run(&python, COMPOSITION);
}
