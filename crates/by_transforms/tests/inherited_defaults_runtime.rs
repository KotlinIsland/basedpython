//! Runtime test for the defaults an override inherits from the method it overrides.
//!
//! The lowering writes the base's default into the override's own signature, so what the
//! checker allows — a call that leaves the argument out — is a call python accepts and binds
//! to the same value. Asserting on the lowered *text* cannot see that: a default written in
//! the wrong place still parses, and a call binding the wrong value still runs.
//!
//! No third-party packages are needed, so any `python3` will do; if none is found the test
//! skips rather than fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

/// each shape a default can be declared in, taken by an override that declares none
const INHERITED: &str = r#"
class A:
    def positional(self, a = 1): ...
    def annotated(self, a: str = "x"): ...
    def keyword_only(self, *, k = None): ...
    def positional_only(self, a = 2, /): ...

class B(A):
    override def positional(self, a):
        return a

    override def annotated(self, a: str):
        return a

    override def keyword_only(self, *, k):
        return k

    override def positional_only(self, a, /):
        return a

b = B()
assert b.positional() == 1, "a positional default reaches the override"
assert b.positional(9) == 9, "and an argument still wins"
assert b.annotated() == "x", "an annotated parameter's default reaches it too"
assert b.keyword_only() is None, "a keyword-only default reaches it by name"
assert b.positional_only() == 2, "so does a positional-only one, by position"

print("ok")
"#;

/// the override declares what it inherited, so the next override down inherits it in turn
const CHAIN: &str = r#"
class A:
    def f(self, a = 1): ...

class B(A):
    override def f(self, a):
        return a

class C(B):
    override def f(self, a):
        return ("C", a)

assert B().f() == 1, "the first override takes the default"
assert C().f() == ("C", 1), "and passes it on to the next"

print("ok")
"#;

/// an inherited default makes the parameters after it "after a default", exactly as a written
/// one would — the relaxed-order lowering has to see it, or the emitted signature is one
/// python refuses to parse
const RELAXED_ORDER: &str = r#"
class A:
    def f(self, a = 1, b = 2): ...

class B(A):
    override def f(self, a, b):
        return (a, b)

assert B().f() == (1, 2), "both defaults reach the override"
assert B().f(3) == (3, 2), "and an argument fills them left to right"

print("ok")
"#;

/// the `init(…)` shorthand writes its own body, so it writes its own signature edits too
const INIT_SHORTHAND: &str = r#"
class A:
    init(var a: int = 1)

class B(A):
    init(var a: int)

assert B().a == 1, "an `init` shorthand takes the default its base declares"
assert B(7).a == 7, "and an argument still wins"

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
fn an_override_is_callable_without_the_argument_the_base_defaulted() {
    let Some(python) = python() else {
        return;
    };
    run(&python, INHERITED);
}

#[test]
fn a_default_reaches_every_override_down_the_chain() {
    let Some(python) = python() else {
        return;
    };
    run(&python, CHAIN);
}

#[test]
fn a_parameter_after_an_inherited_default_still_binds() {
    let Some(python) = python() else {
        return;
    };
    run(&python, RELAXED_ORDER);
}

#[test]
fn an_init_shorthand_takes_the_default_its_base_declares() {
    let Some(python) = python() else {
        return;
    };
    run(&python, INIT_SHORTHAND);
}
