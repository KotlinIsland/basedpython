//! Runtime test for a decorator written above a binding.
//!
//! The transform tests show that the decorator lines go and the written type is
//! wrapped in an `Annotated`; what they cannot show is that the metadata is really
//! there afterwards, where the libraries this exists for go looking for it. Every
//! claim exercised here is one a plausible-looking lowering would break:
//!
//! - the name holds the value that was written under the decorator, unchanged —
//!   the decorator annotates, it does not wrap
//! - `get_type_hints(..., include_extras=True)` — how pydantic, attrs and msgspec
//!   read field metadata — answers with the metadata attached
//! - a chain reads innermost first, the order `@a @b int` puts them in
//! - a decorator written with arguments contributes the call's *result*
//! - a decoration already in the type position and one above the binding both land
//!   on the same type, and `Annotated` flattens the two together
//! - the declaration keywords still declare what they declared

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;

/// basedpython whose module-level `assert`s exercise the lowering end to end
const PROGRAM: &str = r#"
import sys
from typing import Annotated, Final, get_type_hints

def tag(label: str) -> str:
    return f"tag:{label}"

# the binding holds what was written, not something the decorator returned
class Config:
    @tag("port")
    let port: int = 8080

assert Config.port == 8080, Config.port

# and the metadata is where a library reads it from
hints = get_type_hints(Config, include_extras=True)
assert hints["port"] == Annotated[int, "tag:port"], hints["port"]

# a chain reads innermost first
class Chained:
    @tag("outer")
    @tag("inner")
    let both: int = 1

assert get_type_hints(Chained, include_extras=True)["both"] == Annotated[
    int, "tag:inner", "tag:outer"
], get_type_hints(Chained, include_extras=True)["both"]

# a decoration in the type position and one above the binding add up
class Together:
    @tag("above")
    let value: @tag("inline") int = 2

assert Together.value == 2, Together.value
assert get_type_hints(Together, include_extras=True)["value"] == Annotated[
    int, "tag:inline", "tag:above"
], get_type_hints(Together, include_extras=True)["value"]

# a lowering inside the type still happens
class Optional_:
    @tag("maybe")
    let maybe: int? = None

assert get_type_hints(Optional_, include_extras=True)["maybe"] == Annotated[
    int | None, "tag:maybe"
], get_type_hints(Optional_, include_extras=True)["maybe"]

# a `let` at module scope is still `Final`, with the metadata inside it
@tag("module")
let setting: int = 3

assert setting == 3, setting
module_hints = get_type_hints(sys.modules[__name__], include_extras=True)
assert module_hints["setting"] == Final[Annotated[int, "tag:module"]], module_hints["setting"]

# the value is evaluated once, and the decorator does not see it
calls: list[str] = []

def once() -> int:
    calls.append("once")
    return 4

class Evaluated:
    @tag("field")
    let field: int = once()

assert calls == ["once"], calls
assert Evaluated.field == 4, Evaluated.field

print("ok")
"#;

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn decorated_bindings_run_correctly() {
    let Some(python) = common::python() else {
        eprintln!("skipping decorated-binding runtime test: no python interpreter found");
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
        "transpiled decorated-binding program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
