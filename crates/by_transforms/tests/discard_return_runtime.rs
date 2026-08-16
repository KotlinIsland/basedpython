//! Runtime test for the adapter a callable gets at a `-> None` conversion site.
//!
//! The mdtest shows which sites the checker accepts and the text-level test shows
//! the wrap; what neither can show is that the adapter behaves like the callable
//! it replaced. Every claim exercised here is one a plausible-looking adapter
//! would break:
//!
//! - the result really is `None`, which is the whole reason the site was allowed
//!   to accept a callable returning something else
//! - the wrapped callable still runs, and every argument reaches it unchanged —
//!   positional, keyword, variadic and keyword-variadic
//! - a wrapped callback can still be deregistered. python does that by value
//!   (`observers.remove(cb)`, `seen.discard(cb)`), and each site wraps the value
//!   separately, so two *different* adapter objects have to compare equal and
//!   hash alike or the callback is stuck in the list forever
//! - attributes still answer, so a framework reading `cb.__name__` off a wrapped
//!   callback gets the wrapped function's name rather than an `AttributeError`

use std::process::{Command, Stdio};

use by_transforms::{Config, PythonVersion, transpile};

/// basedpython whose module-level `assert`s exercise the adapter end to end
const PROGRAM: &str = r#"
calls: list[str] = []

def handler() -> int:
    calls.append("handler")
    return 1

def varied(a: int, /, name: str = "n", *rest: int, **kw: int) -> str:
    calls.append(f"{a}|{name}|{rest}|{sorted(kw.items())}")
    return "done"

# the site declared `None`, so `None` is what the caller sees — the value the
# wrapped callable returned is gone rather than merely mistyped
def call_it(cb: () -> None) -> object:
    return cb()

assert call_it(handler) is None
assert calls == ["handler"], calls
calls.clear()

# a lambda converts the same way a named function does
assert call_it(lambda: 1) is None

# every argument reaches the wrapped callable unchanged
def call_varied(cb: (int, /, name: str, *args: int, **kwargs: int) -> None) -> None:
    cb(1, name="x", **{"k": 3})

call_varied(varied)
assert calls == ["1|x|()|[('k', 3)]"], calls
calls.clear()

# deregistration by value: the adapter at the `remove` site is a different object
# from the one at the `append` site, so this only works if they compare equal
observers: list[() -> None] = []

def subscribe(cb: () -> None) -> None:
    observers.append(cb)

def unsubscribe(cb: () -> None) -> None:
    observers.remove(cb)

subscribe(handler)
assert len(observers) == 1
for observer in observers:
    assert observer() is None
assert calls == ["handler"], calls
calls.clear()
unsubscribe(handler)
assert observers == [], observers

# the same through a hash-based container, which needs `__hash__` to agree too
seen: set[() -> None] = set()
seen.add(handler)
assert len(seen) == 1
seen.discard(handler)
assert len(seen) == 0, len(seen)

# a method is reached through a different branch of the check that decides
# whether a call is worth inspecting at all, so it needs its own exercise
class Registry:
    def __init__(self):
        self.items: list[() -> None] = []

    def add(self, cb: () -> None) -> None:
        self.items.append(cb)

registry = Registry()
registry.add(handler)
assert registry.items[0]() is None
assert calls == ["handler"], calls
calls.clear()

# attributes answer off the wrapped callable
held: () -> None = handler
assert getattr(held, "__name__") == "handler", getattr(held, "__name__")

# a callable that arrives in a variable converts as one named at the site does
source: () -> int = handler
assert call_it(source) is None
calls.clear()

# a `return`
def make() -> (() -> None):
    return handler

assert make()() is None
calls.clear()

# an attribute assignment
class Widget:
    on_click: () -> None

widget = Widget()
widget.on_click = handler
assert widget.on_click() is None
calls.clear()

# element-wise inside a literal collection: each element is wrapped where it
# stands, and each one still discards
callbacks: list[() -> None] = [handler, handler]
assert [cb() for cb in callbacks] == [None, None]
assert calls == ["handler", "handler"], calls

# a callable that already returns `None` is untouched, so it stays the very same
# object rather than picking up an adapter it does not need
def silent() -> None: ...

plain: () -> None = silent
assert plain === silent

print("ok")
"#;

/// the first interpreter that accepts `probe`
fn python_supporting(probe: &str) -> Option<String> {
    let mut candidates = Vec::new();
    if let Ok(p) = std::env::var("PYTHON") {
        candidates.push(p);
    }
    candidates.extend(["python3.13", "python3"].map(String::from));

    candidates.into_iter().find(|py| {
        Command::new(py)
            .args(["-c", probe])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn discarded_returns_run_correctly() {
    let Some(python) = python_supporting("import sys; sys.exit(0)") else {
        eprintln!("skipping discard-return runtime test: no python interpreter found");
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
        "transpiled discard-return program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
