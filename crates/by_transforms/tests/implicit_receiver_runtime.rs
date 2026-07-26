//! Runtime divergence test for implicit-receiver lowering.
//!
//! The mdtest checker verifies the *types* a receiver callable produces and the
//! transform unit tests verify the *lowered text*. This test closes the loop: it
//! transpiles the three runtime-visible forms — a receiver callable passed a
//! plain function, `x.fn()` called through the receiver, an unapplied `x.fn`
//! bound as a `functools.partial`, and a trailing lambda block using the
//! receiver's members unqualified — and runs them on a real interpreter. A
//! misplaced receiver argument or a member bound to the wrong object would fail
//! here even though the type-level tests pass.
//!
//! No third-party packages are needed, so any `python3` will do; if none is
//! found the test skips rather than fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

/// basedpython whose module-level `assert`s exercise the implicit-receiver
/// runtime contract end to end.
const PROGRAM: &str = r##"
def render(value: int) -> str:
    return "#" + str(value)

# the receiver is the callable's leading positional parameter, so a plain
# function of that shape satisfies it and a direct call passes the receiver first
def direct(fn: int.() -> str) -> str:
    return fn(3)

assert direct(render) == "#3", "receiver is a leading positional parameter"

# calling through the receiver binds it as the first argument
def through_receiver(fn: int.() -> str) -> str:
    receiver = 4
    return receiver.fn()

assert through_receiver(render) == "#4", "`x.fn()` passes `x` as the receiver"

# an unapplied reference carries the receiver the way a bound method would
def unapplied(fn: int.(str) -> str) -> str:
    receiver = 5
    bound = receiver.fn
    return bound("!")

def suffixed(value: int, mark: str) -> str:
    return str(value) + mark

assert unapplied(suffixed) == "5!", "an unapplied reference binds the receiver"

# a trailing lambda block sees the receiver's members unqualified
def apply(fn: str.() -> None) -> None:
    fn("abc")

seen: str = ""
apply:
    seen = upper() + str(len(it))

assert seen == "ABC3", "block members resolve against the receiver"

print("ok")
"##;

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn implicit_receivers_run_correctly() {
    let Some(python) = python() else {
        eprintln!("skipping implicit-receiver runtime test: no `python3` interpreter found");
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
        "transpiled implicit-receiver program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
