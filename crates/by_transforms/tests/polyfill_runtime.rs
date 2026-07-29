//! Runtime divergence tests for the **pep 695 polyfill** target.
//!
//! Every other runtime test transpiles at `PY313`, where type parameters are
//! native and the polyfill never runs. That left the default output target
//! (`--min-version 3.10`) — the one a user gets without passing a flag —
//! untested at runtime, and three separate lowerings reached it emitting python
//! that raises on import while `by check` reported nothing:
//!
//! - a callable arrow (`(T) -> R`) rendered `Callable[[T], R]`, keeping the
//!   pre-polyfill parameter names while the polyfill bound `_T` / `_R`
//! - a `type` alias over a variadic emitted `type_params=(_T, Unpack[_Ts])`,
//!   which `TypeAliasType` rejects
//! - a keyword subscript or a `*Ts` inside an alias value or a type-parameter
//!   bound reached the output in its `.by` spelling
//!
//! Asserting on the lowered *text* is what let all three through, so these
//! tests execute it instead.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

/// exercises the polyfill's typevar rename across every position that renders
/// replacement text rather than patching source bytes
const RENAMES: &str = r#"
def apply[T, R](fn: (T) -> R, t: T) -> R:
    return fn(t)

class Holder[T, R]:
    def apply(self, fn: (T) -> R, t: T) -> R:
        return fn(t)

class Deco[**P, R]:
    def __init__(self, fn: (**P) -> R) -> None:
        self.fn = fn

    def call(self, *args: P.args, **kwargs: P.kwargs) -> R:
        return self.fn(*args, **kwargs)

assert apply(str, 1) == "1", "arrow parameter and return rename together"
assert Holder[int, str]().apply(str, 2) == "2", "a method's arrow renames too"
assert Deco(str).call(3) == "3", "a parameters-spec arrow renames too"

# the annotation has to survive introspection, not merely import: at 3.9 the
# module is emitted with `from __future__ import annotations`, so a stale name
# raises here rather than at import
import typing
assert set(typing.get_type_hints(apply)) == {"fn", "t", "return"}, "annotations resolve"

print("ok")
"#;

/// the alias / bound positions, where the polyfill re-renders a whole statement
/// and has to splice in what the other passes rewrote inside it
const ALIASES: &str = r#"
class Pair[A, B]:
    def __init__(self) -> None:
        self.tag = "pair"

type Named = Pair[int, B=str]
type Starred[T, *Ts] = tuple[T, *Ts]

def bounded[T: Pair[int, B=str]](t: T) -> str:
    return t.tag

class Bounded[T: Pair[int, B=str]]:
    def tag(self, t: T) -> str:
        return t.tag

assert bounded(Pair()) == "pair", "a keyword subscript in a bound lowers"
assert Bounded[Pair]().tag(Pair()) == "pair", "and in a class's bound"
assert Starred.__type_params__ != (), "the variadic reached `type_params`"

print("ok")
"#;

/// The polyfilled output imports `typing_extensions` for `TypeAliasType`, so an
/// interpreter without it can only run the rename half.
fn python_with_typing_extensions(python: &str) -> bool {
    Command::new(python)
        .args(["-c", "import typing_extensions"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_polyfilled(python: &str, program: &str) {
    let config = Config {
        min_version: PythonVersion::PY310,
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
        "polyfilled program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn polyfilled_type_parameters_rename_everywhere() {
    let Some(python) = python() else {
        eprintln!("skipping polyfill runtime test: no `python3` interpreter found");
        return;
    };
    run_polyfilled(&python, RENAMES);
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn polyfilled_aliases_and_bounds_run() {
    let Some(python) = python() else {
        eprintln!("skipping polyfill runtime test: no `python3` interpreter found");
        return;
    };
    if !python_with_typing_extensions(&python) {
        eprintln!("skipping polyfill alias runtime test: {python} has no `typing_extensions`");
        return;
    }
    run_polyfilled(&python, ALIASES);
}
