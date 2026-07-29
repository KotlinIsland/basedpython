//! Runtime divergence test for per-iteration loop bindings.
//!
//! The transform's unit tests assert on the lowered *text*; the whole point of
//! this feature is a runtime observation — what a closure made in iteration `k`
//! sees when it is called after the loop — so it has to be executed. The
//! programs below assert the values themselves, and also the things the two
//! lowerings could quietly break: a `nonlocal` accumulator writing through the
//! loop's own cell, a decorated / annotated / recursive `def` surviving the
//! closure rebuild, and a generator or coroutine staying one.
//!
//! No third-party packages are needed, so any `python3` will do; if none is
//! found the test skips rather than fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

/// the capture itself, across every form that binds an iteration
const CAPTURES: &str = r#"
# the canonical case: python hands every lambda the one cell and prints 3, 3, 3
fns = []
for i in [1, 2, 3]:
    fns.append(lambda: i)
assert [fn() for fn in fns] == [1, 2, 3], "a lambda captures its own iteration"

# a `def` is rebuilt with fresh cells rather than wrapped. the loop has to be
# inside a function: at module level python reads the target as a global from
# inside the `def`, and there is no cell to rebind
def collect_defs() -> list:
    defs = []
    for i in [1, 2, 3]:
        def keep():
            return i
        defs.append(keep)
    return defs

assert [fn() for fn in collect_defs()] == [1, 2, 3], "a def captures its own iteration"

# a comprehension target has the same one-cell-per-comprehension behaviour
comprehended = [lambda: i for i in [1, 2, 3]]
assert [fn() for fn in comprehended] == [1, 2, 3], "a comprehension target is per-element"

# a generator expression is lazy, so it captures too
lazy = []
for i in [1, 2, 3]:
    lazy.append(i * x for x in [10])
assert [next(g) for g in lazy] == [10, 20, 30], "a generator expression captures"

# every target of a destructuring loop binds
pairs = []
for left, right in [(1, 2), (3, 4)]:
    pairs.append(lambda: (left, right))
assert [fn() for fn in pairs] == [(1, 2), (3, 4)], "each unpacked target binds"

# nested loops each contribute their own binding
grid = []
for row in [1, 2]:
    for column in [3, 4]:
        grid.append(lambda: (row, column))
assert [fn() for fn in grid] == [(1, 3), (1, 4), (2, 3), (2, 4)], "nested loops both bind"

# a method defined in a class in a loop closes over the iteration too
def collect_classes() -> list:
    classes = []
    for i in [1, 2]:
        class Holder:
            def value(self):
                return i
        classes.append(Holder())
    return classes

assert [held.value() for held in collect_classes()] == [1, 2], "a method in a loop captures"

# a trailing-lambda block is a `def` the parser synthesized, so it is not itself
# rebound — but a closure written inside it is still made once per iteration
def run(once fn: () -> None):
    fn()

def collect_blocks() -> list:
    made = []
    for i in [1, 2]:
        run:
            made.append(lambda: i)
    return made

assert [fn() for fn in collect_blocks()] == [1, 2], "a block's body is still bound"

# the loop variable still reads normally inside the body
seen = []
for i in [1, 2, 3]:
    seen.append(i)
assert seen == [1, 2, 3], "an ordinary read is untouched"

# the `else` clause runs once, after the last value — nothing to bind
for i in [1, 2, 3]:
    pass
else:
    after = lambda: i
assert after() == 3, "the else clause sees the final value"

print("ok")
"#;

/// what the closure rebuild must not damage
const REBUILD: &str = r#"
# a `nonlocal` write goes through the loop's own cell, so that name is never
# rebound — only the loop binding it reads is
def accumulate() -> tuple[int, list[int]]:
    total = 0
    bumps = []
    for i in [1, 2, 3]:
        def bump():
            nonlocal total
            total += i
        bumps.append(bump)
    for bump in bumps:
        bump()
    return total, [1, 2, 3]

assert accumulate()[0] == 6, "a nonlocal accumulator still writes through"

# user decorators still see the function, and still wrap the rebuilt one
def tag(fn):
    fn.tagged = True
    return fn

def collect_decorated() -> list:
    decorated = []
    for i in [1, 2]:
        @tag
        def described(a: int, b: str = "b") -> str:
            """doc"""
            return f"{i}{a}{b}"
        decorated.append(described)
    return decorated

decorated = collect_decorated()
assert [fn(1) for fn in decorated] == ["11b", "21b"], "defaults and captures both survive"
assert all(fn.tagged for fn in decorated), "the user decorator applied to the rebuilt function"
assert decorated[0].__name__ == "described", "the name survives the rebuild"
assert decorated[0].__doc__ == "doc", "the docstring survives the rebuild"
assert decorated[0].__defaults__ == ("b",), "the defaults survive the rebuild"
import typing
assert set(typing.get_type_hints(decorated[0])) == {"a", "b", "return"}, "annotations survive"

# the signature is what every framework reads to decide what a function wants —
# a query parameter, a fixture, a validated argument — so the rebuild must not
# perturb it. `getfullargspec` is checked too: it is the other reader in the wild
import inspect
assert list(inspect.signature(decorated[0]).parameters) == ["a", "b"], "the signature is exact"
assert str(inspect.signature(decorated[0])) == "(a: int, b: str = 'b') -> str", (
    "annotations and defaults read back unchanged"
)
assert inspect.getfullargspec(decorated[0]).args == ["a", "b"], "argspec is exact"
assert not inspect.getfullargspec(decorated[0]).kwonlyargs, "no parameter was injected"

# recursion goes through the name, which the decorated definition rebound to
# the rebuilt function — so the call resolves and reaches this iteration's value
def count_down() -> list:
    results = []
    for i in [2, 3]:
        def countdown(n: int) -> int:
            return i if n == 0 else countdown(n - 1)
        results.append(countdown(2))
    return results

assert count_down() == [2, 3], "a recursive def keeps working"

# a generator function is still a generator after the rebuild
def collect_generators() -> list:
    generators = []
    for i in [1, 2]:
        def steps():
            yield i
            yield i * 10
        generators.append(steps)
    return generators

assert [list(fn()) for fn in collect_generators()] == [[1, 10], [2, 20]], (
    "a generator def is rebuilt as one"
)

# a zero-argument `super()` needs its implicit `__class__` cell to survive
class Base:
    def label(self) -> str:
        return "base"

def collect_subclasses() -> list:
    subclasses = []
    for i in [1, 2]:
        class Sub(Base):
            def label(self) -> str:
                return f"{super().label()}{i}"
        subclasses.append(Sub())
    return subclasses

assert [sub.label() for sub in collect_subclasses()] == ["base1", "base2"], "`__class__` survives"

# a coroutine is still a coroutine
import asyncio

async def collect() -> list[int]:
    coroutines = []
    for i in [1, 2]:
        async def value() -> int:
            return i
        coroutines.append(value)
    return [await fn() for fn in coroutines]

assert asyncio.run(collect()) == [1, 2], "an async def is rebuilt as a coroutine function"

print("ok")
"#;

/// what stays exactly as python has it
const UNCHANGED: &str = r#"
# a closure that binds the name itself is not touched
shadowed = []
for i in [1, 2]:
    shadowed.append(lambda i=9: i)
assert [fn() for fn in shadowed] == [9, 9], "a parameter of the same name shadows"

inner = []
for i in [1, 2]:
    inner.append(lambda: [i for i in [7]])
assert [fn() for fn in inner] == [[7], [7]], "an inner comprehension target shadows"

# a `while` loop has no target, so its closures still share the one binding
whiles = []
n = 0
while n < 3:
    n += 1
    whiles.append(lambda: n)
assert [fn() for fn in whiles] == [3, 3, 3], "a while loop is python's"

# so does a name merely assigned in the body — including a `def`'s own name,
# which is why a function called after the loop recurses into the last
# definition rather than into itself
def collect_locals() -> list:
    reads = []
    for i in [1, 2]:
        doubled = i * 2
        reads.append(lambda: doubled)
    return reads

assert [fn() for fn in collect_locals()] == [4, 4], "a body local is python's"

def collect_counters() -> list:
    counters = []
    for i in [2, 3]:
        def countdown(n: int) -> int:
            return i if n == 0 else countdown(n - 1)
        counters.append(countdown)
    return counters

assert [fn(2) for fn in collect_counters()] == [3, 3], "a def's own name is a body local"

# a `def` in a *module-level* loop reads the target as a global, which the
# closure rebind cannot reach — and a closure inside it is left alone too,
# rather than frozen where the body runs
module_defs = []
for i in [1, 2]:
    def module_level():
        return i
    module_defs.append(module_level)
assert [fn() for fn in module_defs] == [2, 2], "a module-level def is python's"

# the capture happens where the closure is written, so a later rebind of the
# target in the same iteration is not seen by it
rebound = []
for i in [1]:
    rebound.append(lambda: i)
    i = 99
assert [fn() for fn in rebound] == [1], "the value at the point the closure was made"

print("ok")
"#;

/// a reified type parameter is itself a closure cell, so a reified generic
/// defined in a loop puts both kinds of cell through the rebuild at once: the
/// `@generic` wrapper reads `__type_params__` off the function it receives, and
/// rebuilds a closure that must still carry the frozen loop binding
const REIFIED: &str = r#"
def collect() -> list:
    made = []
    for i in [1, 2]:
        def probe[T](x: object) -> str:
            return f"{i}:{isinstance(x, T)}"
        made.append(probe[int](1))
    return made

assert collect() == ["1:True", "2:True"], "the loop binding and the type argument both survive"

print("ok")
"#;

/// reification keeps native pep 695 syntax, so it needs a 3.12+ interpreter
fn python_with_native_type_params(python: &str) -> bool {
    Command::new(python)
        .args([
            "-c",
            "import sys; raise SystemExit(0 if sys.version_info >= (3, 12) else 1)",
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn run(python: &str, program: &str, unique_loop_bindings: bool) {
    let config = Config {
        min_version: PythonVersion::PY313,
        unique_loop_bindings,
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
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn loop_bindings_are_per_iteration() {
    let Some(python) = python() else {
        eprintln!("skipping loop-binding runtime test: no `python3` interpreter found");
        return;
    };
    run(&python, CAPTURES, true);
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn the_closure_rebuild_preserves_the_function() {
    let Some(python) = python() else {
        eprintln!("skipping loop-binding runtime test: no `python3` interpreter found");
        return;
    };
    run(&python, REBUILD, true);
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn what_stays_pythons_stays_pythons() {
    let Some(python) = python() else {
        eprintln!("skipping loop-binding runtime test: no `python3` interpreter found");
        return;
    };
    run(&python, UNCHANGED, true);
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn a_reified_generic_in_a_loop_keeps_both_bindings() {
    let Some(python) = python() else {
        eprintln!("skipping loop-binding runtime test: no `python3` interpreter found");
        return;
    };
    if !python_with_native_type_params(&python) {
        eprintln!("skipping reified-generic runtime test: {python} is older than 3.12");
        return;
    }
    run(&python, REIFIED, true);
}

/// with the option off the output is python's, sharing one cell per loop —
/// the same program that asserts per-iteration values must fail
#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn disabling_the_option_restores_pythons_sharing() {
    let Some(python) = python() else {
        eprintln!("skipping loop-binding runtime test: no `python3` interpreter found");
        return;
    };
    let config = Config {
        min_version: PythonVersion::PY313,
        unique_loop_bindings: false,
        ..Config::default()
    };
    let transpiled = transpile(CAPTURES, &config).expect("transpile should succeed");
    assert!(
        !transpiled.contains("_by_loop_bind"),
        "the disabled pass emitted its runtime:\n{transpiled}"
    );
    let output = Command::new(&python)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");
    assert!(
        !output.status.success(),
        "the disabled lowering still produced per-iteration bindings:\n{transpiled}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("a lambda captures its own iteration"),
        "expected the first per-iteration assertion to fail, got:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}
