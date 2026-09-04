//! Runtime divergence tests for how a trailing-lambda block scopes its names.
//!
//! The mdtests fix what the *checker* resolves inside a block — a `let` that is
//! the block's own, an `it` that always belongs to the innermost block — and the
//! transform unit tests fix the lowered text. This test closes the loop on a
//! real interpreter, on the two points where the lowered text either holds or
//! fails in a way neither of those would see: a block declares `it=None` (and a
//! receiver ahead of it) whatever its callback passes, so a callback that passes
//! fewer arguments must still be able to call it; and a `let` inside a block
//! whose name the enclosing function also binds must not lower to `nonlocal x` +
//! `x: Final = …`, which python rejects at compile time.
//!
//! No third-party packages are needed, so any `python3` will do; if none is
//! found the test skips rather than fails.

#![expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

/// a block declares `it=None` whatever its callback passes, so a callback that
/// passes fewer arguments than the block declares can still call it — the
/// property the unconditional declaration rests on. a receiver callback passes
/// its receiver and no `it`, which must bind the receiver parameter rather than
/// `it`
const UNFILLED_PARAMETERS_PROGRAM: &str = r#"
def handler(block: () -> None):
    block()

def against(block: str.() -> None):
    "receiver".block()

seen: list[str] = []

def main() -> None:
    handler:
        seen.append("handled")
    against:
        seen.append(upper())

main()
assert seen == ["handled", "RECEIVER"], seen
print("ok")
"#;

/// a `let` declared inside a block is the block's own local even when the
/// enclosing function binds the same name — here a `match` capture — so the
/// lowering must not make it `nonlocal`, which an annotated name cannot be
const BLOCK_LET_PROGRAM: &str = r#"
import asyncio
from typing import Awaitable

async def load(name: str) -> str:
    return name.upper()

async def scope(once block: () -> Awaitable[None]):
    await block()

def run(once block: () -> None):
    block()

seen: list[str] = []

async def main() -> None:
    match "morgan":
        case str() as user:
            await scope():
                let user = await load("nested")
                seen.append(user)
            seen.append(user)
    # a synchronous block and an annotated declaration behave the same way
    total: int = 1
    run:
        total: int = 2
        seen.append(str(total))
    seen.append(str(total))

asyncio.run(main())
assert seen == ["NESTED", "morgan", "2", "1"], seen
print("ok")
"#;

fn run_program(python: &str, program: &str, what: &str) {
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
        "transpiled {what} program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

#[test]
fn a_callback_passing_fewer_arguments_can_still_call_the_block() {
    let Some(python) = python() else {
        eprintln!("skipping block-scoping runtime test: no `python3` interpreter found");
        return;
    };
    run_program(&python, UNFILLED_PARAMETERS_PROGRAM, "unfilled-parameters");
}

#[test]
fn let_inside_a_block_is_a_block_local() {
    let Some(python) = python() else {
        eprintln!("skipping block-scoping runtime test: no `python3` interpreter found");
        return;
    };
    run_program(&python, BLOCK_LET_PROGRAM, "block-`let`");
}
