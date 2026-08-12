//! the differential harness: compiled and interpreted must agree
//!
//! the whole design rests on one property — a compiled module and its
//! interpreted twin are observably identical. this test asserts it directly:
//! the same `.by` source is transpiled to python *and* compiled to an extension,
//! both are called with the same arguments, and the two `repr`s must match
//! character for character.
//!
//! it is the strongest test in the suite, because it needs no expected values of
//! its own — cpython supplies them.
//!
//! see docs/basedpython/development/compilation/plan.md#differential-testing

#![expect(
    clippy::print_stderr,
    reason = "skip notices belong on the test harness's stderr"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use by_build::{Options, Toolchain, build_source};
use by_transforms::Config;

mod common;

/// a test whose *source* needs a newer interpreter than this one has nothing to
/// say: neither leg can run it, so there is nothing to compare
fn supports(toolchain: &Toolchain, least: (u8, u8)) -> bool {
    toolchain.version.is_none_or(|version| version >= least)
}

/// whether a build error means there is no C compiler, rather than that the compiler
/// produced something one rejected
fn missing_toolchain(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let text = cause.to_string();
        text.contains("could not run the C compiler") || text.contains("No such file or directory")
    })
}

fn environment() -> Option<(String, Toolchain)> {
    let python = match std::env::var("PYTHON") {
        Ok(python) => python,
        Err(_) => ["python3", "python"]
            .into_iter()
            .find(|candidate| {
                Command::new(candidate)
                    .arg("--version")
                    .output()
                    .is_ok_and(|out| out.status.success())
            })?
            .to_string(),
    };
    let toolchain = Toolchain::probe(&python).ok()?;
    Some((python, toolchain))
}

/// a helper both legs get, so a raising call can be compared by type and message
/// rather than only by the fact that it raised
const CAPTURE_HELPER: &str = "\
import gc

class _Track:
    def __init__(self, log):
        self.log = log
    def __enter__(self):
        self.log.append('enter')
        return 'x'
    def __exit__(self, *a):
        self.log.append('exit')
        return False

def _traced(fn):
    log = []
    return (fn(_Track(log)), log)

# a manager on both protocols that records *which* exception each exit saw. the
# whole of the early-exit question is whether `__exit__` is told the block failed,
# so the kind is the observation and `_Track`'s bare 'exit' cannot make it
class _Both:
    def __init__(self, log, tag=''):
        self.log, self.tag = log, tag
    def __enter__(self):
        self.log.append('enter' + self.tag)
        return 'held' + self.tag
    def __exit__(self, kind, value, tb):
        self.log.append(('exit' + self.tag, kind))
        return False
    async def __aenter__(self):
        self.log.append('enter' + self.tag)
        return 'held' + self.tag
    async def __aexit__(self, kind, value, tb):
        self.log.append(('exit' + self.tag, kind))
        return False

def _logged(fn):
    log = []
    try:
        return (fn(_Both(log), log), log)
    except BaseException as e:
        return ((type(e).__name__, str(e)), log)

# run something many times and hand back the last answer, so a build that leaked or
# freed too early shows as a crash or a wrong value rather than as nothing
def _repeated(fn, times):
    out = None
    for _ in range(times):
        out = fn()
    return out

# how many references a cached object gained over `times` calls. a leak of a module
# or any other interned object is invisible to `gc.get_objects` — it is one object
# with a climbing refcount — so the count is the observation
def _refdelta(name, fn, times):
    import sys
    # the first call is what puts a lazily-imported module in `sys.modules` at all
    fn()
    held = sys.modules[name]
    before = sys.getrefcount(held)
    for _ in range(times):
        fn()
    return sys.getrefcount(held) - before

# hand the callable a fresh accumulator and report both the answer and whether the
# receiver was the object that came back
def _inplace(fn):
    a = m.Acc(12)
    out = fn(a)
    return (out, out is a)

# drive the generator and then read the cell the enclosing frame shares with it
def _counted(pair):
    gen, read = pair
    return (list(gen()), read())

async def _drain(source):
    out = []
    async for v in source:
        out.append(v)
    return out

async def _comprehended(source):
    return [v async for v in source]

async def _slow(i):
    await asyncio.sleep(0)
    return i * 10

async def _awaited(fn, n):
    return await _drain(fn(_slow, n))

# the exhaustion is the observation: a *second* `__anext__` has to keep raising
async def _stepped(source):
    first = await source.__anext__()
    out = [first]
    for _ in range(2):
        try:
            out.append(await source.__anext__())
        except StopAsyncIteration as e:
            out.append(type(e).__name__)
    return out

# `asend(v)` carries a value in where `__anext__` carries nothing, and `asend(None)`
# is the same thing as `__anext__`
async def _echoed(source):
    return [await source.__anext__(), await source.asend('x'), await source.asend(None)]

# a thrown exception resumes *at the suspension*, so the body's own handler sees it —
# and one the body does not catch comes back out
async def _thrown(fn, error):
    log = []
    source = fn(log, 5)
    first = await source.__anext__()
    try:
        await source.athrow(error('boom'))
        outcome = 'returned'
    except StopAsyncIteration:
        outcome = 'stopped'
    except BaseException as e:
        outcome = type(e).__name__
    return (first, outcome, log)

async def _closed_async(fn, n):
    log = []
    source = fn(log, n)
    seen = [await source.__anext__(), await source.__anext__()]
    closed = await source.aclose()
    try:
        await source.__anext__()
    except StopAsyncIteration:
        seen.append('stopped')
    return (seen, closed, log)

async def _drained_with_log(fn, n):
    log = []
    return (await _drain(fn(log, n)), log)

# a log the module's *own* manager writes into, for a class that is itself compiled
def _own(fn):
    log = []
    return (fn(log), log)

def _nested(fn):
    log = []
    return (fn(_Both(log, 'A'), _Both(log, 'B'), log), log)

# what a generator *returned*, which only its `StopIteration` carries
def _value(gen):
    try:
        while True:
            next(gen)
    except StopIteration as e:
        return e.value

def _closed(gen, steps):
    out = [next(gen) for _ in range(steps)]
    gen.close()
    return out

def _discarded(gen):
    try:
        next(gen)
    except BaseException as e:
        del gen
        gc.collect()
        return type(e).__name__
    del gen
    gc.collect()
    return 'no'

def _abandoned(gen, steps):
    out = [next(gen) for _ in range(steps)]
    del gen
    gc.collect()
    return out

def _raised(gen):
    next(gen)
    try:
        gen.throw(ValueError('stop'))
    except ValueError as e:
        return 'threw ' + str(e)
    return 'no'

# an exception that leaves a generator finishes it: a later step is `StopIteration`
# rather than a resumption, closing it runs nothing more, and neither does dropping
# it — the cleanup the exception already unwound must not run a second time
def _after_raising(gen):
    next(gen)
    try:
        gen.throw(ValueError('stop'))
    except ValueError:
        pass
    out = []
    try:
        out.append(next(gen))
    except StopIteration:
        out.append('stopped')
    out.append(gen.close())
    del gen
    gc.collect()
    return out

import asyncio

def _run(coro):
    return asyncio.run(coro)

def _capture_async(fn, *args):
    try:
        asyncio.run(fn(*args))
    except BaseException as e:
        return type(e).__name__ + ': ' + str(e)
    return None

class _Counter:
    def __init__(self, limit):
        self.limit = limit
        self.n = 0
    def __aiter__(self):
        return self
    async def __anext__(self):
        if self.n >= self.limit:
            raise StopAsyncIteration
        self.n += 1
        return self.n

def _counter(limit):
    return _Counter(limit)

class _Boom:
    def __aiter__(self):
        return self
    async def __anext__(self):
        raise ValueError('boom')

class _NoNext:
    def __aiter__(self):
        return 5

_tiny = type('Tiny', (float,), {})

class _Loud(list):
    def append(self, value):
        super().append(('loud', value))
        self.seen = True

class _Counted:
    def __init__(self):
        self.items = []
    def append(self, value):
        self.items.append(value)
    def __repr__(self):
        return 'Counted(' + repr(self.items) + ')'

# a *python* method, so the receiver is reached through `_PyFunction_Vectorcall` —
# the frame that increfs each argument, and so the one a NULL argument kills
class _Sink:
    def absorb(self, item):
        return ('got', item)

class _Reflected:
    def __radd__(self, other):
        return other + 100.0
    def __rmul__(self, other):
        return other * 100.0

def _delete_first(g):
    del g[0]

def _capture(fn, *args):
    try:
        fn(*args)
    except BaseException as e:
        return e
    return None

# step a generator, sending each value in and recording what comes back — an
# exhaustion or a raise included, so a wrong answer after a suspension is visible
# rather than fatal
def _sent(gen, values):
    out = []
    try:
        out.append(next(gen))
        for value in values:
            out.append(gen.send(value))
    except BaseException as e:
        out.append((type(e).__name__, str(e), getattr(e, 'value', None)))
    return out

# a `throw` the generator *handles* leaves it usable, so every later step has to go
# on rather than raise again at the suspension it was thrown into
def _recovered(gen, error, steps):
    out = []
    try:
        out.append(next(gen))
        out.append(gen.throw(error('x')))
        for _ in range(steps):
            out.append(next(gen))
    except BaseException as e:
        out.append((type(e).__name__, str(e)))
    return out

def _capture_kw(fn, args, kwargs):
    try:
        fn(*args, **kwargs)
    except BaseException as e:
        return e
    return None

class _Swallow:
    def __enter__(self): return self
    def __exit__(self, *a): return True

class _Pass:
    def __enter__(self): return self
    def __exit__(self, *a): return False

class _Value:
    def __init__(self, v): self.v = v
    def __enter__(self): return self.v
    def __exit__(self, *a): return False

class _Recording:
    def __init__(self): self.seen = []
    def __enter__(self):
        self.seen.append('enter')
        return self
    def __exit__(self, *a):
        self.seen.append(a)
        return False

def _run_recording(m):
    r = _Recording()
    return (m.guarded(r), r.seen)

def _run_nested(m):
    a, b = _Recording(), _Recording()
    return (m.nested(a, b), a.seen, b.seen)

def _chain(e):
    out = []
    while e is not None:
        out.append((type(e).__name__, str(e), e.__suppress_context__))
        e = e.__cause__ or (None if e.__suppress_context__ else e.__context__)
    return out

# every shape the await protocol has to answer for. an awaited object reaches a
# compiled frame as an ordinary `object`, so these are the interpreted side of the
# boundary and none of them is a coroutine the compiler built

# finishes at once, through `__await__` rather than by being a coroutine
class _Ready:
    def __init__(self, value):
        self.value = value
    def __await__(self):
        return self._done()
    def _done(self):
        return self.value
        yield

class _RaiseIter:
    def __init__(self, error):
        self.error = error
    def __iter__(self):
        return self
    def __next__(self):
        raise self.error

# `__await__` hands back an iterator that ends on a given exception
class _Raises:
    def __init__(self, error):
        self.error = error
    def __await__(self):
        return _RaiseIter(self.error)

class _Sub(StopIteration):
    pass

# `StopIteration.value` is a *struct* field, so a subclass shadowing the attribute
# changes what `e.value` reads and not what a delegation collects. reading it as an
# attribute is a wrong answer python never gives
class _Shadowed(StopIteration):
    value = 'shadowed'

class _NotIter:
    def __await__(self):
        return 5

# suspends through a real future, so the loop has to resume it
class _Suspends:
    def __init__(self, value):
        self.value = value
    def __await__(self):
        loop = asyncio.get_running_loop()
        pending = loop.create_future()
        loop.call_soon(pending.set_result, self.value)
        return pending.__await__()

async def _sleeps(value):
    await asyncio.sleep(0)
    return value

async def _throws(kind):
    await asyncio.sleep(0)
    raise kind('boom')

class _CoroAwait:
    def __await__(self):
        made = _sleeps(1)
        made.close()
        return made

def _counting(n):
    i = 0
    while i < n:
        yield i
        i = i + 1
    return n * 100
";

fn run(python: &str, dir: &Path, body: &str) -> String {
    common::python_output(python, dir, body)
}

/// run `calls` against both builds of `source` and assert the outputs match
///
/// `calls` is a python expression referring to the module as `m`
fn agree(tag: &str, source: &str, calls: &[&str]) {
    agree_inner(tag, source, calls, false);
}

/// as [`agree`], but the source is expected to contain declined functions
fn agree_with_declines(tag: &str, source: &str, calls: &[&str]) {
    agree_inner(tag, source, calls, true);
}

/// as [`agree`], but the source is ordinary python
///
/// the interpreted leg is then the source *itself* rather than a transpilation of
/// it, which is the whole of what makes a `.py` file compilable: it is already the
/// program. python's own semantics apply — a `float` annotation admits an `int`,
/// and a mixed numeric pair promotes
fn agree_python(tag: &str, source: &str, calls: &[&str]) {
    agree_in(tag, source, calls, false, by_irbuild::Language::Python);
}

/// as [`agree_python`], but the source is expected to contain declined functions
fn agree_python_with_declines(tag: &str, source: &str, calls: &[&str]) {
    agree_in(tag, source, calls, true, by_irbuild::Language::Python);
}

fn agree_inner(tag: &str, source: &str, calls: &[&str], allow_declines: bool) {
    agree_in(
        tag,
        source,
        calls,
        allow_declines,
        by_irbuild::Language::BasedPython,
    );
}

fn agree_in(
    tag: &str,
    source: &str,
    calls: &[&str],
    allow_declines: bool,
    language: by_irbuild::Language,
) {
    let Some((python, toolchain)) = environment() else {
        return;
    };

    let compiled_dir = std::env::temp_dir().join(format!("by_diff_{tag}_c"));
    let interpreted_dir = std::env::temp_dir().join(format!("by_diff_{tag}_i"));
    let _ = std::fs::remove_dir_all(&compiled_dir);
    let _ = std::fs::remove_dir_all(&interpreted_dir);

    let module = format!("by_diff_{tag}");

    // the interpreted leg: for basedpython, the transpiler's own output run by
    // cpython, under the same config `by_build` uses, so the two legs are the same
    // program. for python there is nothing to transpile — it already is one
    let interpreted_source = match language {
        by_irbuild::Language::BasedPython => {
            by_transforms::transpile(source, &Config::default()).expect("the source transpiles")
        }
        by_irbuild::Language::Python => source.to_string(),
    };
    std::fs::create_dir_all(&interpreted_dir).expect("the directory is created");
    std::fs::write(
        interpreted_dir.join(format!("{module}.py")),
        &interpreted_source,
    )
    .expect("the interpreted module is written");

    // the compiled leg
    let options = Options {
        language,
        ..Options::default()
    };
    let built = match build_source(source, &module, &toolchain, &compiled_dir, &options) {
        Ok(built) => built,
        Err(error) => {
            // only an *absent* toolchain is a skip. anything else is the compiler
            // failing to build source it was handed, which is exactly what this suite
            // exists to catch — and treating the two alike made a build failure pass
            assert!(
                missing_toolchain(&error),
                "{tag} failed to build: {error:#}"
            );
            eprintln!("skipping {tag}: no working C toolchain ({error})");
            return;
        }
    };
    if !allow_declines {
        assert!(
            built.declined.is_empty(),
            "{tag} declined functions the test expects to be compiled: {:?}",
            built.declined
        );
    }

    for call in calls {
        let body = format!("import {module} as m\n{CAPTURE_HELPER}print(repr({call}))\n");
        let compiled = run(&python, &compiled_dir, &body);
        let interpreted = run(&python, &interpreted_dir, &body);
        assert_eq!(
            compiled, interpreted,
            "{tag}: `{call}` differs — compiled {compiled}, interpreted {interpreted}"
        );
    }
}

#[test]
fn integer_arithmetic_agrees() {
    agree(
        "arith",
        "\
def mix(a: int, b: int) -> int:
    return (a + b) * a - b // 2
",
        &[
            "m.mix(3, 4)",
            "m.mix(0, 1)",
            "m.mix(-5, 7)",
            "m.mix(-7, -3)",
            "m.mix(10**20, 3)",
        ],
    );
}

#[test]
fn floor_division_and_modulo_agree_on_every_sign() {
    agree(
        "divmod",
        "\
def fdiv(a: int, b: int) -> int:
    return a // b

def fmod(a: int, b: int) -> int:
    return a % b
",
        &[
            "[m.fdiv(a, b) for a in (7, -7) for b in (2, -2)]",
            "[m.fmod(a, b) for a in (7, -7) for b in (2, -2)]",
            "m.fdiv(10**25, 7)",
            "m.fmod(10**25, 7)",
        ],
    );
}

#[test]
fn a_zero_divisor_raises_what_the_interpreter_raises() {
    // an unboxed division performs itself, so nothing in cpython has raised by the time
    // a zero divisor is found — and the wording is not one string. python names the
    // operand type and the operation, differently for each of the six, and 3.14 replaced
    // the lot with `division by zero`. so the operation is re-performed through the
    // abstract api to raise, rather than a copy of the wording being carried
    agree(
        "divzero",
        "\
def fdiv(a: int, b: int) -> int:
    return a // b

def fmod(a: int, b: int) -> int:
    return a % b

def tdiv(a: int, b: int) -> float:
    return a / b

def float_fdiv(a: float, b: float) -> float:
    return a // b

def float_mod(a: float, b: float) -> float:
    return a % b

def float_tdiv(a: float, b: float) -> float:
    return a / b
",
        &[
            "[(type(e).__name__, str(e)) for e in [_capture(m.fdiv, 1, 0)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.fmod, 1, 0)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.tdiv, 1, 0)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.float_fdiv, 1.0, 0.0)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.float_mod, 1.0, 0.0)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.float_tdiv, 1.0, 0.0)]]",
        ],
    );
}

#[test]
fn control_flow_agrees() {
    agree(
        "control",
        "\
def classify(n: int) -> int:
    if n < 0:
        return -1
    elif n == 0:
        return 0
    else:
        return 1

def total(n: int) -> int:
    acc = 0
    i = 0
    while i < n:
        acc = acc + i * i
        i = i + 1
    return acc
",
        &[
            "[m.classify(n) for n in (-3, 0, 5)]",
            "[m.total(n) for n in (0, 1, 10, 100)]",
            "m.total(2000)",
        ],
    );
}

#[test]
fn float_arithmetic_agrees() {
    agree(
        "float",
        "\
def area(r: float) -> float:
    return 3.141592653589793 * r * r

def blend(a: float, b: float, t: float) -> float:
    return a * (1.0 - t) + b * t
",
        &[
            "m.area(1.0)",
            "m.area(2.5)",
            "m.blend(0.0, 10.0, 0.25)",
            "m.blend(-1.5, 1.5, 0.5)",
        ],
    );
}

#[test]
fn calls_across_functions_agree() {
    agree(
        "calls",
        "\
def square(n: int) -> int:
    return n * n

def sum_squares(n: int) -> int:
    acc = 0
    i = 1
    while i <= n:
        acc = acc + square(i)
        i = i + 1
    return acc
",
        &[
            "m.sum_squares(0)",
            "m.sum_squares(10)",
            "m.sum_squares(300)",
        ],
    );
}

#[test]
fn for_loops_over_range_agree() {
    agree(
        "forrange",
        "\
def total(n: int) -> int:
    acc = 0
    for i in range(n):
        acc = acc + i
    return acc

def stepped(start: int, stop: int) -> int:
    acc = 0
    for i in range(start, stop, 3):
        acc = acc + i
    return acc

def countdown(n: int) -> int:
    acc = 0
    for i in range(n, 0, -1):
        acc = acc + i
    return acc

def first_even(n: int) -> int:
    found = -1
    for i in range(n):
        if i % 2 == 1:
            continue
        found = i
        break
    return found

def bound_is_read_once(n: int) -> int:
    seen = 0
    for i in range(n):
        n = 0
        seen = seen + 1
    return seen
",
        &[
            "[m.total(n) for n in (0, 1, 5, 100)]",
            "[m.stepped(a, b) for a in (0, 2) for b in (0, 1, 10)]",
            "[m.countdown(n) for n in (0, 1, 5)]",
            "[m.first_even(n) for n in (0, 1, 2, 5)]",
            // an empty range must not execute the body at all
            "m.total(-5)",
            "m.countdown(-5)",
            // mutating the bound inside the loop does not change the trip count
            "[m.bound_is_read_once(n) for n in (0, 3, 7)]",
        ],
    );
}

#[test]
fn a_range_the_counting_loop_cannot_take_falls_back_rather_than_declining() {
    // plain python, so an unannotated parameter really is gradual — which is what
    // puts a bound out of the counting loop's reach in the first place
    agree_python(
        "rangefall",
        "\
# an `object` bound: `range` takes anything with `__index__`, and the counting
# loop's counter is an int, so this one goes through the iteration protocol
def gradual(n) -> int:
    acc = 0
    for i in range(n):
        acc = acc + i
    return acc

def gradual_start(a, b) -> int:
    acc = 0
    for i in range(a, b):
        acc = acc + i
    return acc

# a computed step decides the comparison direction at runtime
def computed_step(n: int, s: int) -> int:
    acc = 0
    for i in range(0, n, s):
        acc = acc + i
    return acc

# `range` raises these itself, and reaching them is the whole point of falling
# back rather than declining
def zero_step(n: int) -> int:
    acc = 0
    for i in range(0, n, 0):
        acc = acc + i
    return acc

def wrong_arity(n: int) -> int:
    acc = 0
    for i in range(0, n, 1, 2):
        acc = acc + i
    return acc

# the bounds are evaluated once and in order, even on the fallback path
def order(log, a, b) -> int:
    def seen(tag, value):
        log.append(tag)
        return value
    acc = 0
    for i in range(seen('a', a), seen('b', b)):
        acc = acc + i
    return acc
",
        &[
            "[m.gradual(n) for n in (0, 1, 5, 100)]",
            "[m.gradual(True), m.gradual(3)]",
            "[m.gradual_start(a, b) for a in (0, 2) for b in (0, 1, 10)]",
            "[m.computed_step(10, s) for s in (1, 2, 3, -1)]",
            "[m.computed_step(-10, -2)]",
            "(lambda e: (type(e).__name__, str(e)))(_capture(m.zero_step, 5))",
            "(lambda e: (type(e).__name__, str(e)))(_capture(m.wrong_arity, 5))",
            "(lambda e: (type(e).__name__, str(e)))(_capture(m.gradual, 'no'))",
            "(lambda l: (m.order(l, 0, 4), l))([])",
            // a big bound the counting loop's own counter would still hold
            "m.gradual(10 ** 3)",
        ],
    );
}

#[test]
fn booleans_agree_including_their_type() {
    agree(
        "bools",
        "\
def both(a: bool, b: bool) -> bool:
    if a:
        if b:
            return True
        return False
    return False
",
        &[
            "[m.both(a, b) for a in (True, False) for b in (True, False)]",
            // `True` must come back as `True`, not as `1`
            "type(m.both(True, True)).__name__",
        ],
    );
}

#[test]
fn gradual_values_agree_through_the_object_protocol() {
    // a gradual parameter now compiles to an `object` register rather than
    // declining. every operation on it goes through the abstract object
    // protocol, which is what the interpreter would have done anyway
    agree(
        "gradual",
        "\
def add(a, b) -> object:
    return a + b

def compare(a, b) -> bool:
    if a < b:
        return True
    return False

def truthy(a) -> int:
    if a:
        return 1
    return 0

def negate(a) -> object:
    return -a
",
        &[
            "m.add(1, 2)",
            "m.add(1.5, 2.5)",
            "m.add('a', 'b')",
            "m.add([1], [2])",
            "m.add((1,), (2,))",
            "[m.compare(1, 2), m.compare('b', 'a'), m.compare(2.0, 2.0)]",
            "[m.truthy(x) for x in (0, 1, '', 'x', [], [0], None)]",
            "m.negate(5)",
            "m.negate(-1.5)",
        ],
    );
}

#[test]
fn a_mixed_representation_pair_agrees() {
    agree(
        "mixed",
        "\
def scale(n: int, factor) -> object:
    return n * factor

def offset(x: float, delta) -> object:
    return x + delta
",
        &[
            "m.scale(3, 4)",
            "m.scale(3, 1.5)",
            "m.scale(3, 'ab')",
            "m.scale(2, [7])",
            "m.offset(1.5, 2)",
            "m.offset(1.5, 0.25)",
        ],
    );
}

#[test]
fn a_container_parameter_round_trips() {
    agree(
        "containers",
        "\
def identity(xs: list[int]) -> object:
    return xs

def pick(d: dict[str, int], k) -> object:
    return d

def is_empty(xs: list[int]) -> int:
    if xs:
        return 0
    return 1
",
        &[
            "m.identity([1, 2, 3])",
            "m.pick({'a': 1}, 'a')",
            "[m.is_empty([]), m.is_empty([1])]",
        ],
    );
}

#[test]
fn an_error_from_the_object_protocol_propagates() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_objerr");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "def add(a, b) -> object:\n    return a + b\n";
    if build_source(
        source,
        "by_diff_objerr",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // `1 + 'x'` raises TypeError inside PyNumber_Add; the error check after the
    // op has to see it and unwind rather than returning a NULL to python
    let out = run(
        &python,
        &dir,
        "import by_diff_objerr as m\n\
         try:\n    m.add(1, 'x')\n\
         except TypeError:\n    print('caught')\n\
         else:\n    print('no error')\n",
    );
    assert_eq!(out, "caught");
}

#[test]
fn boxed_values_do_not_leak() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_objleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def chain(a, b) -> object:
    x = a + b
    y = x + a
    return y + b
";
    if build_source(
        source,
        "by_diff_objleak",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import gc, by_diff_objleak as m\n\
         for _ in range(50): m.chain([1], [2])\n\
         gc.collect(); before = len(gc.get_objects())\n\
         for _ in range(2000): m.chain([1], [2])\n\
         gc.collect(); after = len(gc.get_objects())\n\
         print('stable' if after <= before + 100 else f'grew {before}->{after}')\n",
    );
    assert_eq!(out, "stable");
}

#[test]
fn an_unboxed_buffer_does_not_leak() {
    // a buffer carries its own reference count so it retains and releases inside the
    // ordinary ownership discipline. appending grows it, and a growth that lost the
    // old allocation — or kept both — would show here and nowhere else
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_bufleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def built(n: int) -> float:
    out = []
    total = 0.0
    i = 0
    while i < n:
        total = total + i * 1.5
        out.append(total)
        i = i + 1
    return out[len(out) - 1]

def comprehended(n: int) -> float:
    out = [i * 2.5 for i in range(n)]
    return out[len(out) - 1]

def scanned(n: int) -> float:
    out = [i * 1.0 for i in range(n)]
    seen = 0.0
    for v in out:
        seen = seen + v
    return seen
";
    if build_source(
        source,
        "by_diff_bufleak",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import gc, by_diff_bufleak as m\n\
         def hit():\n\
         \x20   m.built(64); m.comprehended(64); m.scanned(64)\n\
         for _ in range(50): hit()\n\
         gc.collect(); before = len(gc.get_objects())\n\
         for _ in range(2000): hit()\n\
         gc.collect(); after = len(gc.get_objects())\n\
         print('stable' if after <= before + 100 else f'grew {before}->{after}')\n",
    );
    assert_eq!(out, "stable");
}

#[test]
fn a_refcounted_argument_the_caller_does_not_keep_alive_survives() {
    // the regression that motivated borrowed parameters: the wrapper released
    // every argument *and* so did the callee's cleanup. it only survived because
    // the caller usually holds its own reference — a temporary big int does not,
    // and a temporary object argument segfaults outright
    agree(
        "argowner",
        "\
def echo(x) -> object:
    return x

def twice(n: int) -> int:
    return n + n

def reassigns(n: int) -> int:
    n = n * n
    return n + 1
",
        &[
            // each argument is a fresh temporary with no other reference to it
            "m.echo([1, 2, 3])",
            "m.echo(10 ** 40)",
            "m.echo('a' * 50)",
            "[m.twice(10 ** 30 + i) for i in range(5)]",
            "[m.reassigns(10 ** 20 + i) for i in range(5)]",
            // and repeatedly, so a leak or an over-release shows up
            "sum(m.twice(10 ** 25 + i) for i in range(200))",
        ],
    );
}

#[test]
fn a_temporary_object_argument_does_not_crash() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_argtemp");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "def echo(x) -> object:\n    return x\n";
    if build_source(
        source,
        "by_diff_argtemp",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // a loop over temporaries, so an over-release has many chances to be fatal
    let out = run(
        &python,
        &dir,
        "import by_diff_argtemp as m\n\
         for i in range(20000):\n    m.echo([i, i])\n\
         print('survived')\n",
    );
    assert_eq!(out, "survived");
}

#[test]
fn short_circuit_operators_agree() {
    agree(
        "boolops",
        "\
def both(a, b) -> object:
    return a and b

def either(a, b) -> object:
    return a or b

def three(a, b, c) -> object:
    return a or b or c

def pick(c, a, b) -> object:
    return a if c else b
",
        &[
            // the result is an *operand*, not a bool
            "[m.both(1, 2), m.both(0, 2), m.both('', 'x'), m.both([], [1])]",
            "[m.either(1, 2), m.either(0, 2), m.either('', 'x'), m.either(None, 5)]",
            "[m.three(0, 0, 3), m.three(0, 2, 3), m.three(1, 2, 3)]",
            "[m.pick(True, 'a', 'b'), m.pick(0, 'a', 'b'), m.pick([], 1, 2)]",
        ],
    );
}

#[test]
fn short_circuiting_really_short_circuits() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_shortcircuit");
    let _ = std::fs::remove_dir_all(&dir);
    // dividing by zero raises; `a or b` must not evaluate `b` when `a` is truthy
    let source = "\
def guarded(a: int, b: int) -> object:
    return a or 1 // b
";
    if build_source(
        source,
        "by_diff_shortcircuit",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import by_diff_shortcircuit as m\n\
         print(m.guarded(7, 0))\n",
    );
    assert_eq!(out, "7", "the right operand must not have been evaluated");
}

#[test]
fn chained_comparisons_agree_and_evaluate_once() {
    agree(
        "chains",
        "\
def between(a: int, b: int, c: int) -> bool:
    return a < b < c

def four(a: int, b: int, c: int, d: int) -> bool:
    return a <= b <= c <= d

def mixed(a, b, c) -> bool:
    return a < b < c
",
        &[
            "[m.between(1, 2, 3), m.between(3, 2, 1), m.between(1, 1, 3)]",
            "[m.four(1, 2, 3, 4), m.four(1, 2, 2, 1)]",
            "[m.mixed('a', 'b', 'c'), m.mixed(1.0, 2.0, 1.5)]",
            // the type is bool, not int
            "type(m.between(1, 2, 3)).__name__",
        ],
    );
}

#[test]
fn truthiness_agrees_for_every_representation() {
    agree(
        "truthy",
        "\
def from_int(n: int) -> int:
    if n:
        return 1
    return 0

def from_float(x: float) -> int:
    if x:
        return 1
    return 0

def from_str(s: str) -> int:
    if s:
        return 1
    return 0

def from_object(o) -> int:
    if o:
        return 1
    return 0
",
        &[
            "[m.from_int(n) for n in (0, 1, -1, 10 ** 30)]",
            "[m.from_float(x) for x in (0.0, -0.0, 1.5, -1.5)]",
            "[m.from_str(s) for s in ('', 'x', ' ')]",
            "[m.from_object(o) for o in (0, 1, '', 'x', [], [0], None, {}, {1: 2})]",
        ],
    );
}

#[test]
fn the_remaining_operators_agree() {
    agree(
        "operators",
        "\
def bits(a: int, b: int) -> int:
    return (a & b) + (a | b) + (a ^ b)

def shifts(a: int, b: int) -> int:
    return (a << b) + (a >> b)

def power(a: int, b: int) -> int:
    return a ** b

def fpower(a: float, b: float) -> float:
    return a ** b

def invert(a: int) -> int:
    return ~a

def obj_bits(a, b) -> object:
    return a & b
",
        &[
            "[m.bits(a, b) for a in (0, 5, -6) for b in (0, 3, -1)]",
            "[m.shifts(a, b) for a in (1, -8, 10 ** 20) for b in (0, 1, 5)]",
            "[m.power(a, b) for a in (0, 2, -3) for b in (0, 1, 10)]",
            "m.power(2, 100)",
            "[m.fpower(2.0, 0.5), m.fpower(9.0, 0.5)]",
            "[m.invert(a) for a in (0, 1, -1, 10 ** 20)]",
            "[m.obj_bits(6, 3), m.obj_bits({1, 2}, {2, 3})]",
        ],
    );
}

#[test]
fn assert_and_raise_agree() {
    agree(
        "raises",
        "\
def checked(a: int) -> int:
    assert a > 0, \"must be positive\"
    return a

def always_raises(a: int) -> int:
    raise ValueError(\"nope\")

def conditional_raise(a: int) -> int:
    if a < 0:
        raise IndexError(\"negative\")
    return a
",
        &[
            "m.checked(5)",
            "m.conditional_raise(5)",
            // the exception type *and* message must match, not merely the fact
            "[(type(e).__name__, str(e)) for e in [_capture(m.checked, -1)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.always_raises, 0)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.conditional_raise, -1)]]",
        ],
    );
}

#[test]
fn strings_agree() {
    agree(
        "strings",
        "\
def join(a: str, b: str) -> str:
    return a + b

def size(s: str) -> int:
    return len(s)

def obj_size(o) -> int:
    return len(o)

def compare(a: str, b: str) -> bool:
    return a < b

def order(a: str, b: str) -> object:
    return (a == b, a != b, a < b, a <= b, a > b, a >= b)

def at(s: str, i: int) -> str:
    return s[i]

# a slice is a subscript too, and is not a character read
def part(s: str, a: int, b: int) -> str:
    return s[a:b]

def obj_at(o, i) -> object:
    return o[i]
",
        &[
            "m.join('foo', 'bar')",
            "m.join('', '')",
            "m.join('é', '🎉')",
            "[m.size(s) for s in ('', 'abc', 'é', '🎉')]",
            "[m.obj_size(o) for o in ([], [1, 2], 'abc', {1: 2}, (1, 2, 3))]",
            "[m.compare('a', 'b'), m.compare('b', 'a'), m.compare('a', 'a')]",
            // every operator, over pairs that differ in length, in kind, and only
            // in a character past the first — a comparison that stopped at the
            // narrowest representation would disagree on the mixed-kind pairs
            "[m.order(a, b) for a in ('', 'a', 'ab', 'b', 'é', 'a\\x00', 'a\\u0100', '🎉') for b in ('', 'a', 'ab', 'b', 'é', 'a\\x00', 'a\\u0100', '🎉')]",
            // a subclass may have said its own thing about `==` and about order, and
            // an annotation of `str` admits one
            "m.order(type('S', (str,), {'__eq__': lambda s, o: True, '__lt__': lambda s, o: True, '__hash__': str.__hash__})('z'), 'a')",
            "m.order('a', type('S', (str,), {'__eq__': lambda s, o: True, '__gt__': lambda s, o: True, '__hash__': str.__hash__})('z'))",
            // a plain subclass inherits str's comparisons, and must still get them
            "m.order(type('P', (str,), {})('ab'), 'ab')",
            // a one-character result must be the *same object* the interpreter
            // hands back, not merely an equal one — for latin-1 that is a cached
            // singleton, and a fast path that allocated instead would show up here
            "[m.at('abc', i) for i in (0, 1, 2, -1, -3)]",
            "[m.at(s, 0) is s[0] for s in ('a', 'é', 'ħ', '🎉')]",
            "[m.at('éx🎉', i) for i in (0, 1, 2, -1)]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.at, 'abc', 3), _capture(m.at, 'abc', -4), _capture(m.at, '', 0)]]",
            // a `str` annotation admits a subclass, and a subclass may have said its
            // own thing about indexing — or nothing, and inherit `str`'s
            "[m.at(type('S', (str,), {'__getitem__': lambda self, i: 'Z'})('ab'), i) for i in (0, 1, 9)]",
            "[m.at(type('P', (str,), {})('abc'), i) for i in (0, 2, -1)]",
            "[m.part(s, a, b) for s in ('', 'abcde', 'é🎉z') for a in (0, 1, -2) for b in (0, 2, 99)]",
            // the same index through the object path, where the container's type
            // is only known at runtime — a `str` subclass must not take a fast
            // path guarded on the exact type
            "[m.obj_at(o, 1) for o in ('abc', [1, 2, 3], (1, 2, 3), {1: 'x'})]",
            "[m.obj_at(o, -1) for o in ('abc', [1, 2, 3])]",
            "[m.obj_at(type('S', (str,), {'__getitem__': lambda self, i: i * 10})('ab'), i) for i in (0, 1, 9)]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.obj_at, 'abc', 5), _capture(m.obj_at, 'abc', 'k')]]",
        ],
    );
}

/// `s[i] == "c"` never builds the character, so every way the character could
/// have differed from its code point has to be asked of the fused form directly
#[test]
fn comparing_a_character_against_a_one_character_literal_agrees() {
    agree(
        "charcmp",
        "\
def is_space(s: str, i: int) -> bool:
    return s[i] == \" \"

def not_space(s: str, i: int) -> bool:
    return s[i] != \" \"

# the literal on the left asks the mirrored question of every operator
def order(s: str, i: int) -> object:
    return (s[i] == \"m\", \"m\" == s[i], s[i] < \"m\", \"m\" < s[i], s[i] <= \"m\", s[i] > \"m\", s[i] >= \"m\")

# one code point, whatever anyone counting bytes or utf-16 units would say
def is_party(s: str, i: int) -> bool:
    return s[i] == \"🎉\"

def is_e_acute(s: str, i: int) -> bool:
    return s[i] == \"é\"

# two code points, so there is no single code point to compare against and the
# character has to be built after all
def is_combined(s: str, i: int) -> bool:
    return s[i] == \"e\\u0301\"

def is_empty(s: str, i: int) -> bool:
    return s[i] == \"\"

# the character outlives the comparison, so the object is wanted for its own sake
def held(s: str, i: int) -> object:
    c = s[i]
    return (c == \" \", c)
",
        &[
            "[m.is_space(' a ', i) for i in (0, 1, 2, -1, -3)]",
            "[m.not_space(' a ', i) for i in (0, 1, -1)]",
            // the index is still an index: out of range on either side, and on a
            // text with nothing in it at all
            "[(type(e).__name__, str(e)) for e in [_capture(m.is_space, 'ab', 2), _capture(m.is_space, 'ab', -3), _capture(m.is_space, '', 0)]]",
            // every operator against a character below, equal to and above the
            // literal, and against ones a byte-wise or utf-16 comparison would
            // order differently
            "[m.order(c, 0) for c in ('a', 'm', 'z', 'é', 'ħ', '\\u0100', '🎉', '\\uffff')]",
            "[m.is_party(s, 0) for s in ('🎉', '🎈', 'a', '\\U0001f38a')]",
            "[m.is_e_acute(s, 0) for s in ('é', 'e', 'è', '\\u0301')]",
            // a precomposed character is not the combining pair, and neither is
            // either half of it
            "[m.is_combined(s, 0) for s in ('é', 'e', 'e\\u0301')]",
            "[m.is_empty(s, 0) for s in ('a', ' ')]",
            "[m.held(' a', i) for i in (0, 1)]",
            // an annotation of `str` admits a subclass, whose `__getitem__` may
            // hand back a text of no code points or of several — neither of which
            // has a code point of its own to be compared
            "[m.is_space(type('S', (str,), {'__getitem__': lambda s, i: ' '})('ab'), i) for i in (0, 9)]",
            "[m.is_space(type('S', (str,), {'__getitem__': lambda s, i: '  '})('ab'), 0)]",
            "[m.is_space(type('S', (str,), {'__getitem__': lambda s, i: ''})('ab'), 0)]",
            "[m.order(type('S', (str,), {'__getitem__': lambda s, i: 'zz'})('ab'), 0)]",
            // and whose result may have said its own thing about `==` and about
            // order, which a comparison of code points would never ask
            "[m.order(type('S', (str,), {'__getitem__': lambda s, i: type('C', (str,), {'__eq__': lambda a, b: True, '__lt__': lambda a, b: True, '__hash__': str.__hash__})('q')})('ab'), 0)]",
            // a plain subclass inherits str's indexing and comparisons, and must
            // still get them
            "[m.is_space(type('P', (str,), {})(' a'), i) for i in (0, 1)]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.is_space, type('P', (str,), {})('ab'), 5)]]",
        ],
    );
}

#[test]
fn string_building_agrees() {
    agree(
        "strbuild",
        "\
def build(n: int) -> str:
    out = \"\"
    i = 0
    while i < n:
        out = out + \"word\" + str(i % 10) + \" \"
        i = i + 1
    return out

# the accumulator starts as the caller's own string, so growing it in place would
# change what the caller still holds
def grow(seed: str, n: int) -> object:
    out = seed
    i = 0
    while i < n:
        out = out + seed
        i = i + 1
    return (out, seed, len(out))

# a literal is shared with every other use of it in the program
def from_literal(n: int) -> object:
    out = \"abc\"
    i = 0
    while i < n:
        out = out + \"d\"
        i = i + 1
    return (out, \"abc\", \"abc\" == \"abc\")

def doubled(s: str) -> str:
    return s + s

# the one shape where a sole owner is not licence to grow in place: the resize
# would move the buffer out from under the copy reading it
def doubled_temp(a: str, b: str) -> str:
    t = a + b
    return t + t

def aliased(s: str) -> object:
    t = s
    u = t + t
    return (u, t, s)

# every operand is read again afterwards, so none of them may be taken over
def kept(a: str, b: str, c: str) -> object:
    return (a + b + c, a, b, c)

def subclass_left(a: str, b: str) -> object:
    return (a + b, type(a + b).__name__, a)
",
        &[
            "[m.build(n) for n in (0, 1, 3, 40)]",
            "[m.grow(s, n) for s in ('', 'x', 'ab', 'é🎉') for n in (0, 1, 5)]",
            "[m.from_literal(n) for n in (0, 1, 4)]",
            "[m.doubled(s) for s in ('', 'a', 'ab', '🎉')]",
            "[m.doubled_temp(a, b) for a in ('', 'ab', 'é') for b in ('', 'cd', '🎉')]",
            "[m.aliased(s) for s in ('', 'a', 'ab')]",
            "[m.kept('a', 'b', 'c'), m.kept('', '', ''), m.kept('é', '🎉', 'z')]",
            "m.subclass_left(type('S', (str,), {})('ab'), 'cd')",
        ],
    );
}

#[test]
fn bytes_literals_agree() {
    agree(
        "byteslit",
        "\
def raw() -> bytes:
    return b\"abc\"

def escaped() -> bytes:
    return b\"\\x00\\xff\\n\\t\\\"\\\\\"

# an escaped byte followed by a digit: C reads at most three octal digits, so the
# trailing `1` has to survive as a byte of its own
def octal_edge() -> bytes:
    return b\"\\x011\\x0079\"

def joined() -> bytes:
    return b\"ab\" b\"cd\"

def empty() -> bytes:
    return b\"\"

def size(b: bytes) -> int:
    return len(b)

def indexed(b: bytes, i: int) -> int:
    return b[i]

def concatenated(b: bytes) -> bytes:
    return b + b\"!\"

def defaulted(b: bytes = b\"zz\") -> bytes:
    return b

def decoded() -> str:
    return b\"calf\\xc3\\xa9\".decode(\"utf-8\")
",
        &[
            "m.raw()",
            "m.escaped()",
            "m.octal_edge()",
            "m.joined()",
            "m.empty()",
            "[m.size(b) for b in (b'', b'abc', b'\\x00\\x00')]",
            "[m.indexed(b'abc', i) for i in (0, 1, 2)]",
            "m.concatenated(b'q')",
            "m.defaulted()",
            "m.defaulted(b'given')",
            "m.decoded()",
            // the literal is a module static, so a thousand reads must hand back the
            // same object rather than a thousand of them
            "_repeated(lambda: m.raw(), 1000)",
        ],
    );
}

/// the source of [`string_literals_containing_a_nul_agree`], built once so the
/// compiled leg can be asked *which* build answered it
const NUL_LITERALS: &str = "\
def equals_prefix() -> bool:
    return \"a\\x00b\" == \"a\"

def whole() -> str:
    return \"a\\x00b\"

def length() -> int:
    return len(\"a\\x00b\")

# at the end, where a C string has nothing left to stop short of
def trailing() -> str:
    return \"ab\\x00\"

def trailing_length() -> int:
    return len(\"ab\\x00\")

# and alone, which the C form cannot tell from the empty string at all
def only_nul() -> str:
    return \"\\x00\"

def only_nul_length() -> int:
    return len(\"\\x00\")

# the emitter writes utf-8 and the constructor decodes it, so a byte count and a
# code point count have to stay apart
def mixed() -> str:
    return \"\\u00e9\\x00\\U0001f389e\\u0301\"

def mixed_length() -> int:
    return len(\"\\u00e9\\x00\\U0001f389e\\u0301\")

def concatenated() -> str:
    return \"a\\x00\" + \"b\\x00c\"

# as a key, where a truncated literal collides with the very string it truncates to
def keyed(table: dict[str, int]) -> int:
    return table[\"a\\x00b\"] * 10 + table[\"a\"]

def held(table: dict[str, int]) -> bool:
    return \"a\\x00b\" in table
";

#[test]
fn string_literals_containing_a_nul_agree() {
    // built from the C string alone, every use of `\"a\0b\"` was `\"a\"`: it compared
    // equal to its own truncation, measured 1, and answered another key's value
    agree(
        "nulstr",
        NUL_LITERALS,
        &[
            "m.equals_prefix()",
            "m.whole()",
            "m.length()",
            "m.trailing()",
            "m.trailing_length()",
            "m.only_nul()",
            "m.only_nul_length()",
            "m.mixed()",
            "m.mixed_length()",
            "m.concatenated()",
            "m.keyed({'a\\x00b': 7, 'a': 3})",
            "m.held({'a\\x00b': 7})",
            "m.held({'a': 3})",
            // the literal is a module static shared by every read of it
            "_repeated(lambda: m.whole(), 1000)",
        ],
    );
}

#[test]
fn a_literal_that_could_close_a_c_comment_agrees() {
    // the literal names itself in a comment beside its static, and `*/` closed that
    // comment early — the C compiler then rejected the whole module
    agree(
        "commentlit",
        "\
def ender() -> str:
    return \"a */ b /* c\"

def raw_ender() -> bytes:
    return b\"*/\"
",
        &["m.ender()", "m.raw_ender()"],
    );
}

#[test]
fn the_compiled_build_is_the_one_that_answers_for_a_nul_literal() {
    // `agree` cannot say which build answered — a declined function answers the same.
    // `builtin_function_or_method` is what says the compiled leg is under these calls
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_nulstr_which");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        NUL_LITERALS,
        "by_diff_nulstr_which",
        &toolchain,
        &dir,
        &Options::default(),
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "{:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_nulstr_which as m\n\
         print(type(m.whole).__name__, m.length(), m.equals_prefix(), ascii(m.whole()))\n",
    );
    assert_eq!(out, "builtin_function_or_method 3 False 'a\\x00b'");
}

#[test]
fn calls_out_of_the_unit_agree() {
    agree(
        "callout",
        "\
def magnitude(n: int) -> int:
    return abs(n)

def as_text(n: int) -> object:
    return str(n)

def biggest(xs: list[int]) -> object:
    return max(xs)

def ordered(xs: list[int]) -> object:
    return sorted(xs)

def rounded(x: float) -> object:
    return round(x)
",
        &[
            "[m.magnitude(n) for n in (0, 5, -5, -(10 ** 30))]",
            "[m.as_text(n) for n in (0, -1, 10 ** 25)]",
            "m.biggest([3, 1, 2])",
            "m.ordered([3, 1, 2])",
            "[m.rounded(x) for x in (1.4, 1.6, -1.5, 2.5)]",
        ],
    );
}

#[test]
fn a_call_out_checks_what_comes_back() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_callcheck");
    let _ = std::fs::remove_dir_all(&dir);
    // `abs` is a builtin the checker knows returns an int, so the call site
    // narrows with a checked unbox. shadowing it in the module namespace — which
    // `By_LookupGlobal` consults first, exactly as `LOAD_GLOBAL` does — makes it
    // return the wrong type, and the check has to catch that rather than
    // reinterpret the bits
    let source = "def magnitude(n: int) -> int:\n    return abs(n)\n";
    if build_source(
        source,
        "by_diff_callcheck",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import by_diff_callcheck as m\n\
         assert m.magnitude(-3) == 3\n\
         m.__dict__['abs'] = lambda x: 'not an int'\n\
         try:\n    m.magnitude(-3)\n\
         except TypeError as e:\n    print('TypeError:', e)\n\
         else:\n    print('no error')\n",
    );
    assert_eq!(out, "TypeError: expected int, got str");
}

#[test]
fn a_missing_global_raises_a_name_error() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_nameerr");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "def f() -> object:\n    return nowhere()\n";
    if build_source(
        source,
        "by_diff_nameerr",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // the message as well as the class: the name reaches the error as an object
    // rather than as the C string it was written as, and a format that dropped it
    // would still raise the right class
    let out = run(
        &python,
        &dir,
        "import by_diff_nameerr as m\n\
         try:\n    m.f()\n\
         except NameError as e:\n    print('NameError:', e)\n\
         else:\n    print('no error')\n",
    );
    assert_eq!(out, "NameError: name 'nowhere' is not defined");
}

/// a global read resolves the *name* every time, never the value it last found
///
/// the name is interned once per call site, which is only sound because that is all
/// that is kept: one call site, run twice, with the binding moved underneath it
#[test]
fn a_rebound_global_is_seen_on_every_read() {
    agree_python(
        "globalrebind",
        "\
def twice(flip: object) -> object:
    out = []
    i = 0
    while i < 2:
        out.append(picked())
        flip()
        i = i + 1
    return out
",
        &[
            // a name that arrives in the module namespace takes precedence from
            // the next read on, though builtins answered the one before
            "(setattr(__import__('builtins'), 'picked', lambda: 'builtins'), \
             m.twice(lambda: m.__dict__.__setitem__('picked', lambda: 'module')))[1]",
            // and one that leaves it falls back rather than staying resolved
            "(setattr(__import__('builtins'), 'picked', lambda: 'builtins'), \
             m.__dict__.__setitem__('picked', lambda: 'module'), \
             m.twice(lambda: m.__dict__.pop('picked', None)))[2]",
        ],
    );
}

#[test]
fn iteration_agrees() {
    agree(
        "iterate",
        "\
def total(xs: list[int]) -> int:
    acc = 0
    for x in xs:
        acc = acc + x
    return acc

def count(o) -> int:
    n = 0
    for _item in o:
        n = n + 1
    return n

def first_big(xs: list[int]) -> int:
    for x in xs:
        if x > 10:
            return x
    else:
        return -1
    return -2

def joined(parts: list[str]) -> str:
    out = \"\"
    for p in parts:
        out = out + p
    return out
",
        &[
            "[m.total(xs) for xs in ([], [1], [1, 2, 3], [10 ** 20, 1])]",
            "[m.count(o) for o in ([], [1, 2], 'abc', (1, 2, 3), {1: 2, 3: 4}, range(5))]",
            "[m.first_big(xs) for xs in ([], [1, 2], [1, 20, 30])]",
            "[m.joined(p) for p in ([], ['a'], ['a', 'b', 'c'])]",
        ],
    );
}

#[test]
fn iterating_a_wrongly_typed_list_raises() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_iterchk");
    let _ = std::fs::remove_dir_all(&dir);
    // the annotation says int elements; the unbox per element is the
    // `iterations` soundness position and must catch a lie
    let source = "\
def total(xs: list[int]) -> int:
    acc = 0
    for x in xs:
        acc = acc + x
    return acc
";
    if build_source(
        source,
        "by_diff_iterchk",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import by_diff_iterchk as m\n\
         try:\n    m.total([1, 'x'])\n\
         except TypeError as e:\n    print('TypeError:', e)\n\
         else:\n    print('no error')\n",
    );
    assert_eq!(out, "TypeError: expected int, got str");
}

#[test]
fn loop_else_agrees() {
    agree(
        "loopelse",
        "\
def search(n: int, target: int) -> int:
    for i in range(n):
        if i == target:
            break
    else:
        return -1
    return 0

def whileelse(n: int) -> int:
    i = 0
    while i < n:
        if i == 3:
            break
        i = i + 1
    else:
        return -1
    return i
",
        &[
            "[m.search(5, t) for t in (0, 3, 9)]",
            "[m.whileelse(n) for n in (0, 2, 5, 10)]",
        ],
    );
}

#[test]
fn method_calls_and_attributes_agree() {
    agree(
        "methods",
        "\
def collect(words: list[str]) -> object:
    out = []
    for w in words:
        out.append(len(w))
    return out

def trimmed(s: str) -> str:
    return s.strip()

def upper_join(parts: list[str]) -> str:
    acc = \"\"
    for p in parts:
        acc = acc + p.upper()
    return acc

def numerator(f) -> object:
    return f.numerator
",
        &[
            "m.collect(['a', 'bb', 'ccc'])",
            "m.collect([])",
            "[m.trimmed(s) for s in ('  x  ', 'x', '   ')]",
            "[m.upper_join(p) for p in ([], ['a'], ['ab', 'cd'])]",
            "[m.numerator(n) for n in (3, -7)]",
        ],
    );
}

#[test]
fn a_missing_attribute_raises() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_attrerr");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "def f(o) -> object:\n    return o.nope\n";
    if build_source(
        source,
        "by_diff_attrerr",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import by_diff_attrerr as m\n\
         try:\n    m.f(1)\n\
         except AttributeError:\n    print('AttributeError')\n\
         else:\n    print('no error')\n",
    );
    assert_eq!(out, "AttributeError");
}

#[test]
fn list_displays_agree_and_do_not_leak() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_listbuild");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def pair(a, b) -> object:
    return [a, b]

def empty() -> object:
    return []
";
    if build_source(
        source,
        "by_diff_listbuild",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // the elements are owned references the list steals; getting that wrong is
    // either a leak or a use-after-free
    let out = run(
        &python,
        &dir,
        "import gc, by_diff_listbuild as m\n\
         assert m.pair(1, 'x') == [1, 'x'] and m.empty() == []\n\
         for _ in range(50): m.pair([1], [2])\n\
         gc.collect(); before = len(gc.get_objects())\n\
         for _ in range(2000): m.pair([1], [2])\n\
         gc.collect(); after = len(gc.get_objects())\n\
         print('stable' if after <= before + 100 else f'grew {before}->{after}')\n",
    );
    assert_eq!(out, "stable");
}

#[test]
fn f_strings_agree() {
    agree(
        "fstrings",
        "\
def plain(name: str, n: int) -> str:
    return f\"{name} has {n}\"

def spec(n: int) -> str:
    return f\"{n:04d}|{n:+}|{n:x}\"

def conversions(o) -> str:
    return f\"{o!s} {o!r} {o!a}\"

def empty() -> str:
    return f\"\"

def braces(n: int) -> str:
    return f\"{{{n}}}\"
",
        &[
            "m.plain('x', 3)",
            "[m.spec(n) for n in (0, 7, -7, 255)]",
            "[m.conversions(o) for o in ('a', 1, [1], None, 'é')]",
            "m.empty()",
            "m.braces(5)",
        ],
    );
}

#[test]
fn subscripting_agrees() {
    agree(
        "subscript",
        "\
def head(xs: list[int]) -> int:
    return xs[0]

def at(xs: list[int], i: int) -> int:
    return xs[i]

def put(xs: list[int], i: int, v: int) -> object:
    xs[i] = v
    return xs

def lookup(d: dict[str, int], k: str) -> int:
    return d[k]
",
        &[
            "m.head([5, 6])",
            "[m.at([1, 2, 3], i) for i in (0, 1, 2, -1)]",
            "m.put([1, 2], 0, 9)",
            "m.lookup({'a': 1, 'b': 2}, 'b')",
            "[(type(e).__name__) for e in [_capture(m.at, [1], 5)]]",
            "[(type(e).__name__) for e in [_capture(m.lookup, {}, 'z')]]",
        ],
    );
}

#[test]
fn displays_agree() {
    agree(
        "displays",
        "\
def mixed(n: int) -> object:
    return [n, n + 1]

def a_set(n: int) -> object:
    return {n, n + 1, n}

def a_tuple(n: int) -> object:
    return (n, n + 1)

def a_dict(n: int) -> object:
    return {\"n\": n, \"twice\": n * 2}

def nested(n: int) -> object:
    return [{n: [n]}, (n,), {n}]
",
        &[
            "m.mixed(1)",
            "sorted(m.a_set(1))",
            "m.a_tuple(1)",
            "m.a_dict(1)",
            "m.nested(1)",
            "[type(m.a_set(1)).__name__, type(m.a_tuple(1)).__name__, type(m.a_dict(1)).__name__]",
        ],
    );
}

#[test]
fn folding_does_not_change_behaviour() {
    agree(
        "folded",
        "\
def constants() -> int:
    return 2 + 3 * 4

def always() -> int:
    if 1 < 2:
        return 1
    return 0

def divides_by_zero() -> int:
    return 1 // 0
",
        &[
            "m.constants()",
            "m.always()",
            // folding must not have evaluated this at compile time
            "[(type(e).__name__) for e in [_capture(m.divides_by_zero)]]",
        ],
    );
}

#[test]
fn try_except_agrees() {
    agree(
        "tryexcept",
        "\
def safe_div(a: int, b: int) -> int:
    try:
        return a // b
    except ZeroDivisionError:
        return 0

def classify(xs: list[int], i: int) -> str:
    try:
        v = xs[i]
    except IndexError:
        return \"out of range\"
    except TypeError:
        return \"bad type\"
    else:
        return \"ok\"

def with_finally(a: int, b: int) -> object:
    trail = []
    try:
        trail.append(a // b)
    except ZeroDivisionError:
        trail.append(-1)
    finally:
        trail.append(99)
    return trail

def named(a: int, b: int) -> str:
    try:
        return str(a // b)
    except ZeroDivisionError as e:
        return str(e)

def catch_all(a: int, b: int) -> str:
    try:
        return str(a // b)
    except:
        return \"caught\"

def unmatched(a: int, b: int) -> int:
    try:
        return a // b
    except IndexError:
        return -1
",
        &[
            "[m.safe_div(7, 2), m.safe_div(1, 0), m.safe_div(-7, 2)]",
            "[m.classify([1], 0), m.classify([1], 5)]",
            "[m.with_finally(6, 3), m.with_finally(1, 0)]",
            "[m.named(6, 3), m.named(1, 0)]",
            "[m.catch_all(6, 3), m.catch_all(1, 0)]",
            "m.unmatched(6, 3)",
            // an unmatched handler must let the exception continue, with `finally`
            // having run
            "[(type(e).__name__) for e in [_capture(m.unmatched, 1, 0)]]",
        ],
    );
}

/// the operation that would have rebound a name raised, so the name is still bound
/// to what it was — and the handler in the same function goes on to read it
///
/// releasing and storing before the failure test left that read a `NULL`, which is
/// a `SystemError` where it is returned and a **segfault** where it is handed to
/// cpython as an argument. `absorb` is reached through a python function, which is
/// the frame that increfs it
#[test]
fn a_failing_operation_leaves_its_target_bound() {
    agree(
        "keptbind",
        "\
def kept(table: dict[str, object], key: str) -> object:
    held: object = \"before\"
    try:
        held = table[key]
    except KeyError:
        pass
    return held

def handed_on(table: dict[str, object], key: str, sink: object) -> object:
    held: object = \"before\"
    try:
        held = table[key]
    except KeyError:
        pass
    return sink.absorb(held)

def counted(table: dict[str, object], key: str) -> object:
    n = 1
    try:
        n = table[key]
    except KeyError:
        pass
    return n
",
        &[
            // the crashing one first: a `NULL` handed to cpython kills the process,
            // and the leg that only *returns* one raises instead
            "m.handed_on({'a': 'b'}, 'zz', _Sink())",
            "m.handed_on({'a': 'b'}, 'a', _Sink())",
            "[m.kept({'a': 'b'}, 'a'), m.kept({'a': 'b'}, 'zz')]",
            "m.counted({'a': 3}, 'zz')",
        ],
    );
}

/// `_not_given = _not_given()` is the stdlib's own singleton idiom, and module init
/// installing the native class over the instance it left there is a wrong answer the
/// consumer only meets much later — `enum`'s `EnumType.__call__` compares against it
/// by identity, so every by-value lookup fails
#[test]
fn a_class_the_module_rebinds_keeps_what_the_rebind_produced() {
    agree_python_with_declines(
        "rebound",
        "\
class Marker:
    def __repr__(self) -> str:
        return '<marker>'

Marker = Marker()

def describe() -> str:
    return repr(Marker)
",
        &[
            "repr(m.Marker)",
            "m.describe()",
            "type(m.Marker).__name__",
            "isinstance(m.Marker, type)",
        ],
    );
}

#[test]
fn a_rebound_global_is_observed() {
    // resolving a global per call rather than caching it: early binding is a
    // tier-3 assumption, and at the default tier python would see the rebind
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_rebind");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "def size(o) -> object:\n    return helper(o)\n";
    if build_source(
        source,
        "by_diff_rebind",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import by_diff_rebind as m\n\
         m.__dict__['helper'] = lambda o: 1\n\
         first = m.size(0)\n\
         m.__dict__['helper'] = lambda o: 2\n\
         print(first, m.size(0))\n",
    );
    assert_eq!(out, "1 2");
}

#[test]
fn comprehensions_agree() {
    agree(
        "comprehensions",
        "\
def squares(n: int) -> object:
    return [x * x for x in range(n)]

def evens(n: int) -> object:
    return [x for x in range(n) if x % 2 == 0]

def guarded(n: int) -> object:
    return [x for x in range(n) if x % 2 == 0 if x > 2]

def lookup(words: list[str]) -> object:
    return {w: len(w) for w in words}

def uniq(xs: list[int]) -> object:
    return {x % 3 for x in xs}

def over_a_list(xs: list[int]) -> object:
    return [x + 1 for x in xs]
",
        &[
            "[m.squares(n) for n in (0, 1, 5)]",
            "m.evens(7)",
            "m.guarded(10)",
            "m.lookup(['a', 'bb', 'ccc'])",
            "sorted(m.uniq([1, 2, 3, 4, 5]))",
            "m.over_a_list([1, 2, 3])",
            "[type(m.squares(1)).__name__, type(m.uniq([1])).__name__, type(m.lookup([])).__name__]",
        ],
    );
}

#[test]
fn decorators_agree() {
    agree_with_declines(
        "decorators",
        "\
def twice(f) -> object:
    def wrapper(n: int) -> int:
        return f(n) * 2
    return wrapper

def tag(f) -> object:
    def wrapper(n: int) -> int:
        return f(n) + 100
    return wrapper

@twice
def plus_one(n: int) -> int:
    return n + 1

@twice
@tag
def stacked(n: int) -> int:
    return n
",
        &[
            "m.plus_one(5)",
            // the outermost decorator is applied last, so `twice` wraps `tag`
            "m.stacked(1)",
        ],
    );
}

#[test]
fn string_literals_do_not_leak() {
    // `gc.get_objects()` cannot see this: `str` is not GC-tracked, so a leaked
    // literal is invisible to an object-count check. the refcount of the returned
    // literal is the measurement that works
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_strleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def classify(n: int) -> str:
    scratch = \"x\" + \"y\"
    if n < 0:
        return \"neg\"
    return scratch
";
    if build_source(
        source,
        "by_diff_strleak",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import sys, by_diff_strleak as m\n\
         before = sys.getrefcount(m.classify(-1))\n\
         for _ in range(20000):\n    m.classify(-1)\n    m.classify(1)\n\
         after = sys.getrefcount(m.classify(-1))\n\
         print('stable' if after <= before + 2 else f'leaked {before}->{after}')\n",
    );
    assert_eq!(out, "stable");
}

#[test]
fn a_concatenated_operand_keeps_the_callers_reference() {
    // an operand handed over to a growing concatenation is a reference the frame
    // did not own — the caller's count goes down and the object is freed under it.
    // the two builds return the same string either way, so only the count says so
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_strhold");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def join(a: str, b: str) -> str:
    return a + b

def grow(seed: str, n: int) -> str:
    out = seed
    i = 0
    while i < n:
        out = out + \"x\"
        i = i + 1
    return out
";
    if build_source(
        source,
        "by_diff_strhold",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import sys, by_diff_strhold as m\n\
         held = 'a' * 40\n\
         before = sys.getrefcount(held)\n\
         for _ in range(20000):\n    m.join(held, 'y')\n    m.grow(held, 3)\n\
         after = sys.getrefcount(held)\n\
         print('stable' if after == before else f'moved {before}->{after}')\n",
    );
    assert_eq!(out, "stable");
}

#[test]
fn native_classes_agree() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    agree(
        "classes",
        "\
data class Point:
    x: int
    y: int

    def total(self) -> int:
        return self.x + self.y

    def scaled(self, k: int) -> int:
        return self.total() * k

data class Named:
    label: str
    count: int

    def shout(self) -> str:
        return self.label + \"!\"

def make(a: int, b: int) -> object:
    return Point(a, b)
",
        &[
            "m.Point(3, 4).total()",
            "m.Point(3, 4).scaled(10)",
            "[m.Point(a, b).total() for a in (0, -1, 10 ** 20) for b in (0, 5)]",
            "m.make(2, 3).total()",
            "(m.Point(1, 2).x, m.Point(1, 2).y)",
            "m.Named('hi', 1).shout()",
            "m.Named('hi', 1).label",
            // the argument *count* is checked in both builds
            "[(type(e).__name__) for e in [_capture(m.Point, 1)]]",
        ],
    );
}

#[test]
fn a_native_constructor_checks_its_argument_types() {
    // a documented delta, not an equality: a compiled field is an unboxed
    // `ByTagged`, so a `str` cannot be stored there and the check is mandatory.
    // `@dataclass` does not enforce annotations at runtime, so the interpreted
    // twin accepts it — the same difference `--soundness all` would close
    let Some((python, toolchain)) = environment() else {
        return;
    };
    if !supports(&toolchain, (3, 10)) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    let dir = std::env::temp_dir().join("by_diff_ctorcheck");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "data class Point:\n    x: int\n    y: int\n";
    if build_source(
        source,
        "by_diff_ctorcheck",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import by_diff_ctorcheck as m\n\
         try:\n    m.Point('a', 1)\n\
         except TypeError as e:\n    print('TypeError:', e)\n\
         else:\n    print('accepted a str')\n",
    );
    assert_eq!(out, "TypeError: expected int, got str");
}

#[test]
fn a_native_class_has_a_fixed_layout() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    if !supports(&toolchain, (3, 10)) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    let dir = std::env::temp_dir().join("by_diff_layout");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
data class Point:
    x: int
    y: int
";
    if build_source(
        source,
        "by_diff_layout",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // no `__dict__`, and no attribute outside the declared set
    let out = run(
        &python,
        &dir,
        "import by_diff_layout as m\n\
         p = m.Point(1, 2)\n\
         print(hasattr(p, '__dict__'))\n\
         try:\n    p.extra = 1\n\
         except AttributeError:\n    print('AttributeError')\n\
         else:\n    print('accepted an undeclared attribute')\n",
    );
    assert_eq!(out, "False\nAttributeError");
}

#[test]
fn a_native_class_instance_does_not_leak() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    if !supports(&toolchain, (3, 10)) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    let dir = std::env::temp_dir().join("by_diff_classleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
data class Holder:
    label: str
    n: int
";
    if build_source(
        source,
        "by_diff_classleak",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // `tp_dealloc` has to release each refcounted field
    let out = run(
        &python,
        &dir,
        "import sys, by_diff_classleak as m\n\
         label = 'x' * 40\n\
         before = sys.getrefcount(label)\n\
         for _ in range(20000):\n    m.Holder(label, 1)\n\
         after = sys.getrefcount(label)\n\
         print('stable' if after <= before + 2 else f'leaked {before}->{after}')\n",
    );
    assert_eq!(out, "stable");
}

#[test]
fn a_declined_function_still_exists_and_behaves_the_same() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 11))) {
        return;
    }
    // the interpreted fallback is what makes coverage total: a construct with no
    // native lowering costs speed in that one place and nothing anywhere else
    agree_with_declines(
        "fallback",
        "\
def fast(a: int) -> int:
    return a * 2

def slow(n: int) -> int:
    out = n
    try:
        pass
    except* ValueError:
        out = 0
    return out
",
        &[
            "m.fast(21)",
            "m.slow(21)",
            // the declined one is a plain python function in both builds
            "type(m.slow).__name__",
        ],
    );
}

#[test]
fn a_float_module_imports_without_any_extra_runtime_module() {
    // `float` transpiles through `JustFloat`, which the lazy-import pass binds
    // locally. transpiling with `lazy_imports` off instead emits
    // `from ty_extensions import JustFloat`, and the extension then fails to
    // import at all — the embedded fallback runs at module init
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_floatimport");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "def area(r: float) -> float:\n    return 3.0 * r * r\n";
    if build_source(
        source,
        "by_diff_floatimport",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // nothing but the extension itself is on the path
    let out = run(
        &python,
        &dir,
        "import by_diff_floatimport as m\nprint(m.area(2.0))\n",
    );
    assert_eq!(out, "12.0");
}

#[test]
fn a_compiled_function_is_a_c_function_object() {
    // one of the few places the two builds are *meant* to differ: a natively
    // compiled function is a builtin, not a python function. recorded in
    // plan.md#semantic-deltas rather than asserted as equal
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_cfunc");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "def f(a: int) -> int:\n    return a\n";
    if build_source(
        source,
        "by_diff_cfunc",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import by_diff_cfunc as m\nprint(type(m.f).__name__)\n",
    );
    assert_eq!(out, "builtin_function_or_method");
}

#[test]
fn module_level_code_runs_at_import() {
    agree(
        "modulelevel",
        "\
LIMIT = 7

def under(n: int) -> int:
    return n

def limit() -> int:
    return 7
",
        &["m.LIMIT", "m.limit()", "m.under(3)"],
    );
}

#[test]
fn no_any_turns_a_gradual_decline_into_an_error() {
    let Some((_, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_noany");
    let _ = std::fs::remove_dir_all(&dir);

    let source = "\
def precise(a: int) -> int:
    return a + 1

def loose(a) -> None:
    pass
";
    // by default a gradual parameter compiles — it lands on `object`
    let built = build_source(
        source,
        "by_diff_noany",
        &toolchain,
        &dir,
        &Options::default(),
    );
    match built {
        Ok(built) => assert!(built.declined.is_empty(), "{:?}", built.declined),
        Err(error) => {
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    }

    // `--no-any` refuses instead, and names the function
    let error = build_source(
        source,
        "by_diff_noany",
        &toolchain,
        &dir,
        &Options {
            no_any: true,
            ..Options::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("no-any"), "{error}");
    assert!(error.contains("loose"), "{error}");
    assert!(error.contains("`a` is gradual"), "{error}");
    // a fully typed function is not blamed
    assert!(!error.contains("precise"), "{error}");
}

#[test]
fn require_native_rejects_any_decline_at_all() {
    // a different question from `--no-any`: `list[int]` is not gradual, it is a
    // type the compiler does not represent *yet*, so only this flag catches it
    let Some((_, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_reqnative");
    let _ = std::fs::remove_dir_all(&dir);
    // precisely typed and still declines: `except*` has no lowering
    let source = "\
def precise(a: int) -> int:
    return a + 1

def grouped(a: int) -> int:
    out = a
    try:
        pass
    except* ValueError:
        out = 0
    return out
";
    // `--no-any` is satisfied: nothing here is gradual
    match build_source(
        source,
        "by_diff_reqnative",
        &toolchain,
        &dir,
        &Options {
            no_any: true,
            ..Options::default()
        },
    ) {
        Ok(built) => assert_eq!(built.declined.len(), 1, "{:?}", built.declined),
        Err(error) => {
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    }

    // `--require-native` is not
    let error = build_source(
        source,
        "by_diff_reqnative",
        &toolchain,
        &dir,
        &Options {
            require_native: true,
            ..Options::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("require-native"), "{error}");
    assert!(error.contains("`except*`"), "{error}");
}

#[test]
fn no_any_accepts_a_fully_typed_module() {
    let Some((_, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_noany_ok");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "def f(a: int, b: int) -> int:\n    return a * b\n";
    match build_source(
        source,
        "by_diff_noany_ok",
        &toolchain,
        &dir,
        &Options {
            no_any: true,
            ..Options::default()
        },
    ) {
        Ok(built) => assert!(built.declined.is_empty()),
        Err(error) => eprintln!("skipping: no working C toolchain ({error})"),
    }
}

#[test]
fn optimized_output_still_agrees_with_the_interpreter() {
    // copy propagation and infallibility both rewrite the IR — the differential
    // legs are what say the rewrites preserved the program
    agree(
        "optimized",
        "\
def area(r: float) -> float:
    scaled = r * r
    return 3.5 * scaled

def compose(a: float, b: float) -> float:
    x = area(a)
    y = area(b)
    return x + y
",
        &[
            "m.area(2.0)",
            "m.compose(1.0, 2.0)",
            "[m.compose(a, a) for a in (0.0, -1.5, 3.25)]",
        ],
    );
}

#[test]
fn a_declined_function_is_reported_with_a_reason() {
    let Some((_, toolchain)) = environment() else {
        return;
    };
    let dir: PathBuf = std::env::temp_dir().join("by_diff_declined");
    let _ = std::fs::remove_dir_all(&dir);

    let source = "\
def fast(a: int) -> int:
    return a + 1

def slow(a: int) -> None:
    try:
        pass
    except* ValueError:
        pass
";
    let Ok(built) = build_source(
        source,
        "by_diff_declined",
        &toolchain,
        &dir,
        &Options::default(),
    ) else {
        eprintln!("skipping: no working C toolchain");
        return;
    };
    assert_eq!(built.declined.len(), 1);
    assert_eq!(built.declined[0].name, "slow");
    assert!(
        built.declined[0].reason.contains("`except*`"),
        "{:?}",
        built.declined[0]
    );
}

#[test]
fn field_access_agrees() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    agree(
        "fields",
        "\
data class Point:
    x: int
    y: int

    def total(self) -> int:
        return self.x + self.y

    def shift(self, d: int) -> int:
        self.x = self.x + d
        return self.x

data class Line:
    a: Point
    b: Point

    def span(self) -> int:
        return self.b.x - self.a.x

    def retarget(self, p: Point) -> int:
        self.b = p
        return self.span()

def sum_of(p: Point) -> int:
    return p.x + p.y

def bump_twice(p: Point) -> int:
    p.shift(1)
    p.shift(2)
    return p.x
",
        &[
            "m.Point(3, 4).total()",
            "m.sum_of(m.Point(3, 4))",
            "m.Point(3, 4).shift(10)",
            "m.bump_twice(m.Point(0, 0))",
            "m.Line(m.Point(1, 2), m.Point(10, 20)).span()",
            "m.Line(m.Point(1, 2), m.Point(10, 20)).a.y",
            "m.Line(m.Point(1, 2), m.Point(10, 20)).retarget(m.Point(100, 0))",
            // a write through the python-visible setter, then a native read
            "[(p := m.Point(1, 2), setattr(p, 'x', 9), p.total())[-1]]",
            "[m.sum_of(m.Point(a, b)) for a in (0, -1, 10 ** 20) for b in (0, 5)]",
        ],
    );
}

#[test]
fn a_field_setter_checks_its_value() {
    // the same documented delta as the constructor: an unboxed field cannot hold
    // the wrong representation, and `@dataclass` does not check assignments
    let Some((python, toolchain)) = environment() else {
        return;
    };
    if !supports(&toolchain, (3, 10)) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    let dir = std::env::temp_dir().join("by_diff_setcheck");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
data class Point:
    x: int
    y: int

frozen data class Fixed:
    n: int
";
    if build_source(
        source,
        "by_diff_setcheck",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import by_diff_setcheck as m\n\
         p = m.Point(1, 2)\n\
         p.x = 5\n\
         print(p.x)\n\
         for attempt in (lambda: setattr(p, 'x', 'a'), lambda: delattr(p, 'x'), lambda: setattr(m.Fixed(1), 'n', 2)):\n\
        \x20   try:\n        attempt()\n\
        \x20   except (TypeError, AttributeError) as e:\n        print(type(e).__name__)\n\
        \x20   else:\n        print('accepted')\n",
    );
    assert_eq!(out, "5\nTypeError\nAttributeError\nAttributeError");
}

#[test]
fn a_class_typed_argument_is_checked_at_the_boundary() {
    // a compiled parameter typed as a native class is a pointer to its struct,
    // so python handing over anything else has to be caught here — the
    // alternative is a wild pointer dereference
    let Some((python, toolchain)) = environment() else {
        return;
    };
    if !supports(&toolchain, (3, 10)) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    let dir = std::env::temp_dir().join("by_diff_argcheck");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
data class Point:
    x: int

def read(p: Point) -> int:
    return p.x
";
    if build_source(
        source,
        "by_diff_argcheck",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import by_diff_argcheck as m\n\
         print(m.read(m.Point(7)))\n\
         for bad in ('a', 1, None):\n\
        \x20   try:\n        m.read(bad)\n\
        \x20   except TypeError as e:\n        print(e)\n\
        \x20   else:\n        print('accepted')\n",
    );
    assert_eq!(
        out,
        "7\nexpected by_diff_argcheck.Point, got str\n\
         expected by_diff_argcheck.Point, got int\n\
         expected by_diff_argcheck.Point, got NoneType"
    );
}

#[test]
fn a_field_read_does_not_leak() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    if !supports(&toolchain, (3, 10)) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    let dir = std::env::temp_dir().join("by_diff_fieldleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
data class Holder:
    label: str

def label_of(h: Holder) -> str:
    return h.label

data class Nest:
    inner: Holder

def inner_label(n: Nest) -> str:
    return n.inner.label
";
    if build_source(
        source,
        "by_diff_fieldleak",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // a field read hands back a *borrowed* reference, so the register that takes
    // it must retain — and release. `str` is not gc-tracked, so this measures the
    // refcount rather than the object count
    let out = run(
        &python,
        &dir,
        "import sys, by_diff_fieldleak as m\n\
         label = 'x' * 40\n\
         h = m.Holder(label)\n\
         n = m.Nest(h)\n\
         before = sys.getrefcount(label)\n\
         for _ in range(20000):\n\
        \x20   m.label_of(h)\n\
        \x20   m.inner_label(n)\n\
         after = sys.getrefcount(label)\n\
         print('stable' if after == before else f'leaked {before}->{after}')\n",
    );
    assert_eq!(out, "stable");
}

#[test]
fn a_constructor_result_is_used_natively() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    agree(
        "ctornative",
        "\
data class Point:
    x: int
    y: int

    def total(self) -> int:
        return self.x + self.y

data class Line:
    a: Point
    b: Point

def diag(a: int) -> int:
    return Point(a, a).x

def build(a: int, b: int) -> int:
    return Line(Point(a, b), Point(b, a)).b.x

def widened(a: int) -> object:
    return Point(a, a)

def chained(a: int) -> int:
    p = Point(a, a + 1)
    q = Point(p.y, p.x)
    return q.total() + p.total()
",
        &[
            "m.diag(7)",
            "m.build(3, 9)",
            "m.widened(4).total()",
            "m.chained(5)",
            "[m.chained(a) for a in (0, -3, 10 ** 20)]",
        ],
    );
}

#[test]
fn a_declined_callee_does_not_break_the_build() {
    // the whole module used to fail to compile: the emitted call named a symbol
    // that was never defined
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    agree_with_declines(
        "declinechain",
        "\
def helper[T](a: T) -> T:
    return a

def caller(a: int) -> int:
    return helper(a) + a

data class Point:
    x: int

    def bad[T](self, a: T) -> int:
        return self.x

def read(p: Point) -> int:
    return p.x

def alone(a: int) -> int:
    return a + 1
",
        &[
            "m.helper(1)",
            "m.caller(2)",
            "m.read(m.Point(5))",
            "m.Point(5).bad(1)",
            "m.alone(3)",
        ],
    );
}

#[test]
fn direct_method_dispatch_agrees() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    agree(
        "direct",
        "\
data class Point:
    x: int
    y: int

    def total(self) -> int:
        return self.x + self.y

    def scaled(self, k: int) -> int:
        return self.total() * k

    def shifted(self, d: int) -> int:
        self.x = self.x + d
        return self.total()

    def label(self) -> str:
        return \"p\"

data class Line:
    a: Point
    b: Point

    def span(self) -> int:
        return self.b.total() - self.a.total()

    def widest(self, other: Line) -> int:
        mine = self.span()
        theirs = other.span()
        if mine > theirs:
            return mine
        return theirs

def drive(a: int, b: int) -> int:
    p = Point(a, b)
    return p.scaled(2) + p.total() + p.shifted(1)

def countdown(p: Point, n: int) -> int:
    total = 0
    for _ in range(n):
        total = total + p.total()
    return total
",
        &[
            "m.Point(3, 4).scaled(5)",
            "m.Point(3, 4).label()",
            "m.drive(1, 2)",
            "m.countdown(m.Point(2, 3), 4)",
            "m.Line(m.Point(1, 1), m.Point(10, 10)).span()",
            "m.Line(m.Point(1, 1), m.Point(10, 10)).widest(m.Line(m.Point(0, 0), m.Point(2, 2)))",
            "[m.drive(a, a) for a in (0, -7, 10 ** 20)]",
            // a python caller reaches the same method through the type object
            "[getattr(m.Point(3, 4), 'scaled')(2)]",
        ],
    );
}

#[test]
fn a_direct_method_call_does_not_leak() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    if !supports(&toolchain, (3, 10)) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    let dir = std::env::temp_dir().join("by_diff_methleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
data class Holder:
    label: str

    def get(self) -> str:
        return self.label

def twice(h: Holder) -> str:
    return h.get() + h.get()
";
    if build_source(
        source,
        "by_diff_methleak",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import sys, by_diff_methleak as m\n\
         label = 'x' * 40\n\
         h = m.Holder(label)\n\
         before = sys.getrefcount(label)\n\
         for _ in range(20000):\n\
        \x20   m.twice(h)\n\
        \x20   h.get()\n\
         after = sys.getrefcount(label)\n\
         print('stable' if after == before else f'leaked {before}->{after}')\n",
    );
    assert_eq!(out, "stable");
}

#[test]
fn a_borrowed_intermediate_does_not_leak_or_lose_a_reference() {
    // the borrow pass drops the retain/release pair around `n.inner`. getting it
    // wrong is either a leak or a use-after-free, so both directions are checked:
    // the label's refcount must be *stable*, and the inner holder must survive
    // being dropped from python while a compiled read is in flight
    let Some((python, toolchain)) = environment() else {
        return;
    };
    if !supports(&toolchain, (3, 10)) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    let dir = std::env::temp_dir().join("by_diff_borrow");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
data class Holder:
    label: str

data class Nest:
    inner: Holder

def inner_label(n: Nest) -> str:
    return n.inner.label
";
    if build_source(
        source,
        "by_diff_borrow",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import sys, by_diff_borrow as m\n\
         label = 'x' * 40\n\
         h = m.Holder(label)\n\
         n = m.Nest(h)\n\
         holder_refs = sys.getrefcount(h)\n\
         before = sys.getrefcount(label)\n\
         for _ in range(20000):\n\
        \x20   m.inner_label(n)\n\
         print('label', 'stable' if sys.getrefcount(label) == before else 'moved')\n\
         print('holder', 'stable' if sys.getrefcount(h) == holder_refs else 'moved')\n\
         del h\n\
         print(m.inner_label(n)[:4])\n",
    );
    assert_eq!(out, "label stable\nholder stable\nxxxx");
}

#[test]
fn calling_a_callable_value_agrees() {
    agree(
        "callvalue",
        "\
def apply(f: object, a: int) -> object:
    return f(a)

def apply2(f: object, a: int, b: int) -> object:
    return f(a, b)

def indirect(a: int) -> object:
    fn = abs
    return fn(a)

def shadowed(len: object, s: str) -> object:
    return len(s)

def nothing(f: object) -> object:
    return f()
",
        &[
            "m.apply(abs, -5)",
            "m.apply(str, 5)",
            "m.apply2(max, 3, 9)",
            "m.indirect(-7)",
            // a parameter shadowing a builtin has to win
            "m.shadowed(lambda s: 'shadowed', 'abc')",
            "m.nothing(dict)",
            // and a value that is not callable raises the same way
            "[type(e).__name__ for e in [_capture(m.apply, 3, 4)]]",
            "[type(e).__name__ for e in [_capture(m.nothing, None)]]",
        ],
    );
}

#[test]
fn a_borrow_survives_a_finalizer_that_runs_a_collection() {
    // the borrow pass may only skip the retain where nothing can run in between.
    // a `__del__` firing during the window would be the way to catch it wrong
    let Some((python, toolchain)) = environment() else {
        return;
    };
    if !supports(&toolchain, (3, 10)) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    let dir = std::env::temp_dir().join("by_diff_finalizer");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
data class Inner:
    label: str

data class Outer:
    inner: Inner

def read_then_call(o: Outer, f: object) -> str:
    held = o.inner.label
    f()
    return held + o.inner.label
";
    if build_source(
        source,
        "by_diff_finalizer",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import gc, by_diff_finalizer as m\n\
         class Noisy:\n\
        \x20   def __del__(self):\n        gc.collect()\n\
         o = m.Outer(m.Inner('alpha'))\n\
         def churn():\n\
        \x20   Noisy()\n\
        \x20   o.inner = m.Inner('bravo')\n\
         for _ in range(5000):\n\
        \x20   m.read_then_call(o, churn)\n\
         print(m.read_then_call(o, lambda: None))\n",
    );
    assert_eq!(out, "bravobravo");
}

#[test]
fn reading_a_global_as_a_value_agrees() {
    agree(
        "globalread",
        "\
LIMIT = 10

def limit() -> object:
    return LIMIT

def builtin_alias(a: int) -> object:
    fn = abs
    return fn(a)

def missing() -> object:
    return not_defined_anywhere
",
        &[
            "m.limit()",
            "m.builtin_alias(-9)",
            "[type(e).__name__ for e in [_capture(m.missing)]]",
            // a rebound global is observed, because the read is not cached
            "[(setattr(m, 'LIMIT', 99), m.limit())[-1]]",
        ],
    );
}

#[test]
fn closures_agree() {
    agree_with_declines(
        "closures",
        "\
def make_adder(n: int) -> object:
    def add(a: int) -> int:
        return a + n
    return add

def make_pair(a: int, b: str) -> object:
    def describe(times: int) -> str:
        return b * times + str(a)
    return describe

def helper(a: int) -> int:
    def double(x: int) -> int:
        return x * 2
    return double(a) + double(a)

def compose(f: object, g: object) -> object:
    def both(n: int) -> object:
        return f(g(n))
    return both

def counted(n: int) -> int:
    def step(a: int) -> int:
        return a + n
    total = 0
    for i in range(4):
        total = step(total)
    return total

def used_early(a: int) -> int:
    if a > 0:
        return later(a)
    def later(x: int) -> int:
        return x
    return later(a)
",
        &[
            "m.make_adder(5)(3)",
            // two closures from one function must have independent environments
            "[(m.make_adder(1), m.make_adder(100))[0](0), m.make_adder(100)(0)]",
            "m.make_pair(7, 'ab')(2)",
            "m.helper(4)",
            "m.compose(abs, lambda n: n - 10)(3)",
            "m.counted(2)",
            "[m.make_adder(a)(a) for a in (0, -3, 10 ** 20)]",
            // the closure is a real callable python can inspect and pass around
            "callable(m.make_adder(1))",
            "sorted([3, 1, 2], key=m.make_adder(0))",
            // and a wrong argument count still raises
            "[type(e).__name__ for e in [_capture(m.make_adder(1))]]",
            // python raises here, and the interpreted fallback is what reports it
            "[type(e).__name__ for e in [_capture(m.used_early, 5)]]",
        ],
    );
}

#[test]
fn a_closure_does_not_leak_its_environment() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_closureleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def make(label: str) -> object:
    def get(times: int) -> str:
        return label * times
    return get
";
    if build_source(
        source,
        "by_diff_closureleak",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // the environment holds the captured `str`, and releasing the closure has to
    // release the environment, which releases the field
    let out = run(
        &python,
        &dir,
        "import sys, by_diff_closureleak as m\n\
         label = 'x' * 40\n\
         before = sys.getrefcount(label)\n\
         for _ in range(20000):\n\
        \x20   m.make(label)(1)\n\
         after = sys.getrefcount(label)\n\
         print('stable' if after == before else f'leaked {before}->{after}')\n\
         held = m.make(label)\n\
         print(sys.getrefcount(label) > before)\n\
         del held\n\
         print(sys.getrefcount(label) == before)\n",
    );
    assert_eq!(out, "stable\nTrue\nTrue");
}

#[test]
fn a_closure_environment_is_not_visible_in_the_module() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_envhidden");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def make(n: int) -> object:
    def get() -> int:
        return n
    return get
";
    if build_source(
        source,
        "by_diff_envhidden",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import by_diff_envhidden as m\n\
         print([n for n in dir(m) if 'env' in n])\n\
         print(m.make(3)())\n",
    );
    assert_eq!(out, "[]\n3");
}

#[test]
fn a_raise_out_of_a_try_body_does_not_leak_what_it_wrote() {
    // the exception edge is a CFG edge, and the refcount pass used not to follow it
    // — so everything the `try` body had written leaked on the exceptional path
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_handlerleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def guarded(words: list[str], index: int) -> str:
    held = \"held\" + words[0]
    try:
        return held + words[index]
    except IndexError:
        return held
";
    if build_source(
        source,
        "by_diff_handlerleak",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import sys, by_diff_handlerleak as m\n\
         words = ['x' * 40]\n\
         label = words[0]\n\
         before = sys.getrefcount(label)\n\
         for _ in range(20000):\n\
        \x20   m.guarded(words, 9)\n\
         after = sys.getrefcount(label)\n\
         print('stable' if after == before else f'leaked {before}->{after}')\n\
         print(m.guarded(words, 0)[:8])\n",
    );
    assert_eq!(out, "stable\nheldxxxx");
}

#[test]
fn a_mutable_capture_agrees() {
    // python closes over the *variable*, so all of these depend on both frames seeing
    // one cell — a copy at `def` time would give different answers for every one
    agree(
        "cells",
        "\
def counter() -> (() -> int):
    n = 0
    def get() -> int:
        return n
    n = 1
    return get

def bumper() -> (() -> int):
    n = 0
    def bump() -> int:
        nonlocal n
        n = n + 1
        return n
    return bump

def loop_closures() -> list[object]:
    out = []
    i = 0
    while i < 3:
        def show() -> int:
            return i
        out.append(show)
        i = i + 1
    return out

def shared_pair(start: int) -> list[object]:
    def read() -> int:
        return start
    def write(v: int) -> int:
        nonlocal start
        start = v
        return start
    return [read, write]

def accumulate(values: list[int]) -> int:
    total = 0
    def add(v: int) -> int:
        nonlocal total
        total = total + v
        return total
    for v in values:
        add(v)
    return total
",
        &[
            "m.counter()()",
            "[(b := m.bumper(), b(), b(), b())[1:]]",
            "[f() for f in m.loop_closures()]",
            // one cell: the write through one closure is visible through the other
            "[(p := m.shared_pair(5), p[0](), p[1](9), p[0]())[1:]]",
            "m.accumulate([1, 2, 3])",
            "[m.accumulate([a, a]) for a in (0, -4, 10 ** 20)]",
        ],
    );
}

#[test]
fn reading_a_cell_before_it_is_written_raises_the_way_python_does() {
    // a cell starts unset, and NULL has to read back as an error rather than a zero
    agree_with_declines(
        "cellunset",
        "\
def early() -> object:
    def get() -> int:
        return n
    out = get
    n = 1
    return out

def early_call() -> object:
    def get() -> int:
        return n
    result = _capture_local(get)
    n = 1
    return result

def _capture_local(f: object) -> object:
    try:
        return f()
    except NameError as e:
        return type(e).__name__
",
        &[
            // reading it after the write is fine
            "m.early()()",
            // reading it before is `UnboundLocalError`, which is a `NameError`
            "m.early_call()",
        ],
    );
}

#[test]
fn a_shared_cell_does_not_leak() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_cellleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def holder(label: str) -> ((str) -> str):
    current = label
    def swap(next: str) -> str:
        nonlocal current
        previous = current
        current = next
        return previous
    return swap
";
    if build_source(
        source,
        "by_diff_cellleak",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // the cell holds a reference and a write must release the old one. the live
    // closure has to be dropped before measuring, or its own hold on the cell reads
    // as a leak
    let out = run(
        &python,
        &dir,
        "import gc, sys, by_diff_cellleak as m\n\
         label = 'x' * 40\n\
         other = 'y' * 40\n\
         before = sys.getrefcount(label)\n\
         for _ in range(20000):\n\
        \x20   swap = m.holder(label)\n\
        \x20   swap(other)\n\
        \x20   swap(label)\n\
         del swap\n\
         print('refs', 'stable' if sys.getrefcount(label) == before else 'leaked')\n\
         gc.collect()\n\
         objects = len(gc.get_objects())\n\
         for _ in range(2000):\n\
        \x20   held = m.holder(label)\n\
        \x20   held(other)\n\
         del held\n\
         gc.collect()\n\
         print('envs', 'stable' if len(gc.get_objects()) <= objects else 'leaked')\n",
    );
    assert_eq!(out, "refs stable\nenvs stable");
}

#[test]
fn generators_agree() {
    agree_with_declines(
        "generators",
        "\
def counted(n: int) -> object:
    i = 0
    while i < n:
        yield i
        i = i + 1

def three() -> object:
    yield 1
    yield 2
    yield 3

def accumulating(words: list[str]) -> object:
    seen = \"\"
    for w in words:
        seen = seen + w
        yield seen

def pairs(xs: list[int], ys: list[int]) -> object:
    for a in xs:
        for b in ys:
            yield a * b

def nothing() -> object:
    if False:
        yield 1

def early(n: int) -> object:
    yield n
    return

def echoing() -> object:
    total = 0
    while True:
        got = yield total
        total = total + 1
",
        &[
            "list(m.counted(4))",
            "list(m.three())",
            "list(m.accumulating(['a', 'bb', 'ccc']))",
            "list(m.pairs([1, 2, 3], [10, 20]))",
            "list(m.nothing())",
            "list(m.early(7))",
            // arbitrary precision survives the suspension
            "list(m.counted(3))[-1] + 10 ** 20",
            // partial consumption, then more
            "[(g := m.counted(5), next(g), next(g), list(g))[1:]]",
            // it is a real iterator, so everything that takes one works
            "sum(m.counted(5))",
            "sorted(m.three(), reverse=True)",
            "[x for x in m.counted(3) if x]",
            "list(zip(m.counted(3), m.three()))",
            // exhaustion keeps raising
            "[type(e).__name__ for e in [_capture(next, m.nothing())]]",
            "[(g := m.early(1), next(g), type(_capture(next, g)).__name__, type(_capture(next, g)).__name__)[2:]]",
            // `send` is what the `yield` expression evaluates to
            "[(e := m.echoing(), next(e), e.send(9), e.send(9))[1:]]",
            // `close` exhausts it
            "[(g := m.counted(9), next(g), g.close(), type(_capture(next, g)).__name__)[3:]]",
        ],
    );
}

#[test]
fn a_generator_is_a_real_iterator_to_python() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_geniter");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def counted(n: int) -> object:
    i = 0
    while i < n:
        yield i
        i = i + 1
";
    if build_source(
        source,
        "by_diff_geniter",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import by_diff_geniter as m\n\
         g = m.counted(3)\n\
         print(iter(g) is g)\n\
         print(hasattr(g, '__next__'), hasattr(g, 'send'), hasattr(g, 'close'))\n\
         print(list(g))\n",
    );
    assert_eq!(out, "True\nTrue True True\n[0, 1, 2]");
}

/// the generator shapes every leak test below drives, built once
///
/// `require_native` is what makes the answers the *compiled* generator's: a
/// declined function would run from its interpreted definition and leak nothing,
/// so the test would pass without exercising anything
fn leak_module(tag: &'static str) -> Option<(String, std::path::PathBuf)> {
    let (python, toolchain) = environment()?;
    let dir = std::env::temp_dir().join(tag);
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Boom(Exception):
    pass

def repeat(label: str, times: int) -> object:
    i = 0
    while i < times:
        yield label
        i = i + 1

def guarded(times: int) -> object:
    i = 0
    while i < times:
        try:
            yield i
        except Boom:
            yield -1
        i = i + 1

def in_handler(times: int) -> object:
    i = 0
    while i < times:
        try:
            raise Boom()
        except Boom:
            yield i
        i = i + 1
";
    let options = Options {
        require_native: true,
        ..Options::default()
    };
    if build_source(source, tag, &toolchain, &dir, &options).is_err() {
        eprintln!("skipping: no working C toolchain");
        return None;
    }
    Some((python, dir))
}

/// the two counters every leak test here uses
///
/// a *total* object count is too noisy to see one leaked object per iteration
/// until the iteration count is large, and a count of some unrelated object is
/// blind to it entirely — so each of these watches the object that would actually
/// be leaked: instances of one class, and the reference count of one instance
const LEAK_INSTRUMENTS: &str = "\
import gc, sys
def live(kind):
    gc.collect()
    return sum(1 for o in gc.get_objects() if type(o) is kind)
def leaked(kind, once):
    base = live(kind)
    for _ in range(50):
        once()
    return live(kind) - base
";

#[test]
fn a_generator_does_not_leak_its_state() {
    let Some((python, dir)) = leak_module("by_diff_genleak") else {
        return;
    };
    // the state object holds every parameter, and dropping it must release them —
    // including when the generator is abandoned part-way through.
    //
    // watching the *label* alone is what let a leaked `GeneratorExit` through for
    // as long as it did: the parameter is released either way, so the count below
    // it is the one that moves
    let out = run(
        &python,
        &dir,
        &format!(
            "{LEAK_INSTRUMENTS}\
             import by_diff_genleak as m\n\
             label = 'x' * 40\n\
             before = sys.getrefcount(label)\n\
             for _ in range(20000):\n\
            \x20   list(m.repeat(label, 3))\n\
             print('drained', 'stable' if sys.getrefcount(label) == before else 'leaked')\n\
             for _ in range(20000):\n\
            \x20   g = m.repeat(label, 9)\n\
            \x20   next(g)\n\
             del g\n\
             gc.collect()\n\
             print('abandoned', 'stable' if sys.getrefcount(label) == before else 'leaked')\n\
             def abandon():\n\
            \x20   g = m.repeat(label, 9)\n\
            \x20   next(g)\n\
             print('exit objects', leaked(GeneratorExit, abandon))\n"
        ),
    );
    assert_eq!(out, "drained stable\nabandoned stable\nexit objects 0");
}

/// finalising a suspended generator throws `GeneratorExit` in, and the unwind has
/// to release it — the frame does not own what it hands to the error state
#[test]
fn ending_a_generator_releases_the_generator_exit() {
    let Some((python, dir)) = leak_module("by_diff_genexit") else {
        return;
    };
    let out = run(
        &python,
        &dir,
        &format!(
            "{LEAK_INSTRUMENTS}\
             import by_diff_genexit as m\n\
             def abandoned():\n\
            \x20   g = m.repeat('x', 9)\n\
            \x20   next(g)\n\
             def exhausted():\n\
            \x20   list(m.repeat('x', 3))\n\
             def closed():\n\
            \x20   g = m.repeat('x', 9)\n\
            \x20   next(g)\n\
            \x20   g.close()\n\
             def in_handler():\n\
            \x20   g = m.in_handler(9)\n\
            \x20   next(g)\n\
             print('abandoned', leaked(GeneratorExit, abandoned))\n\
             print('exhausted', leaked(GeneratorExit, exhausted))\n\
             print('closed', leaked(GeneratorExit, closed))\n\
             print('in handler', leaked(GeneratorExit, in_handler))\n"
        ),
    );
    assert_eq!(out, "abandoned 0\nexhausted 0\nclosed 0\nin handler 0");
}

/// a `throw` raises at the suspension point, and whether a handler catches it or
/// it comes back out, nothing may hold a reference afterwards
#[test]
fn throwing_into_a_generator_releases_the_exception() {
    let Some((python, dir)) = leak_module("by_diff_genthrow") else {
        return;
    };
    // two instruments over the same shapes: one instance thrown again and again,
    // whose own reference count moves even though the object stays reachable, and a
    // fresh instance per call, which a retained reference keeps alive as a countable
    // object.
    //
    // the exception is a global rather than a parameter on purpose. a caught
    // exception's traceback holds the frames it passed through, so a local naming it
    // is a reference the *test* keeps — 50 of them, in both builds alike
    let out = run(
        &python,
        &dir,
        &format!(
            "{LEAK_INSTRUMENTS}\
             import by_diff_genthrow as m\n\
             def caught():\n\
            \x20   g = m.guarded(9)\n\
            \x20   next(g)\n\
            \x20   g.throw(thrown)\n\
             def uncaught():\n\
            \x20   g = m.repeat('x', 9)\n\
            \x20   next(g)\n\
            \x20   try:\n\
            \x20       g.throw(thrown)\n\
            \x20   except m.Boom:\n\
            \x20       pass\n\
             def in_handler():\n\
            \x20   g = m.in_handler(9)\n\
            \x20   next(g)\n\
             def fresh(once):\n\
            \x20   def run():\n\
            \x20       global thrown\n\
            \x20       thrown = m.Boom()\n\
            \x20       once()\n\
            \x20       thrown = None\n\
            \x20   return run\n\
             thrown = m.Boom()\n\
             boom = thrown\n\
             gc.collect()\n\
             held = sys.getrefcount(boom)\n\
             for _ in range(50):\n\
            \x20   caught()\n\
            \x20   uncaught()\n\
             gc.collect()\n\
             print('thrown refs', sys.getrefcount(boom) - held)\n\
             print('caught', leaked(m.Boom, fresh(caught)))\n\
             print('uncaught', leaked(m.Boom, fresh(uncaught)))\n\
             print('raised inside', leaked(m.Boom, in_handler))\n"
        ),
    );
    assert_eq!(out, "thrown refs 0\ncaught 0\nuncaught 0\nraised inside 0");
}

/// the same rule outside a generator, which is where it is stated: a re-raise
/// hands the exception to the interpreter and keeps nothing
#[test]
fn a_re_raise_does_not_retain_the_exception() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_reraiseleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Boom(Exception):
    pass

def bare(x: int) -> int:
    try:
        raise Boom()
    except Boom:
        raise

def unmatched(x: int) -> int:
    try:
        raise Boom()
    except ValueError:
        return 1
    return 2

def through_finally(x: int) -> int:
    try:
        raise Boom()
    finally:
        x = x + 1
";
    let options = Options {
        require_native: true,
        ..Options::default()
    };
    if build_source(source, "by_diff_reraiseleak", &toolchain, &dir, &options).is_err() {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        &format!(
            "{LEAK_INSTRUMENTS}\
             import by_diff_reraiseleak as m\n\
             def raising(fn):\n\
            \x20   def once():\n\
            \x20       try:\n\
            \x20           fn(1)\n\
            \x20       except m.Boom:\n\
            \x20           pass\n\
            \x20   return once\n\
             for name in ('bare', 'unmatched', 'through_finally'):\n\
            \x20   print(name, leaked(m.Boom, raising(getattr(m, name))))\n"
        ),
    );
    assert_eq!(out, "bare 0\nunmatched 0\nthrough_finally 0");
}

#[test]
fn delegation_agrees() {
    agree_with_declines(
        "delegation",
        "\
def inner(n: int) -> object:
    i = 0
    while i < n:
        yield i
        i = i + 1
    return n * 100

def outer(n: int) -> object:
    got = yield from inner(n)
    yield got

def chained(xs: list[int]) -> object:
    yield from xs
    yield from xs

def nested(n: int) -> object:
    yield from outer(n)
",
        &[
            "list(m.inner(3))",
            // the delegated `return` value is what the expression evaluates to
            "list(m.outer(3))",
            "list(m.chained([1, 2]))",
            "list(m.nested(2))",
            "list(m.chained([]))",
            "sum(m.outer(4))",
            // and the inner generator's own `StopIteration` value reaches the consumer
            "[(g := m.inner(1), next(g), _capture(next, g).value)[2:]]",
        ],
    );
}

#[test]
fn async_await_agrees() {
    agree_with_declines(
        "asyncawait",
        "\
async def plain(n: int) -> int:
    return n * 2

async def chained(n: int) -> int:
    a = await plain(n)
    b = await plain(a)
    return b + 1

async def nothing() -> None:
    pass

async def forwards(n: int) -> int:
    return await chained(n)
",
        &[
            "__import__('asyncio').run(m.plain(5))",
            "__import__('asyncio').run(m.chained(3))",
            "__import__('asyncio').run(m.nothing())",
            "__import__('asyncio').run(m.forwards(1))",
            "[__import__('asyncio').run(m.plain(a)) for a in (0, -3, 10 ** 20)]",
        ],
    );
}

#[test]
fn the_await_protocol_agrees() {
    // an `await` reaches its object through `__await__` and drives what comes back
    // by sending into it. both halves have edges worth pinning: what counts as
    // awaitable at all, and what a finished delegation's value *is*
    agree(
        "awaitproto",
        "\
async def awaited(x: object) -> object:
    return await x

async def twice(x: object, y: object) -> object:
    a = await x
    b = await y
    return (a, b)

async def guarded(x: object) -> object:
    try:
        return await x
    except ValueError as e:
        return 'caught ' + str(e)

def delegated(xs: object) -> object:
    got = yield from xs
    yield got

def returns(v: object) -> object:
    return v
    yield

async def returns_async(v: object) -> object:
    return v

def redelegated(v: object) -> object:
    got = yield from returns(v)
    yield got
",
        &[
            // the *other* half of the same rule: what a compiled frame's own return
            // value becomes on the way out. `StopIteration` reads its argument as an
            // argument *list*, so a tuple would arrive spread and an exception
            // instance would be raised in place of the `StopIteration` itself
            "[_value(m.returns(v)) for v in ((1, 2), (), (1,), 5, None, [1, 2], 'ab')]",
            "[_run(m.returns_async(v)) for v in ((1, 2), (), (1,), 5, None, [1, 2])]",
            "[list(m.redelegated(v)) for v in ((1, 2), (), (1,), 5, None)]",
            "[_run(m.awaited(m.returns_async(v))) for v in ((1, 2), (), 5, None)]",
            "_value(m.returns(ValueError('x')))",
            "_value(m.returns(StopIteration(9)))",
            "_run(m.returns_async(StopIteration(9)))",
            // and the exception it rides on is shaped the way python shapes it
            "[(lambda e: (repr(e), e.args))(_capture(next, m.returns(v))) \
              for v in ((1, 2), (), 5, None)]",
            // an await that completes without ever suspending — the common case
            "_run(m.awaited(_Ready(7)))",
            "_run(m.twice(_Ready(1), _Ready(2)))",
            // one that really suspends, driven by a loop that has to resume it
            "_run(m.awaited(_sleeps(9)))",
            "_run(m.awaited(_Suspends(11)))",
            "_run(m.twice(_Ready(1), _sleeps(2)))",
            "_run(m.twice(_Suspends(1), _Suspends(2)))",
            // a bare `StopIteration` carries `None`, and a subclass carries its own
            "_run(m.awaited(_Raises(StopIteration())))",
            "_run(m.awaited(_Raises(StopIteration(3))))",
            "_run(m.awaited(_Raises(_Sub(5))))",
            // python reads the exception's field, not its `value` attribute
            "_run(m.awaited(_Raises(_Shadowed(7))))",
            "_Shadowed(7).value",
            // a raised *type* rather than an instance still has to become one
            "_run(m.awaited(_Raises(StopIteration)))",
            // normalising a tuple argument would take only its first element
            "_run(m.awaited(_Raises(StopIteration((1, 2)))))",
            // anything else escaping the awaited object is not a return value
            "_capture_async(m.awaited, _throws(ValueError))",
            "_capture_async(m.awaited, _throws(KeyError))",
            "_run(m.guarded(_throws(ValueError)))",
            "_capture_async(m.guarded, _throws(KeyError))",
            // and what is not awaitable at all says so, about the right object
            "_capture_async(m.awaited, 5)",
            "_capture_async(m.awaited, _counting(1))",
            "_capture_async(m.awaited, _NotIter())",
            "_capture_async(m.awaited, _CoroAwait())",
            // `yield from` is the same machine over an iterator rather than an
            // awaitable, and an exhausted one returns `None` rather than nothing
            "list(m.delegated(iter([1, 2, 3])))",
            "list(m.delegated(_counting(2)))",
            "list(m.delegated(iter([])))",
            "_capture(list, m.delegated(_RaiseIter(_Sub(5))))",
            "_sent(m.delegated(_counting(2)), (7, 8))",
        ],
    );
}

#[test]
fn a_value_live_across_a_suspension_agrees() {
    // python evaluates left to right, so `total + await step(i)` has the read of
    // `total` on the stack while the `await` suspends. every shape here holds
    // something across a suspension that has no name of its own, and `agree` is what
    // says they compile rather than falling back
    agree(
        "parked",
        "\
async def stepped(i: int) -> int:
    return (i * 7) % 13

async def summed(n: int) -> int:
    total = 0
    i = 0
    while i < n:
        total = total + await stepped(i)
        i = i + 1
    return total

async def paired(n: int) -> int:
    return (await stepped(n)) + (await stepped(n + 1))

async def tripled(n: int) -> int:
    return (await stepped(n)) * 100 + (await stepped(n + 1)) * 10 + (await stepped(n + 2))

def mixed(a: int, b: int, c: int) -> int:
    return a * 100 + b * 10 + c

async def spanning(n: int) -> int:
    return mixed(n * 5, await stepped(n), await stepped(n + 1))

async def awaited(step: object, n: int) -> int:
    total = 0
    i = 0
    while i < n:
        total = total + await step(i)
        i = i + 1
    return total

def echoing(n: int) -> object:
    total = 0
    i = 0
    while i < n:
        total = total + (yield i)
        i = i + 1
    return total

def twinned() -> object:
    return (yield 1) + (yield 2)

def straddling(n: int) -> object:
    yield mixed(n * 5, (yield 1), (yield 2))

def recovering(n: int) -> object:
    total = n * 2
    try:
        yield total
    except ValueError:
        yield total + 1
    yield total + 2

def crossed(xs: list[int], ys: list[int]) -> object:
    for a in xs:
        base = a * 100
        for b in ys:
            yield base + (yield b)

def counting(n: int) -> object:
    yield n
    return n * 100

def relayed(n: int) -> object:
    base = n + 1
    got = yield from counting(n)
    yield base + got
",
        &[
            "__import__('asyncio').run(m.summed(40))",
            "__import__('asyncio').run(m.paired(3))",
            "__import__('asyncio').run(m.tripled(3))",
            "__import__('asyncio').run(m.spanning(3))",
            // `n * 5` is read before the first suspension and used after the *second*,
            // so nothing between the two reads it. the static flow calls it dead at the
            // first — a suspension is a `return` and goes nowhere — and one field has to
            // carry it across both
            "_sent(m.straddling(3), (4, 5))",
            // arbitrary precision survives the park, which a machine word would not
            "__import__('asyncio').run(m.summed(3)) + 10 ** 30",
            // awaiting something that *does* suspend, so the loop resumes the frame
            "__import__('asyncio').run(m.awaited(_slow, 4))",
            "_sent(m.echoing(4), (10, 20, 30, 40))",
            "_sent(m.twinned(), (5, 7))",
            "list(m.recovering(3))",
            // a `throw` the handler catches, and the parked value read after it
            "_recovered(m.recovering(3), ValueError, 2)",
            "_sent(m.crossed([1, 2], [7, 8]), (100, 200, 300, 400, 500, 600, 700, 800))",
            "list(m.relayed(4))",
            // a coroutine driven by hand, with no loop underneath it
            "[(c := m.summed(3), _capture(c.send, None).value)[1:]]",
        ],
    );
}

#[test]
fn a_handled_throw_leaves_the_generator_usable() {
    // the field carrying what `throw` wants raised starts null, and is *emptied* by
    // writing `None` into it because no operation stores a null. reading it back as a
    // second exception raised `SystemError` at every later resumption
    agree(
        "rethrown",
        "\
def recovering(n: int) -> object:
    try:
        yield n
    except ValueError:
        pass
    yield n + 2
    yield n + 3
",
        &[
            "_recovered(m.recovering(3), ValueError, 2)",
            "list(m.recovering(3))",
        ],
    );
}

#[test]
fn a_parked_value_does_not_leak() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_parkleak");
    let _ = std::fs::remove_dir_all(&dir);
    // `label + (yield i)` holds the label across the suspension, so the state object
    // owns it for as long as the frame is parked
    let source = "\
def tagged(label: str, times: int) -> object:
    i = 0
    while i < times:
        yield label + (yield i)
        i = i + 1

def handling(label: str) -> object:
    try:
        yield label
    except ValueError:
        yield label + \"!\"
    yield label + \"?\"
";
    if build_source(
        source,
        "by_diff_parkleak",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // driven to exhaustion, and abandoned *while suspended* with the value parked
    let out = run(
        &python,
        &dir,
        "import gc, sys, by_diff_parkleak as m\n\
         label = 'x' * 40\n\
         before = sys.getrefcount(label)\n\
         for _ in range(20000):\n\
        \x20   g = m.tagged(label, 2)\n\
        \x20   next(g)\n\
        \x20   g.send('a')\n\
        \x20   g.close()\n\
         del g\n\
         gc.collect()\n\
         print('driven', 'stable' if sys.getrefcount(label) == before else 'leaked')\n\
         for _ in range(20000):\n\
        \x20   g = m.tagged(label, 9)\n\
        \x20   next(g)\n\
        \x20   g.send('a')\n\
         del g\n\
         gc.collect()\n\
         print('parked', 'stable' if sys.getrefcount(label) == before else 'leaked')\n\
         for _ in range(20000):\n\
        \x20   g = m.handling(label)\n\
        \x20   next(g)\n\
        \x20   g.throw(ValueError('boom'))\n\
         del g\n\
         gc.collect()\n\
         print('handled', 'stable' if sys.getrefcount(label) == before else 'leaked')\n",
    );
    // a suspension inside `except` parks the exception the handler took over from, so
    // the state object is holding one when it is dropped mid-handler
    assert_eq!(out, "driven stable\nparked stable\nhandled stable");
}

#[test]
fn a_coroutine_is_awaitable_and_not_iterable() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_coro");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
async def plain(n: int) -> int:
    return n * 2
";
    if build_source(
        source,
        "by_diff_coro",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // `asyncio.iscoroutine` tests the abc, and iterating a coroutine has to be an
    // error rather than quietly working
    let out = run(
        &python,
        &dir,
        "import asyncio, collections.abc as abc, by_diff_coro as m\n\
         c = m.plain(2)\n\
         print(asyncio.iscoroutine(c), isinstance(c, abc.Coroutine))\n\
         print(hasattr(c, 'send'), hasattr(c, 'throw'), hasattr(c, 'close'))\n\
         try:\n    list(c)\n\
         except TypeError:\n    print('not iterable')\n\
         print(asyncio.run(c))\n",
    );
    assert_eq!(out, "True True\nTrue True True\nnot iterable\n4");
}

#[test]
fn a_coroutine_does_not_leak() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_coroleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
async def echo(label: str) -> str:
    return label
";
    if build_source(
        source,
        "by_diff_coroleak",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // including a coroutine that is created and never awaited
    let out = run(
        &python,
        &dir,
        "import asyncio, gc, sys, warnings, by_diff_coroleak as m\n\
         warnings.simplefilter('ignore')\n\
         label = 'x' * 40\n\
         before = sys.getrefcount(label)\n\
         for _ in range(5000):\n\
        \x20   asyncio.run(m.echo(label))\n\
         print('awaited', 'stable' if sys.getrefcount(label) == before else 'leaked')\n\
         for _ in range(5000):\n\
        \x20   c = m.echo(label)\n\
         del c\n\
         gc.collect()\n\
         print('abandoned', 'stable' if sys.getrefcount(label) == before else 'leaked')\n",
    );
    assert_eq!(out, "awaited stable\nabandoned stable");
}

#[test]
fn with_blocks_agree() {
    agree_with_declines(
        "withblocks",
        "\
def counted(path: str) -> int:
    total = 0
    with open(path) as f:
        for line in f:
            total = total + 1
    return total

def guarded(mgr: object) -> str:
    with mgr:
        return \"body\"
    return \"after\"

def nested(mgr: object, other: object) -> str:
    with mgr:
        with other:
            return \"inner\"
    return \"after\"

def suppressing(mgr: object) -> str:
    with mgr:
        raise ValueError(\"boom\")
    return \"suppressed\"

def propagating(mgr: object) -> str:
    with mgr:
        raise ValueError(\"boom\")
    return \"not reached\"

def entered(mgr: object) -> object:
    with mgr as value:
        return value
    return None
",
        &[
            // `__exit__` runs on the normal path with `(None, None, None)`, and a
            // `return` from inside the body is one of those paths
            "_run_recording(m)",
            "m.suppressing(_Swallow())",
            "[type(e).__name__ for e in [_capture(m.propagating, _Pass())]]",
            "m.entered(_Value(7))",
            "_run_nested(m)",
        ],
    );
}

#[test]
fn an_early_exit_runs_the_finally_it_is_leaving() {
    // this was a silent wrong answer in a shipped feature: a `return` or a `break`
    // inside `try` skipped the `finally` entirely
    agree(
        "unwind",
        "\
def early(log: list[str]) -> str:
    try:
        return \"body\"
    finally:
        log.append(\"finally\")

def looped(log: list[str], n: int) -> str:
    i = 0
    while i < n:
        try:
            if i == 1:
                break
        finally:
            log.append(\"f\")
        i = i + 1
    return \"done\"

def continued(log: list[str], n: int) -> int:
    total = 0
    i = 0
    while i < n:
        i = i + 1
        try:
            if i == 2:
                continue
            total = total + i
        finally:
            log.append(\"f\")
    return total

def layered(log: list[str]) -> str:
    try:
        try:
            return \"deep\"
        finally:
            log.append(\"inner\")
    finally:
        log.append(\"outer\")
",
        &[
            "[(log := [], m.early(log), log)[1:]]",
            "[(log := [], m.looped(log, 3), log)[1:]]",
            "[(log := [], m.continued(log, 4), log)[1:]]",
            // innermost first, and both of them
            "[(log := [], m.layered(log), log)[1:]]",
        ],
    );
}

#[test]
fn parameter_defaults_agree() {
    agree_with_declines(
        "defaults",
        "\
def greet(name: str, greeting: str = \"hi\", times: int = 1) -> str:
    return (greeting + \" \" + name) * times

def offset(a: int, b: int = 10) -> int:
    return a + b

def flagged(a: int, on: bool = True) -> int:
    if on:
        return a
    return -a

def boxed_none(a: object = None) -> object:
    return a

def boxed_int(a: object = 7) -> object:
    return a

def boxed_bool(a: object = True) -> object:
    return a

def boxed_float(a: object = 1.5) -> object:
    return a

def optional(a: int, extra: object = None) -> object:
    if extra is None:
        return a
    return extra

def computed(a: int, b: object = []) -> object:
    return b
",
        &[
            "m.greet('a')",
            "m.greet('a', 'yo')",
            "m.greet('a', 'yo', 2)",
            "m.offset(1)",
            "m.offset(1, 2)",
            "(m.flagged(3), m.flagged(3, False))",
            "(m.optional(1), m.optional(1, 'x'))",
            // too few arguments still raises, and so does too many
            "[type(e).__name__ for e in [_capture(m.offset)]]",
            "[type(e).__name__ for e in [_capture(m.offset, 1, 2, 3)]]",
            // a computed default declines, and the interpreted twin keeps the identity
            "m.computed(1)",
            // an immediate written into an *object* place has to be boxed: the
            // unboxed `None` is a bare byte, and `By_NewRef` of one is a NULL the
            // error check reads as a failure with no exception behind it
            "m.boxed_none()",
            "m.boxed_none(1)",
            "m.boxed_int()",
            "m.boxed_bool()",
            "m.boxed_float()",
            "(m.boxed_none() is None, m.boxed_bool() is True)",
        ],
    );
}

#[test]
fn a_default_filled_at_a_native_call_site_agrees() {
    // `parameter_defaults_agree` calls these from python, which fills a default in the
    // *wrapper* — where the value is already an object. a compiled caller fills it
    // inline instead, and that path pushed the default with no coercion at all: a bare
    // `length=0` reaching an unannotated parameter put a tagged integer where the
    // callee declares a `PyObject *`. it was `shutil.py` failing to build that found it,
    // and only because the two representations differ enough for a C compiler to object
    agree(
        "native_defaults",
        "\
def taking(a, b, length=0, name=\"n\", scale=1.5, on=True, extra=None):
    return (a, b, length, name, scale, on, extra)


def calling(x: int) -> object:
    return taking(x, x)


def partly(x: int) -> object:
    return taking(x, x, 3)


def by_keyword(x: int) -> object:
    return taking(x, x, on=False)


def annotated(a: int, step: int = 2) -> int:
    return a + step


def calling_annotated(x: int) -> int:
    return annotated(x)
",
        &[
            "m.calling(1)",
            "m.partly(2)",
            "m.by_keyword(3)",
            "[m.calling_annotated(n) for n in (0, 5)]",
            "m.taking(1, 2)",
            "(m.calling(1)[3], type(m.calling(1)[4]).__name__, m.calling(1)[6] is None)",
        ],
    );
}

#[test]
fn a_string_default_does_not_leak() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_defaultleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def padded(a: str, fill: str = \"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\") -> str:
    return a + fill
";
    if build_source(
        source,
        "by_diff_defaultleak",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // the wrapper releases its arguments, so a default handed over borrowed would be
    // released twice — and one handed over with an extra reference would leak
    let out = run(
        &python,
        &dir,
        "import sys, by_diff_defaultleak as m\n\
         fill = 'x' * 40\n\
         before = sys.getrefcount(fill)\n\
         for _ in range(20000):\n\
        \x20   m.padded('a')\n\
         print('stable' if sys.getrefcount(fill) == before else 'moved')\n\
         print(m.padded('a')[:3])\n",
    );
    assert_eq!(out, "stable\naxx");
}

#[test]
fn keyword_arguments_agree() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    agree_with_declines(
        "keywords",
        "\
def offset(a: int, b: int = 10) -> int:
    return a + b

def described(name: str, sep: str = \"-\", times: int = 1) -> str:
    return (name + sep) * times

def caller(a: int) -> int:
    return offset(a, b=100) + offset(b=1, a=a) + offset(a)

data class Point:
    x: int
    y: int

    def shifted(self, dx: int = 0, dy: int = 0) -> int:
        return self.x + dx + self.y + dy
",
        &[
            // a python caller, by name and by position
            "m.offset(1, b=2)",
            "m.offset(b=2, a=1)",
            "m.offset(1)",
            "m.described('a', times=3)",
            "m.described(name='a', sep='+', times=2)",
            // a compiled caller resolves the names against the callee's signature
            "m.caller(1)",
            // and a method's keywords work through the type object
            "m.Point(1, 2).shifted(dy=10)",
            "m.Point(1, 2).shifted(3, dy=10)",
            // the error cases match
            "[type(e).__name__ for e in [_capture(m.offset, 1, 2, 3)]]",
            "[str(_capture_kw(m.offset, (1,), {'a': 2}))]",
            "[str(_capture_kw(m.offset, (), {'zzz': 1}))]",
            "[str(_capture(m.offset))]",
        ],
    );
}

#[test]
fn variadic_parameters_agree() {
    agree_with_declines(
        "variadic",
        "\
def total(*values: int) -> int:
    out = 0
    for v in values:
        out = out + v
    return out

def named(prefix: str, *rest: str) -> str:
    out = prefix
    for r in rest:
        out = out + r
    return out

def options(a: int, **rest: object) -> int:
    return a + len(rest)

def both(a: int, *rest: int, **named: object) -> int:
    return a + len(rest) + len(named)

def tupled(*values: int) -> object:
    return values

def mapped(**named: object) -> object:
    return named

def calls_total(a: int, b: int) -> int:
    return total(a, b, 3)

def calls_options(a: int) -> int:
    return options(a, x=1, y=2)

def calls_both(a: int) -> int:
    return both(a, 1, 2, k=3, j=4)

def calls_none() -> int:
    return total()
",
        &[
            "m.total()",
            "m.total(1, 2, 3)",
            "(m.named('a'), m.named('a', 'b', 'c'))",
            "(m.options(1), m.options(1, x=2, y=3))",
            "m.both(1, 2, 3, k=4)",
            // the body sees a real tuple and a real dict
            "m.tupled(1, 2)",
            "sorted(m.mapped(b=2, a=1).items())",
            "type(m.tupled()).__name__",
            "type(m.mapped()).__name__",
            // a keyword that names no parameter goes to `**kwargs`, or raises
            "[str(_capture_kw(m.named, ('a',), {'zzz': 1}))]",
            "[str(_capture(m.options))]",
            "[m.total(*a) for a in ([1], [1, 2])]",
            // and a *compiled* caller packs the tuple and the dict itself
            "m.calls_total(1, 2)",
            "m.calls_options(5)",
            "m.calls_both(1)",
            "m.calls_none()",
        ],
    );
}

#[test]
fn a_variadic_argument_does_not_leak() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_varleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def joined(*parts: str) -> int:
    total = 0
    for p in parts:
        total = total + len(p)
    return total

def mapped(**named: object) -> int:
    return len(named)
";
    if build_source(
        source,
        "by_diff_varleak",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // the wrapper *builds* the tuple and the dict, so it owns them and has to release
    // them — and the elements it put in are borrowed from the caller
    let out = run(
        &python,
        &dir,
        "import gc, sys, by_diff_varleak as m\n\
         part = 'x' * 40\n\
         before = sys.getrefcount(part)\n\
         for _ in range(20000):\n\
        \x20   m.joined(part, part)\n\
        \x20   m.mapped(a=part)\n\
         print('refs', 'stable' if sys.getrefcount(part) == before else 'leaked')\n\
         gc.collect()\n\
         objects = len(gc.get_objects())\n\
         for _ in range(5000):\n\
        \x20   m.joined(part)\n\
        \x20   m.mapped(a=part, b=part)\n\
         gc.collect()\n\
         print('objects', 'stable' if len(gc.get_objects()) <= objects else 'leaked')\n",
    );
    assert_eq!(out, "refs stable\nobjects stable");
}

#[test]
fn a_decorated_method_agrees() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    agree_with_declines(
        "methoddeco",
        "\
def doubling(fn: object) -> object:
    def wrapper(self: object) -> object:
        return fn(self) * 2
    return wrapper

data class Point:
    x: int
    y: int

    @property
    def total(self) -> int:
        return self.x + self.y

    @doubling
    def raw(self) -> int:
        return self.x
",
        &[
            // a property is a descriptor on the type, reached without a call
            "m.Point(3, 4).total",
            "type(m.Point.total).__name__",
            // and a user decorator wraps the native method
            "m.Point(3, 4).raw()",
        ],
    );
}

/// a decorator that *mutates* what it is handed, next to one that wraps
///
/// `abc.abstractmethod` writes `__isabstractmethod__` onto its argument and hands the
/// same object back, so it is the whole class of decorator a compiled method has to
/// stay writable for — a method descriptor takes no attributes at all. and the two
/// class constructions have to be covered separately: `Plain` is built from a spec, so
/// the decorators reach methods this module compiled, while `Shape`'s metaclass rules
/// a spec out and its construction falls back to the interpreted definition, which
/// already carries them
#[test]
fn a_mutating_method_decorator_agrees() {
    agree_python(
        "mutatingdeco",
        "\
from abc import ABC, abstractmethod


def doubling(fn: object) -> object:
    def wrapper(self: object) -> object:
        return fn(self) * 2
    return wrapper


def tagging(fn: object) -> object:
    def wrapper(self: object) -> object:
        return str(fn(self)) + '!'
    return wrapper


class Plain:
    @abstractmethod
    def area(self) -> int:
        return 3

    @doubling
    def raw(self) -> int:
        return 5

    @tagging
    @doubling
    def stacked(self) -> int:
        return 4

    @property
    def total(self) -> int:
        return 6


class Shape(ABC):
    @abstractmethod
    def area(self) -> int:
        return 7

    @doubling
    def raw(self) -> int:
        return 11
",
        &[
            // the mutating decorator ran, and the method it marked still calls
            "m.Plain.area.__isabstractmethod__",
            "m.Plain().area()",
            // the name a decorator reads off a function is still the method's own
            "m.Plain.area.__name__",
            // a wrapping decorator wraps once, not once per construction
            "m.Plain().raw()",
            // and the innermost is applied first, so `tagging` sees the doubled value
            "m.Plain().stacked()",
            "m.Plain().total",
            "type(m.Plain.total).__name__",
            // an `ABCMeta` base reads the mark, so the abstract set is the same set
            "sorted(m.Shape.__abstractmethods__)",
            "m.Shape.area.__isabstractmethod__",
            "type('S', (m.Shape,), {'area': lambda self: 13})().raw()",
            "type('S', (m.Shape,), {'area': lambda self: 13})().area()",
        ],
    );
}

#[test]
fn lambdas_agree() {
    agree_with_declines(
        "lambdas",
        "\
def adder(n: int) -> ((int) -> int):
    return lambda x: x + n

def twice(n: int) -> int:
    f = lambda x: x * 2
    return f(f(n))

def picked(flag: bool) -> ((int) -> int):
    if flag:
        return lambda x: x + 1
    return lambda x: x - 1

def counter() -> (() -> int):
    n = 0
    f = lambda: n
    n = 1
    return f

def each(values: list[int]) -> list[object]:
    out = []
    for v in values:
        out.append(lambda: v)
    return out
",
        &[
            "m.adder(3)(4)",
            "m.twice(5)",
            "(m.picked(True)(10), m.picked(False)(10))",
            // the lambda closes over the *variable*, so it sees the later write
            "m.counter()()",
            // and every lambda a loop makes shares one cell, as in python
            "[f() for f in m.each([1, 2, 3])]",
            "sorted([3, 1, 2], key=m.adder(0))",
            "[m.adder(a)(a) for a in (0, -3, 10 ** 20)]",
        ],
    );
}

#[test]
fn a_loop_over_native_instances_agrees() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    // narrowing the element to an emitted class is a checked unbox like any other —
    // asking the *free* narrowable rather than the one that knows this module's
    // layouts declined the whole loop
    agree(
        "loopnative",
        "\
frozen data class Vec2:
    x: float
    y: float

data class Loose:
    n: int

def total(vs: list[Vec2]) -> float:
    out = 0.0
    for v in vs:
        out = out + v.x * v.x + v.y * v.y
    return out

def summed(xs: list[Loose]) -> int:
    return sum([x.n for x in xs])

def wrong_element(vs: list[Vec2]) -> float:
    out = 0.0
    for v in vs:
        out = out + v.x
    return out
",
        &[
            "round(m.total([m.Vec2(1.0, 2.0), m.Vec2(3.0, 4.0)]), 1)",
            "m.summed([m.Loose(1), m.Loose(2)])",
            "round(m.total([]), 1)",
            // and the narrowing is a real check: a list that lies raises
            "[type(e).__name__ for e in [_capture(m.wrong_element, ['not a vec'])]]",
        ],
    );
}

/// the process's own peak footprint, which is the only thing that sees a buffer
/// that is not a `PyObject`
///
/// windows has no `resource` module; `GetProcessMemoryInfo` answers the same
/// question there. the structure is spelled by field *width* rather than by C
/// name because `ctypes.c_ulong` is four bytes on windows and eight elsewhere —
/// a `DWORD` written as `c_ulong` would move every offset behind it — and its
/// size is checked on every platform, so the branch only windows takes is still
/// held to its layout here
const PEAK_FOOTPRINT: &str = "\
import ctypes

class _Counters(ctypes.Structure):
    _fields_ = [(\"cb\", ctypes.c_uint32), (\"page_faults\", ctypes.c_uint32)] + [
        (name, ctypes.c_size_t)
        for name in (\"peak_working_set\", \"working_set\",
                     \"quota_peak_paged_pool\", \"quota_paged_pool\",
                     \"quota_peak_non_paged_pool\", \"quota_non_paged_pool\",
                     \"pagefile\", \"peak_pagefile\")]

assert ctypes.sizeof(_Counters) == 8 + 8 * ctypes.sizeof(ctypes.c_size_t)

def _windows_peak():
    kernel32 = ctypes.windll.kernel32
    kernel32.GetCurrentProcess.restype = ctypes.c_void_p
    kernel32.K32GetProcessMemoryInfo.restype = ctypes.c_int32
    kernel32.K32GetProcessMemoryInfo.argtypes = (
        ctypes.c_void_p, ctypes.POINTER(_Counters), ctypes.c_uint32)
    counters = _Counters()
    counters.cb = ctypes.sizeof(counters)
    if not kernel32.K32GetProcessMemoryInfo(
            kernel32.GetCurrentProcess(), ctypes.byref(counters), counters.cb):
        raise ctypes.WinError()
    return counters.peak_working_set

try:
    import resource
except ImportError:
    _peak = _windows_peak
else:
    def _peak():
        return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
";

#[test]
fn an_unboxed_array_does_not_leak_its_buffer() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_arrayleak");
    let _ = std::fs::remove_dir_all(&dir);
    // the buffer is `PyMem_Malloc`, not a `PyObject` — so a leak of one is invisible
    // to `gc.get_objects()` and to a refcount check. the process's own footprint is
    // the only thing that sees it
    let source = "\
def churn(rounds: int) -> float:
    out = 0.0
    r = 0
    while r < rounds:
        xs = [1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, 8.5]
        out = out + xs[0]
        r = r + 1
    return out
";
    if build_source(
        source,
        "by_diff_arrayleak",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        &format!(
            "import by_diff_arrayleak as m\n{PEAK_FOOTPRINT}\
         m.churn(1000)\n\
         before = _peak()\n\
         m.churn(400000)\n\
         after = _peak()\n\
         print(after - before < before // 2 or after - before < 4_000_000)\n"
        ),
    );
    assert_eq!(out, "True", "the array buffer leaks");
}

#[test]
fn container_fast_paths_agree_and_respect_subclasses() {
    // the fast paths are guarded on the *exact* type: a subclass may override
    // `__getitem__` or `__missing__`, and a fast path that ignored that would be a
    // wrong answer rather than a fast one
    agree(
        "containerprim",
        "\
def indexed(xs: list[int], i: int) -> int:
    return xs[i]

def written(xs: list[int], i: int, v: int) -> str:
    xs[i] = v
    return str(xs)

def looked_up(d: dict[str, int], k: str) -> int:
    return d[k]

def sized(xs: object) -> int:
    return len(xs)

def tupled(t: tuple[int, int], i: int) -> object:
    return t[i]
",
        &[
            "m.indexed([1, 2, 3], 1)",
            "m.indexed([1, 2, 3], -1)",
            "m.written([1, 2, 3], 0, 9)",
            "m.looked_up({'a': 1}, 'a')",
            "m.sized([1, 2, 3])",
            "m.sized({'a': 1})",
            "m.sized('abc')",
            "m.tupled((4, 5), 1)",
            // every error is python's own, message and class included
            "[(type(e).__name__, str(e)) for e in [_capture(m.indexed, [1], 5)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.written, [1], 5, 0)]]",
            "[(type(e).__name__, repr(str(e))) for e in [_capture(m.looked_up, {}, 'zz')]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.tupled, (1,), 9)]]",
            // a *subclass* overriding the protocol must still be honoured
            "m.indexed(type('L', (list,), {'__getitem__': lambda s, i: 99})([1, 2]), 0)",
            "m.looked_up(type('D', (dict,), {'__missing__': lambda s, k: 77})(), 'nope')",
            "m.sized(type('L', (list,), {'__len__': lambda s: 42})([1]))",
        ],
    );
}

#[test]
fn a_comprehension_over_range_agrees() {
    // the statement form has had a counting loop all along; the comprehension drove
    // the iteration protocol instead — a `range` object, an iterator, a `next` and
    // an unbox *per element*, for a loop whose bounds are right there
    agree_with_declines(
        "comprange",
        "\
def squares(n: int) -> object:
    return [i * i for i in range(n)]

def filtered(n: int) -> object:
    return [i for i in range(n) if i % 3 == 0]

def from_two(n: int) -> object:
    return [i for i in range(2, n)]

def nested(n: int) -> object:
    return [i * j for i in range(n) for j in range(n)]

def buffered(n: int) -> float:
    xs = [i * 1.5 for i in range(n)]
    out = 0.0
    for x in xs:
        out = out + x
    return out

def empty(n: int) -> float:
    xs = [i * 1.5 for i in range(0)]
    out = 0.0
    for x in xs:
        out = out + x
    return out
",
        &[
            "m.squares(5)",
            "m.filtered(10)",
            "m.from_two(6)",
            "m.nested(3)",
            "m.squares(0)",
            "m.from_two(1)",
            "m.buffered(5)",
            "m.empty(0)",
            // a negative bound is an empty range, not a backwards one
            "m.squares(-3)",
        ],
    );
}

#[test]
fn a_loop_over_an_unboxed_array_agrees() {
    // the shape the whole representation exists for: an `i64` counter, no iterator
    // object, no null test per step, and no bounds check — the counter is the
    // lowering's own, so it is in range by construction
    agree_with_declines(
        "arrayloop",
        "\
def total(n: int) -> float:
    xs = [1.5, 2.5, 3.5, 4.5]
    out = 0.0
    for x in xs:
        out = out + x
    return out

def empty(n: int) -> float:
    xs = [1.0]
    out = 0.0
    for x in xs:
        out = out + x
    return out

def broken(n: int) -> float:
    xs = [1.0, 2.0, 3.0]
    out = 0.0
    for x in xs:
        if x > 1.5:
            break
        out = out + x
    else:
        out = -1.0
    return out

def skipped(n: int) -> float:
    xs = [1.0, 2.0, 3.0]
    out = 0.0
    for x in xs:
        if x > 1.5:
            continue
        out = out + x
    return out

def exhausted(n: int) -> float:
    xs = [1.0, 2.0]
    out = 0.0
    for x in xs:
        out = out + x
    else:
        out = out + 100.0
    return out

def flags(n: int) -> int:
    bs = [True, False, True, True]
    total = 0
    for b in bs:
        if b:
            total = total + 1
    return total
",
        &[
            "m.total(0)",
            "m.empty(0)",
            // `break` skips the `else`, `continue` does not
            "m.broken(0)",
            "m.skipped(0)",
            "m.exhausted(0)",
            "m.flags(0)",
        ],
    );
}

#[test]
fn a_list_that_escapes_keeps_being_a_list() {
    // the buffer is an optimization, not a restriction: a name that leaves the
    // function never earns one in the first place, so it compiles exactly as it did
    // before the representation existed
    agree(
        "bufferescape",
        "\
def returned(n: int) -> object:
    xs = [1.0, 2.0]
    return xs

def passed(n: int) -> int:
    xs = [1.0, 2.0]
    return len(sorted(xs))

def stored(n: int) -> object:
    xs = [1.0, 2.0]
    return [xs, xs]

def kept(n: int) -> float:
    xs = [1.0, 2.0]
    return xs[0] + xs[1]

def looped(n: int) -> float:
    xs = [1.0, 2.0]
    out = 0.0
    for x in xs:
        out = out + x
    return out
",
        &[
            // these keep a real list, and a real list is what comes back
            "m.returned(0)",
            "type(m.returned(0)).__name__",
            "m.passed(0)",
            "m.stored(0)",
            // and these earn the buffer, invisibly
            "m.kept(0)",
            "m.looped(0)",
        ],
    );
}

#[test]
fn a_from_import_inside_a_body_agrees() {
    // plain python, so the interpreted leg is the source itself: the transpiler's
    // lazy-import polyfill resolves `from pkg import submodule` wrongly, which would
    // otherwise break the leg this is measured against rather than the compiled one
    agree_python(
        "fromimport",
        "\
def one(n: int) -> str:
    from math import sqrt
    return str(round(sqrt(n), 3))

def several(s: str) -> str:
    from os.path import basename, dirname
    return dirname(s) + '|' + basename(s)

def aliased(n: int) -> str:
    from math import sqrt as root
    return str(round(root(n), 3))

# `urllib.parse` is not an attribute of `urllib` until something imports it, so
# this is the fromlist doing its job rather than an attribute read
def submodule(s: str) -> str:
    from urllib import parse
    return parse.quote(s)

def dotted(s: str) -> str:
    from urllib.parse import quote
    return quote(s)

# a name the module does not have is an `ImportError`, not the `AttributeError` a
# plain attribute read would give — a guarded lazy import rests on the difference
def absent() -> object:
    from os import definitely_not_a_real_name
    return definitely_not_a_real_name

def guarded() -> str:
    try:
        from os import definitely_not_a_real_name
    except ImportError:
        return 'caught'
    return 'no'
",
        &[
            "m.one(2)",
            "m.several('/a/b/c.txt')",
            "m.aliased(2)",
            "m.submodule('a b/c')",
            "m.dotted('a b/c')",
            "m.guarded()",
            // the type, the message and every attribute an `except` clause reads
            "(lambda e: (type(e).__name__, str(e), e.name, e.path, e.name_from))(_capture(m.absent))",
            // a thousand imports must not accumulate references to the module. a
            // leaked module is one object with a climbing refcount, which no
            // object-count check can see
            "_repeated(lambda: m.one(2), 1000)",
            "_refdelta('math', lambda: m.one(2), 500)",
            "_refdelta('urllib.parse', lambda: m.submodule('a b'), 500)",
        ],
    );
}

#[test]
fn the_remaining_statement_and_expression_forms_agree() {
    agree(
        "coverage",
        "\
def imports(n: int) -> str:
    import math
    return str(round(math.sqrt(n), 3))

def aliased(n: int) -> str:
    import os.path as p
    return str(p.basename('/a/b'))

def del_key(d: dict[str, int]) -> str:
    del d['a']
    return str(sorted(d.items()))

def del_item(xs: list[int]) -> str:
    del xs[0]
    return str(xs)

def del_attr(o: object) -> str:
    del o.gone
    return str(hasattr(o, 'gone'))

def ellipsis(n: int) -> object:
    return ...

def negated(s: object) -> object:
    return -s

def inverted(s: object) -> object:
    return ~s

def sliced(xs: list[int]) -> str:
    return str(xs[1:3]) + str(xs[::2]) + str(xs[2:]) + str(xs[:2]) + str(xs[::-1])

def slice_assigned(xs: list[int]) -> str:
    xs[1:3] = [9, 9, 9]
    return str(xs)

def slice_deleted(xs: list[int]) -> str:
    del xs[1:3]
    return str(xs)

def walrus(xs: list[int]) -> str:
    if (n := len(xs)) > 2:
        return 'big ' + str(n)
    return 'small ' + str(n)

def genexp(xs: list[int]) -> str:
    return str(sum(x * 2 for x in xs)) + str(any(x > 2 for x in xs)) + str(max(x for x in xs))

def declared_global(n: int) -> int:
    global _counter
    return n
",
        &[
            "m.imports(16)",
            "m.aliased(0)",
            "m.del_key({'a': 1, 'b': 2})",
            "m.del_item([1, 2, 3])",
            "m.ellipsis(0)",
            "m.sliced([1, 2, 3, 4])",
            "m.slice_assigned([1, 2, 3, 4])",
            "m.slice_deleted([1, 2, 3, 4])",
            "m.walrus([1, 2, 3])",
            "m.walrus([1])",
            "m.genexp([1, 2, 3])",
            "m.declared_global(4)",
            // the protocol forms, on a type that answers them
            "m.negated(type('N', (), {'__neg__': lambda s: 'neg'})())",
            "m.inverted(type('N', (), {'__invert__': lambda s: 'inv'})())",
            "m.del_attr(type('A', (), {})() if False else __import__('types').SimpleNamespace(gone=1))",
            // and the errors each raises
            "[(type(e).__name__, str(e)) for e in [_capture(m.del_key, {})]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.del_item, [])]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.negated, object())]]",
        ],
    );
}

#[test]
fn a_bad_first_argument_raises_rather_than_crashing() {
    // the wrapper releases every argument local on the error path, so one whose
    // declaration a `goto` skipped would be released while indeterminate. a wrong
    // type in the *first* parameter is the reachable case: it jumps over the rest.
    //
    // only the exception *type* is compared: the boundary rejects a bad argument
    // where the interpreted leg gets as far as the operation that uses it, so the
    // two agree that it is a `TypeError` and not on where it was raised
    agree(
        "badarg",
        "\
def two(a: int, b: str) -> int:
    return a + len(b)

def three(a: int, b: str, c: list[int]) -> int:
    return a + len(b) + len(c)
",
        &[
            "type(_capture(m.two, 'x', 'y')).__name__",
            "type(_capture(m.two, 1, 2)).__name__",
            "type(_capture(m.three, 'x', 'y', [1])).__name__",
            "type(_capture(m.three, 1, 'y', 'z')).__name__",
            "m.two(1, 'yy')",
            "m.three(1, 'yy', [1, 2])",
        ],
    );
}

#[test]
fn a_plain_python_loop_closure_shares_its_binding() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    // python's loop binding is shared by every iteration, so all three closures
    // see the last value. basedpython's is per-iteration. the compiled half has to
    // follow the *source* language, not a flag — a `.py` fallback is python
    let source = "\
def counters() -> list[object]:
    out = []
    for i in range(3):
        def get() -> int:
            return i
        out.append(get)
    return [f() for f in out]
";
    let dir = std::env::temp_dir().join("by_diff_pyloop");
    let interpreted = std::env::temp_dir().join("by_diff_pyloop_i");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&interpreted);
    std::fs::create_dir_all(&interpreted).expect("the directory is created");
    std::fs::write(interpreted.join("by_diff_pyloop.py"), source).expect("written");

    let options = Options {
        language: by_irbuild::Language::Python,
        ..Options::default()
    };
    let built = match build_source(source, "by_diff_pyloop", &toolchain, &dir, &options) {
        Ok(built) => built,
        Err(error) => {
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "{:?}", built.declined);

    let body = "import by_diff_pyloop as m\nprint(m.counters())\n";
    let compiled = run(&python, &dir, body);
    assert_eq!(compiled, run(&python, &interpreted, body));
    assert!(compiled.contains("[2, 2, 2]"), "{compiled}");
}

#[test]
fn dunder_methods_fill_their_type_slots() {
    // a method table cannot fill a slot: `repr(x)` reads `tp_repr` and never looks
    // the name up. so each of these has to work *both* ways — through the slot and
    // as an ordinary method
    agree_python(
        "dunders",
        "\
class Bag:
    def __init__(self, items: list[int]) -> None:
        self.items = items

    def __repr__(self) -> str:
        return 'Bag(' + str(self.items) + ')'

    def __str__(self) -> str:
        return 'bag of ' + str(len(self.items))

    def __len__(self) -> int:
        return len(self.items)

    def __bool__(self) -> bool:
        return len(self.items) > 1
",
        &[
            "repr(m.Bag([1, 2, 3]))",
            "str(m.Bag([1, 2, 3]))",
            "len(m.Bag([1, 2, 3]))",
            "bool(m.Bag([1, 2, 3]))",
            "bool(m.Bag([1]))",
            "len(m.Bag([]))",
            "bool(m.Bag([]))",
            "f'{m.Bag([1])}'",
            "'{}'.format(m.Bag([1, 2]))",
            "m.Bag([1, 2]).__repr__()",
            "m.Bag([1, 2]).__len__()",
            "[b for b in [m.Bag([]), m.Bag([1, 2])] if b]",
        ],
    );
}

#[test]
fn calling_a_resumable_frame_hands_back_the_state_object() {
    // whatever the annotation says the body produces, the *call* gives the state
    // object and the iteration or the `await` turns it back. there are three
    // signature maps — module-level, methods, nested — and this has to hold in all of
    // them, or the caller assigns a `PyObject *` into the annotation's representation
    agree_python(
        "resumablecall",
        "\
from typing import Any


def flags(n):
    i = 0
    while i < n:
        yield i > 0
        i = i + 1


async def doubled(n: int) -> int:
    return n * 2


class Holder:
    def __init__(self, n: int) -> None:
        self.n = n

    def counted(self):
        i = 0
        while i < self.n:
            yield i == 0
            i = i + 1

    def total(self) -> int:
        out = 0
        for seen in self.counted():
            if seen:
                out = out + 1
        return out


def nested(n: int) -> int:
    def inner():
        i = 0
        while i < n:
            yield i < 1
            i = i + 1
    out = 0
    for seen in inner():
        if seen:
            out = out + 1
    return out


def drained(n: int) -> int:
    out = 0
    for seen in flags(n):
        if seen:
            out = out + 1
    return out
",
        &[
            "list(m.flags(3))",
            "m.drained(3)",
            "m.drained(0)",
            "_run(m.doubled(5))",
            "m.Holder(4).total()",
            "list(m.Holder(2).counted())",
            "m.nested(3)",
            "m.nested(0)",
        ],
    );
}

#[test]
fn a_class_of_only_methods_compiles() {
    // no `__init__` is not the same as no *layout*: a class of methods has an empty
    // one, which is as representable as any other. `object.__init__` is what rejects a
    // call with arguments, and it names the class rather than a method it does not have
    //
    // `Empty` is where the class is a *static* type, which nothing can build on, so it
    // fills the pair of slots itself and raises through them. `Consts` is a base, so it
    // is a heap type and leaves both to `object` — where the message carries the module,
    // because a type built from a spec keeps its module in `tp_name`. that difference is
    // the price of not publishing an `__init__` the source never wrote: one installed
    // anyway stands between everything built on the class and the `object.__init__` it
    // should have reached
    agree_python(
        "onlymethods",
        "\
class Helpers:
    def doubled(self, n: int) -> int:
        return n * 2

    def summed(self, n: int) -> int:
        total = 0
        i = 0
        while i < n:
            total = total + i
            i = i + 1
        return total


class Consts:
    SCALE = 3

    def scaled(self, n: int) -> int:
        return n * Consts.SCALE


class Empty:
    pass


class Sub(Consts):
    def twice(self, n: int) -> int:
        return self.scaled(n) * 2
",
        &[
            "m.Helpers().doubled(21)",
            "m.Helpers().summed(5)",
            "m.Consts().scaled(4)",
            "m.Consts.SCALE",
            "m.Sub().twice(4)",
            "isinstance(m.Sub(), m.Consts)",
            "type(m.Empty()).__name__",
            "type(_capture(lambda: m.Consts(1))).__name__",
            "str(_capture(lambda: m.Consts(1))).rsplit('.', 1)[-1]",
            "str(_capture(lambda: m.Empty(1, 2)))",
            // and a class anything can be built on publishes no `__init__` the source
            // did not write. a static type still does, because it has to fill the slot
            // pair itself — but nothing can be built on one, so no mro reaches it
            "'__init__' in vars(m.Consts)",
            "[m.Helpers().doubled(n) for n in [0, 1, 2]]",
        ],
    );
}

#[test]
fn an_explicit_object_base_is_no_base_at_all() {
    // `class C(object)` is what `class C:` already is, so it lays out and compiles the
    // same way — but only when `object` really is the builtin
    agree_python(
        "objectbase",
        "\
class Plain(object):
    def __init__(self, n: int) -> None:
        self.n = n

    def doubled(self) -> int:
        return self.n * 2


class Derived(Plain):
    def __init__(self, n: int) -> None:
        self.n = n
        self.extra = 1

    def total(self) -> int:
        return self.doubled() + self.extra
",
        &[
            "m.Plain(3).doubled()",
            "m.Derived(4).total()",
            "isinstance(m.Derived(1), m.Plain)",
            "[m.Plain(n).doubled() for n in [0, 1, 2]]",
            "m.Derived(2).n",
        ],
    );
}

#[test]
fn a_module_that_binds_object_itself_keeps_its_own() {
    // the name is resolved, not matched: a class of this module's own called `object`
    // is the base, and taking the builtin instead would give the subclass the wrong one
    agree_python(
        "shadowedobject",
        "\
class object:
    def __init__(self, n: int) -> None:
        self.n = n

    def base(self) -> int:
        return self.n


class Shadowed(object):
    def __init__(self, n: int) -> None:
        self.n = n
        self.extra = 1

    def total(self) -> int:
        return self.base() + self.extra
",
        &[
            "m.Shadowed(3).total()",
            "m.object(5).base()",
            "isinstance(m.Shadowed(1), m.object)",
        ],
    );
}

#[test]
fn a_parameter_defaulting_to_none_is_not_a_none_place() {
    // `def f(x=None)` infers `Unknown | None`, and the gradual member is assignable to
    // whatever is asked — so the union tested as assignable to `None` and the
    // parameter got the `None` representation, which nothing else could be stored in.
    // it is one of python's most common shapes
    agree_python(
        "nonedefault",
        "\
def opened(f, mode=None):
    if mode is None:
        mode = 'rb'
    return mode


def counted(items, start=None):
    if start is None:
        start = 0
    return start + len(items)


def collected(seed=None):
    if seed is None:
        seed = []
    seed.append(1)
    return seed
",
        &[
            "m.opened('x')",
            "m.opened('x', 'wb')",
            "m.counted([1, 2, 3])",
            "m.counted([1, 2, 3], 10)",
            "m.collected()",
            "m.collected([9])",
            "[m.opened('f', mode) for mode in [None, 'rb', 'wb']]",
        ],
    );
}

#[test]
fn an_unannotated_parameter_with_a_default_is_not_a_float() {
    // a parameter with no annotation infers `Unknown | <the default's type>`, and a
    // *gradual* element is assignable both ways to everything — so one of them used to
    // answer for both halves of the promotion test, and any `Unknown | T` read as
    // `int | float`. that gave the parameter a `double` representation and a string
    // default, which the c compiler rejected outright
    agree_python(
        "graduallead",
        "\
def failed(values, errmsg='negative value'):
    for x in values:
        if x < 0:
            raise ValueError(errmsg)
        yield x


def tagged(n, tag='t'):
    return tag + str(n)


def numeric(x, scale=2.0):
    return x * scale
",
        &[
            "list(m.failed([1, 2], 'bad'))",
            "list(m.failed([1, 2]))",
            "[(type(e).__name__, str(e)) for e in [_capture(lambda: list(m.failed([1, -1])))]]",
            "m.tagged(3)",
            "m.tagged(3, 'x')",
            "m.numeric(3)",
            "m.numeric(3, 4.0)",
            "m.numeric(2.5)",
        ],
    );
}

#[test]
fn a_gradual_value_narrowed_on_both_arms_is_still_an_object() {
    // narrowing a gradual value gives an *intersection* holding it, so a conditional
    // over both arms is a union of two of those and the gradual part sits one level
    // down. it widens the whole type all the same: `(Unknown & C) | (Unknown & ~C)` is
    // assignable to `None`, and reading that as a proof gave the result a `None`
    // representation whose boundary rejected every value that was not one — which is
    // how `enum.py` compiled and then could not be imported
    agree_python(
        "narrowedgradual",
        "\
def unwrap(value):
    return value.__func__ if isinstance(value, staticmethod) else value


def widest(value):
    return value if isinstance(value, int) else value
",
        &[
            "m.unwrap(3)",
            "m.unwrap('a')",
            "m.unwrap(len).__name__",
            "m.unwrap(staticmethod(len)).__name__",
            "[m.widest(x) for x in (0, 'x', 2.5, None)]",
            "m.widest(len).__name__",
        ],
    );
}

#[test]
fn a_class_that_stores_through_setattr_keeps_its_interpreted_definition() {
    // an emitted instance is its layout and nothing else — there is no `__dict__`
    // behind it — so an attribute `setattr` names as a value has nowhere to go. the
    // class has to stay interpreted, and the compiled module has to use *that* class:
    // laying it out anyway is how `enum.py` reached `no __dict__ for setting new
    // attributes` and could not be imported
    agree_python_with_declines(
        "setattrstore",
        "\
class Registry:
    def __init__(self):
        self.count = 0

    def install(self, name, fn):
        setattr(self, name, fn)
        self.count = self.count + 1

    def run(self, name):
        return getattr(self, name)()


def build():
    r = Registry()
    r.install('greet', lambda: 'hello')
    return r
",
        &[
            "m.build().run('greet')",
            "m.build().count",
            "[m.build().greet(), m.Registry().count]",
        ],
    );
}

#[test]
fn a_subscript_by_a_string_key_agrees() {
    // a `str` widened to an `object` for a lookup must stay widened: substituting the
    // source back handed `GetItem` a `str` register, which is a `PyObject *` in c and
    // so compiled — but is a different representation, and the verifier said so
    agree_python(
        "strkey",
        "\
def read(d: dict[str, int], k: str) -> int:
    return d[k]


def literal(d: dict[str, int]) -> int:
    return d['k']


def written(d: dict[str, int], k: str, v: int) -> object:
    d[k] = v
    return d


def missing(d: dict[str, int], k: str) -> object:
    return d.get(k)
",
        &[
            "m.read({'k': 1}, 'k')",
            "m.literal({'k': 2})",
            "m.written({}, 'a', 1)",
            "m.missing({'k': 3}, 'k')",
            "m.missing({'k': 3}, 'nope')",
            "str(_capture(lambda: m.read({}, 'absent')))",
            "[m.read({'a': 1, 'b': 2}, k) for k in ['a', 'b']]",
            // the write goes straight to `PyDict_SetItem` for an *exact* dict, so a
            // subclass that overrides `__setitem__` is what says the guard holds
            "m.written(type('D', (dict,), \
             {'__setitem__': lambda s, k, v: dict.__setitem__(s, k, v * 10)})(), 'a', 5)",
        ],
    );
}

#[test]
fn a_class_on_a_base_outside_the_module_agrees() {
    // the type is built on whatever the name resolves to at module init, so the base
    // has to *be* the real one: `raise` on a class that silently got `object` instead
    // is a `TypeError`, which is how the two earlier attempts at this were caught
    //
    // the class declares no layout of its own — `basicsize` 0, and the base allocates
    // and frees — and that has to hold **transitively**, or a subclass of a subclass
    // declares a size smaller than its base and python rejects the type outright
    agree(
        "external_base",
        "\
import collections

from collections import UserList


class MyError(Exception):
    def label(self) -> str:
        return \"mine\"


class Narrower(MyError):
    def label(self) -> str:
        return \"narrower\"


class Deeper(Narrower):
    pass


class Dotted(collections.OrderedDict):
    # a base written as an attribute is a name to look up and then an attribute to
    # walk, which is the only difference from a bare one
    def label(self) -> str:
        return \"dotted\"


class Listy(UserList):
    def doubled(self) -> int:
        return len(self) * 2


def raising(n: int) -> int:
    if n < 0:
        raise Narrower(\"negative\")
    return n * 2


def catching(n: int) -> str:
    try:
        return str(raising(n))
    except MyError as e:
        return \"caught \" + str(e) + \" \" + e.label()


def hierarchy() -> str:
    parts = (
        issubclass(Narrower, MyError),
        issubclass(Deeper, Exception),
        isinstance(Deeper(\"d\"), MyError),
        Deeper(\"d\").label(),
    )
    return \",\".join(str(x) for x in parts)


def uses_list() -> str:
    it = Listy([1, 2, 3])
    it.append(4)
    return str(it.doubled()) + \" \" + str(len(it)) + \" \" + str(list(it))


class WithStorage(Exception):
    # a field of its own: the base allocates the instance, so this one lives in room
    # asked for *past* it, and the class supplies the dealloc, traverse and clear that
    # the base cannot write for storage it does not know about
    def __init__(self, extra: int) -> None:
        self.extra = extra

    def bumped(self) -> int:
        return self.extra + 1


class TwoBases(ValueError, IndexError):
    # more than one base, none of them ours: python works out the mro and which of them
    # owns the layout, and this class declares none of its own
    def label(self) -> str:
        return \"two\"


def caught_by_either(which: int) -> str:
    try:
        raise TwoBases(\"both\")
    except IndexError as e:
        if which == 0:
            return \"index \" + str(e)
        raise
",
        &[
            "[m.catching(n) for n in (3, -1)]",
            "m.hierarchy()",
            "m.uses_list()",
            "[type(e).__name__ for e in [_capture(m.raising, -2)]]",
            "m.WithStorage(5).extra",
            "m.WithStorage(5).bumped()",
            "[type(e).__name__ for e in [_capture(m.WithStorage, 7)]]",
            "(m.WithStorage(1).args, str(m.WithStorage(1)))",
            "(m.Deeper.__mro__[1].__name__, m.Listy.__mro__[1].__name__)",
            "m.caught_by_either(0)",
            "[c.__name__ for c in m.TwoBases.__mro__[1:3]]",
            "(issubclass(m.TwoBases, ValueError), issubclass(m.TwoBases, IndexError))",
            "m.TwoBases('x').label()",
            "(m.Dotted().label(), m.Dotted.__mro__[1].__name__)",
        ],
    );
}

#[test]
fn a_base_this_module_emits_beside_one_it_does_not_agrees() {
    // a class may hold both kinds of base at once. it takes its whole layout from
    // outside — the base of ours in the list lays nothing out, so it asks for no room —
    // and python works out the mro and which of the bases owns the instance.
    //
    // the outside base may own a real one: `dict`, `int` and `Exception` each decide the
    // instance, and getting that wrong writes this class's idea of a layout over theirs
    agree_python(
        "mixed_bases",
        "\
import codecs


class Ours:
    def side(self) -> str:
        return \"ours\"

    def reset(self) -> object:
        # `object`, because the outside base's `reset` answers `None` and a narrower
        # annotation would make the difference a representation error rather than a value
        return \"ours reset\"


class OursFirst(Ours, codecs.Codec):
    def label(self) -> str:
        return \"first\"


class OutsideFirst(codecs.StreamWriter, Ours):
    # the outside base comes first, so *its* `reset` is what the mro reaches and ours is
    # shadowed — a direct call on a receiver typed as `Ours` would answer with ours
    def label(self) -> str:
        return \"outside\"


class AsDict(dict, Ours):
    def label(self) -> str:
        return \"dict\"


class AsInt(int, Ours):
    def label(self) -> str:
        return \"int\"


class OurError(Exception):
    def which(self) -> str:
        return \"ourerror\"


class Diamond(OurError, ValueError):
    # the shared ancestor is outside: `Exception` is above both legs, and the mro has to
    # place it once and after both
    def label(self) -> str:
        return \"diamond\"


class Under(OursFirst):
    # a class of this module's own built on the mixture: the layout chain runs through
    # a class that has none, so this one declares none either
    def label(self) -> str:
        return \"under\"


def through_the_base(o: Ours) -> str:
    return o.side()


def resetting(o: Ours) -> object:
    return o.reset()


def exactly(which: int) -> str:
    if which == 0:
        return OursFirst().side()
    return OutsideFirst(None).side()
",
        &[
            "[c.__name__ for c in m.OursFirst.__mro__]",
            "[c.__name__ for c in m.OutsideFirst.__mro__]",
            "[c.__name__ for c in m.Diamond.__mro__]",
            "(m.OursFirst.__base__.__name__, m.AsDict.__base__.__name__, m.AsInt.__base__.__name__)",
            "(m.OursFirst().side(), m.OursFirst().label())",
            "(m.OutsideFirst(None).side(), m.OutsideFirst(None).label())",
            "[m.through_the_base(o) for o in (m.Ours(), m.OursFirst(), m.OutsideFirst(None), m.Under())]",
            "[m.exactly(n) for n in (0, 1)]",
            "[m.resetting(o) for o in (m.Ours(), m.OursFirst(), m.OutsideFirst(None))]",
            "([c.__name__ for c in m.Under.__mro__], m.Under().label(), m.Under().side())",
            // the outside base still owns the instance it always owned
            "(sorted(m.AsDict(a=1, b=2).items()), m.AsDict().label())",
            "(int(m.AsInt(7)) + 1, m.AsInt(7).label(), m.AsInt(7).side())",
            "(m.Diamond('boom').args, str(m.Diamond('boom')), m.Diamond('x').which(), m.Diamond('x').label())",
            "(isinstance(m.Diamond('x'), ValueError), isinstance(m.Diamond('x'), m.OurError))",
            // an instance of the mixture carries whatever `__dict__` the outside base
            // brought: a class of ours alone has none, and losing it here silently
            // turned every attribute read on one into a walk off the end of the object
            "(lambda o: (setattr(o, 'kept', 3), o.kept, o.__dict__))(m.OursFirst())",
            // and a python subclass of the result, which is a class statement on it
            "(lambda C: (C().side(), C().label(), [c.__name__ for c in C.__mro__]))\
             (type('Py', (m.OursFirst,), {'label': lambda self: 'py'}))",
        ],
    );
}

#[test]
fn a_base_beside_an_outside_one_is_built_by_calling_its_metaclass() {
    // a type spec takes its whole instance shape from the one base python picks out of
    // the list. where that is a class of ours the `__dict__` an outside base needs is
    // dropped, and the type then claims a managed dict it has no room for — the first
    // attribute read on an instance walks off the object and segfaults. calling the
    // metaclass works the shape out from every base at once
    //
    // the descriptors say which build answered: a class that fell back to its
    // interpreted definition would agree on every value above and say `function`
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_mixedmeta");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
import abc
import codecs


class Ours:
    def side(self) -> str:
        return \"ours\"


class Mixed(Ours, codecs.Codec):
    def label(self) -> str:
        return \"mixed\"


class Metaclassed(Ours, abc.ABC):
    # a base whose metaclass is not `type` rules the spec out on its own, and the
    # keyword the header carries has nowhere to go in one either
    def label(self) -> str:
        return \"meta\"
";
    let built = match build_source(
        source,
        "by_diff_mixedmeta",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_mixedmeta as m\n\
         print(type(m.Mixed.label).__name__, type(m.Ours.side).__name__)\n\
         print(m.Mixed.__base__.__name__, type(m.Metaclassed).__name__)\n\
         o = m.Mixed()\n\
         o.kept = 4\n\
         print(o.side(), o.label(), o.kept, o.__dict__)\n",
    );
    assert_eq!(
        out,
        "method_descriptor method_descriptor\n\
         Ours ABCMeta\n\
         ours mixed 4 {'kept': 4}"
    );
}

#[test]
fn a_class_built_through_its_metaclass_agrees() {
    // `PyType_FromSpecWithBases` gives the type it builds `type` for a metaclass, so it
    // cannot be handed a base with any other one, and `PyType_FromMetaclass` refuses a
    // metaclass that overrides `__new__` — which `ABCMeta` does. so the type is built
    // the way python builds one: by calling the metaclass with a namespace.
    //
    // the methods go *in* that namespace rather than onto the finished type, which is
    // what fills the type slots — `type.__new__` runs the same fixup a class statement
    // does, so `__repr__` becomes `tp_repr` with no adapter of ours
    agree(
        "metaclass",
        "\
from abc import ABC, ABCMeta


class Node(ABC):
    def kind(self) -> str:
        return \"node\"

    def __repr__(self) -> str:
        return \"Node(\" + self.kind() + \")\"

    def __len__(self) -> int:
        return 3


class Deeper(Node):
    # a base of *ours* whose own base is not: the layout is nobody's, and the metaclass
    # is inherited down the chain
    def kind(self) -> str:
        return \"deeper\"


class Keyed(metaclass=ABCMeta):
    # no bases at all, only the keyword — python supplies `(object,)` itself
    def kind(self) -> str:
        return \"keyed\"


class Checked(ABC):
    # an `__init__` of its own that stores nothing: it has to reach `tp_init`, which on
    # this construction is python's dispatcher reading the namespace entry
    def __init__(self, value: int) -> None:
        if value < 0:
            raise ValueError(\"negative\")


class Equated(ABC):
    # a class defining `__eq__` and not `__hash__` is unhashable. a spec has to be told
    # that; `type.__new__` does it for itself, so this is the same answer reached two
    # different ways depending on which construction the bases allowed
    def __eq__(self, other: object) -> bool:
        return isinstance(other, Equated)


def widened(value: object) -> str:
    if isinstance(value, Node):
        return \"node \" + value.kind()
    return \"other\"
",
        &[
            // the metaclass is the one python would have worked out, not `type`
            "(type(m.Node).__name__, type(m.Keyed).__name__, type(m.Deeper).__name__)",
            // and the whole ancestry with it
            "[c.__name__ for c in m.Node.__mro__]",
            "[c.__name__ for c in m.Keyed.__mro__]",
            "[c.__name__ for c in m.Deeper.__mro__]",
            // an abstract base answers for it, which is the point of the metaclass
            "(isinstance(m.Node(), __import__('abc').ABC), issubclass(m.Node, __import__('abc').ABC))",
            "(isinstance(m.Deeper(), m.Node), issubclass(m.Deeper, m.Node))",
            // `ABCMeta.__new__` ran, and ran over *this* class's namespace
            "sorted(m.Node.__abstractmethods__)",
            // a dunder written in the class body fills its slot
            "repr(m.Node())",
            "repr(m.Deeper())",
            "len(m.Node())",
            // and the ordinary methods still bind
            "(m.Node().kind(), m.Deeper().kind(), m.Keyed().kind())",
            "[m.widened(v) for v in (m.Node(), m.Deeper(), 1)]",
            // a written `__init__` runs, and the errors around it are python's own
            "(m.Checked(1) is not None, type(_capture(m.Checked, -1)).__name__)",
            "type(_capture(m.Checked)).__name__",
            // and `__eq__` without `__hash__` still takes the hash away
            "(m.Equated() == m.Equated(), m.Equated() == 1)",
            "type(_capture(hash, m.Equated())).__name__",
            // the class reports the module it was written in, not `builtins`
            "(m.Node.__module__, m.Keyed.__module__)",
            "(m.Node.__name__, m.Node.__qualname__)",
            // an imported base reaches this module as a lazy proxy, so `__mro_entries__`
            // is what turns it into the class — and python records what was written
            "repr(m.Node.__orig_bases__)",
        ],
    );
}

#[test]
fn a_class_keyword_reaches_init_subclass_agrees() {
    // a keyword is the metaclass's business, and a type spec has nowhere to put one.
    // the base is built at runtime because a class this module *emits* would have its
    // layout laid out here, which the metaclass construction gives up
    agree(
        "classkeyword",
        "\
from abc import ABCMeta


def _tagged(cls: type, tag: str = \"none\", ready: bool = False, level: int = 0) -> None:
    cls.tag = tag
    cls.ready = ready
    cls.level = level


Base = ABCMeta(\"Base\", (), {\"__init_subclass__\": classmethod(_tagged)})


class Alpha(Base, tag=\"alpha\"):
    def kind(self) -> str:
        return \"alpha\"


class Beta(Base, tag=\"beta\", ready=True, level=2):
    # a literal keyword as well as a name: both are evaluated where a class body would
    # have evaluated them
    def kind(self) -> str:
        return \"beta\"


# the keyword's own text is arbitrary, and the C string form of it stopped at a NUL
class Gamma(Base, tag=\"a\\x00b\"):
    def kind(self) -> str:
        return \"gamma\"
",
        &[
            // the keyword reached `__init_subclass__`, which the metaclass's `__new__`
            // is what calls
            "(m.Alpha.tag, m.Alpha.ready, m.Alpha.level)",
            "(m.Beta.tag, m.Beta.ready, m.Beta.level)",
            "ascii(m.Gamma.tag)",
            "(m.Alpha().kind(), m.Beta().kind(), m.Gamma().kind())",
            // and the metaclass itself came from the base
            "(type(m.Alpha).__name__, type(m.Beta).__name__)",
            "[c.__name__ for c in m.Beta.__mro__]",
            "sorted(m.Beta.__abstractmethods__)",
        ],
    );
}

#[test]
fn which_build_answers_for_a_metaclass_class() {
    // the two `agree` tests above cannot see this on their own: a class that quietly
    // fell back to its interpreted definition answers *identically*, so both would pass
    // on a compiler that built nothing at all. this one names the build — a compiled
    // method is a descriptor, an interpreted one is a plain function.
    //
    // it is also where the restriction is pinned down. the construction through a
    // metaclass hands back a type whose instance layout is the metaclass's answer, so a
    // class appending fields to its base has nowhere to put them and stays interpreted;
    // its fieldless sibling, on the same base, does not
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_metafields");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
from abc import ABC, ABCMeta


class Fielded(ABC):
    def __init__(self, value: int) -> None:
        self.value = value

    def doubled(self) -> int:
        return self.value * 2


class Fieldless(ABC):
    def label(self) -> str:
        return \"fieldless\"


class Keyed(metaclass=ABCMeta):
    def label(self) -> str:
        return \"keyed\"
";
    let built = match build_source(
        source,
        "by_diff_metafields",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    let declined: Vec<(&str, &str)> = built
        .declined
        .iter()
        .map(|declined| (declined.name.as_str(), declined.reason.as_str()))
        .collect();
    assert_eq!(
        declined,
        vec![(
            "Fielded",
            "a class with fields of its own needs a base whose metaclass is `type`"
        )]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_metafields as m\n\
         print(m.Fielded(4).doubled(), m.Fielded(4).value)\n\
         print(m.Fieldless().label(), m.Keyed().label())\n\
         print(type(m.Fielded).__name__, type(m.Fieldless).__name__, type(m.Keyed).__name__)\n\
         print(isinstance(m.Fielded(1), m.Fielded), isinstance(m.Fieldless(), m.Fieldless))\n\
         print(type(m.Fielded.doubled).__name__, type(m.Fieldless.label).__name__,\n\
         \x20     type(m.Keyed.label).__name__)\n",
    );
    assert_eq!(
        out,
        "8 4\n\
         fieldless keyed\n\
         ABCMeta ABCMeta ABCMeta\n\
         True True\n\
         function method_descriptor method_descriptor"
    );
}

#[test]
fn a_class_level_constant_keeps_its_class_off_the_metaclass_construction() {
    // a class-level constant is copied onto the *finished* type, and through a metaclass
    // that is too late: the metaclass has already decided what the class defines from a
    // namespace the constant was never in. an `EnumType` handed a memberless namespace
    // declares no members, and the copy then lands them in the type's dict behind its
    // back — `Boundary.STRICT` answers while `_member_names_` is empty and
    // `isinstance(FIRST, Boundary)` is False. that was a silent wrong answer, so a class
    // with any constant keeps the interpreted definition instead.
    //
    // the two classes are the boundary: same base, same metaclass, and only `Boundary`
    // has a constant. `function` against `method_descriptor` is what says the fallback is
    // exactly that narrow — widening the gate would make `Plain` say `function` too
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_metaconstant");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
from enum import StrEnum, auto


class Boundary(StrEnum):
    STRICT = auto()
    CONFORM = auto()

    def shout(self) -> str:
        return self.name


class Plain(StrEnum):
    # no constant, so the namespace the metaclass is handed is the whole class body
    def shout(self) -> str:
        return \"plain\"


FIRST = Boundary.STRICT
";
    let built = match build_source(
        source,
        "by_diff_metaconstant",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    // nothing is declined: the class is emitted, and only the *construction* the runtime
    // picks for it changes
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_metaconstant as m\n\
         print(m.Boundary._member_names_)\n\
         print(isinstance(m.FIRST, m.Boundary), m.FIRST is m.Boundary.STRICT)\n\
         print(m.Boundary.STRICT.shout(), m.Boundary('strict') is m.Boundary.STRICT)\n\
         print([c.__name__ for c in m.Boundary.__mro__])\n\
         print(type(m.Boundary).__name__, type(m.Plain).__name__)\n\
         print(m.Plain._member_names_, m.Plain().shout() if False else 'plain')\n\
         print(type(m.Boundary.shout).__name__, type(m.Plain.shout).__name__)\n",
    );
    assert_eq!(
        out,
        "['STRICT', 'CONFORM']\n\
         True True\n\
         STRICT True\n\
         ['Boundary', 'StrEnum', 'str', 'ReprEnum', 'Enum', 'object']\n\
         EnumType EnumType\n\
         [] plain\n\
         function method_descriptor"
    );
}

#[test]
fn an_annotated_class_attribute_reaches_the_compiled_type() {
    // the statement was skipped in both the layout pass and the constant pass, so an
    // annotated class attribute was lost outright: `Tagged.KIND` raised where python
    // answers `'tagged'`. it is the same binding a plain assignment makes — the
    // annotation only adds an entry to `__annotations__`
    //
    // the other four are the other constructions, because the attribute was lost on all
    // of them: `Held` owns a layout and is readied in place, `Root` is a base and comes
    // from a type spec, `Leaf` is built on an in-module base and `OnExternal` on one from
    // outside. `method_descriptor` against `function` is what says the *compiled* type
    // answered — a class that fell back to its interpreted definition would agree on
    // every value here and say `function`
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_annotatedconstant");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Tagged:
    KIND: str = \"tagged\"
    BARE: int
    PLAIN = 1

    def method(self) -> str:
        return self.KIND


class Held:
    LIMIT: int = 7

    def __init__(self, n: int) -> None:
        self.n = n

    def total(self) -> int:
        return self.n + self.LIMIT


class Root:
    ROOT: str = \"root\"

    def method(self) -> str:
        return self.ROOT


class Leaf(Root):
    LEAF: int = 2


class OnExternal(Exception):
    CODE: int = 7
";
    let built = match build_source(
        source,
        "by_diff_annotatedconstant",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_annotatedconstant as m\n\
         print(m.Tagged.KIND, m.Tagged.PLAIN, m.Tagged().method())\n\
         print(hasattr(m.Tagged, 'BARE'))\n\
         print(m.Held.LIMIT, m.Held(3).total())\n\
         print(m.Root.ROOT, m.Root().method(), m.Leaf.ROOT, m.Leaf.LEAF, m.OnExternal.CODE)\n\
         print(type(m.Tagged.method).__name__, type(m.Held.total).__name__, type(m.Root.method).__name__)\n",
    );
    assert_eq!(
        out,
        "tagged 1 tagged\n\
         False\n\
         7 10\n\
         root root root 2 7\n\
         method_descriptor method_descriptor method_descriptor"
    );
}

#[test]
fn what_the_module_body_gives_a_class_after_its_statement_agrees() {
    // the interpreted definition runs first and the whole module body runs against it,
    // so a class the body keeps mutating is mutated *there* — and the compiled type that
    // replaces it in the namespace saw none of it. `xml.dom.minidom` installs five
    // properties from a helper that way and `turtle` sixty forwarded methods, all of
    // which raised `AttributeError` on the compiled module.
    //
    // `_pair` is `urllib.parse`'s shape and the reason a plain copy is not enough: the
    // value it writes is the *interpreted* `Bytes`, so copying it would make
    // `Text._encoded_counterpart("a")` build an object `isinstance` says is not a
    // `m.Bytes` — a silent wrong answer where the loss was a loud one
    agree_python(
        "twingift",
        "\
class Bytes:
    def __init__(self, tag: str) -> None:
        self.tag = tag

    def label(self) -> str:
        return \"bytes:\" + self.tag


class Text:
    def __init__(self, tag: str) -> None:
        self.tag = tag

    def label(self) -> str:
        return \"text:\" + self.tag


def _pair() -> None:
    decoded = Text
    encoded = Bytes
    decoded._encoded_counterpart = encoded
    encoded._decoded_counterpart = decoded


_pair()


def _install(cls: type) -> None:
    setattr(cls, \"shout\", lambda self: self.label().upper())


_install(Text)
Text.marker = 7
",
        &[
            "m.Text.marker",
            "m.Text(\"a\").shout()",
            "m.Text._encoded_counterpart is m.Bytes",
            "m.Bytes._decoded_counterpart is m.Text",
            "isinstance(m.Text._encoded_counterpart(\"a\"), m.Bytes)",
            "m.Text._encoded_counterpart(\"a\").label()",
        ],
    );
}

#[test]
fn a_late_gift_that_could_hand_the_interpreted_class_back_is_left_alone() {
    // carrying an attribute across is only sound where the value provably cannot answer
    // with the interpreted definition, which is about to stop being the class under its
    // name. a value that *is* one is replaced by the type standing in for it; everything
    // else is carried only in the shapes enumerated in `By_ReachesTwin`, and a shape not
    // among them stays absent — the loud failure it already was, rather than a quiet
    // wrong one.
    //
    // so `SAMPLE` (an instance of the interpreted class), `ITEMS` (a list, which can be
    // given one after the question is asked) and `HIDDEN` (a tuple holding one) do not
    // come across, while `MARKER`, `PAIR` and `shout` do. a dunder never does: a name in
    // the type's dict does not fill a type slot, so `__ge__` there would answer
    // `a.__ge__(b)` while `a >= b` still went to the slot.
    //
    // `method_descriptor` is what says the compiled type answered at all: a class that
    // fell back to its interpreted definition would carry every one of these, because it
    // *is* the object the module body wrote to
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_twinshapes");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Other:
    def tag(self) -> str:
        return \"other\"


class Held:
    def tag(self) -> str:
        return \"held\"


Held.MARKER = 3
Held.PAIR = Other
Held.SAMPLE = Other()
Held.ITEMS = [1, 2]
Held.HIDDEN = (Other,)
Held.__ge__ = lambda self, right: True
Held.shout = lambda self: self.tag().upper()
";
    let built = match build_source(
        source,
        "by_diff_twinshapes",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_twinshapes as m\n\
         print(m.Held.MARKER, m.Held.PAIR is m.Other, m.Held().shout())\n\
         print(hasattr(m.Held, 'SAMPLE'), hasattr(m.Held, 'ITEMS'), hasattr(m.Held, 'HIDDEN'))\n\
         print(type(m.Held.tag).__name__, type(m.Other.tag).__name__)\n",
    );
    assert_eq!(
        out,
        "3 True HELD\n\
         False False False\n\
         method_descriptor method_descriptor"
    );
}

#[test]
fn a_class_answers_the_annotations_its_body_wrote() {
    // `__annotations__` is read through a getset on the metatype, which refuses outright
    // for a type that is not a heap type and otherwise reads the class's *own* dict —
    // so both halves have to be right at once. a class that wrote none answers an empty
    // mapping rather than raising, and a subclass answers its own rather than its base's:
    // python never merges the two.
    //
    // `tuple[int, str]` is here because a parameterisation is not a class and had to be
    // recognised as one of the shapes that cannot hold an interpreted twin
    agree_python(
        "annwritten",
        "\
class Bare:
    def tag(self) -> str:
        return \"bare\"


class Written:
    count: int
    label: str = \"x\"
    pair: tuple[int, str]
    maybe: int | None

    def tag(self) -> str:
        return \"written\"


class Sub(Written):
    extra: float

    def tag(self) -> str:
        return \"sub\"
",
        &[
            "m.Bare.__annotations__",
            "m.Written.__annotations__",
            "m.Sub.__annotations__",
            "sorted(m.Written.__annotations__)",
            "'__annotations__' in vars(m.Written)",
            "m.Written.__annotations__ is m.Written.__annotations__",
        ],
    );
}

#[test]
fn a_deferred_annotation_is_carried_as_the_string_the_body_wrote() {
    // `from __future__ import annotations` makes every annotation the text of itself, so
    // the mapping is one of strings and a name in it need never resolve — `Later` is
    // written before it exists. this is the whole of what carrying them can do: the
    // values are whatever the class body evaluated, and nothing here re-evaluates them
    agree_python(
        "anndeferred",
        "\
from __future__ import annotations


class Node:
    peer: Later
    tally: int

    def tag(self) -> str:
        return \"node\"


class Later:
    pass
",
        &["m.Node.__annotations__", "m.Later.__annotations__"],
    );
}

#[test]
fn an_annotation_that_could_hand_the_interpreted_class_back_is_refused() {
    // an annotation is subject to the rule every carried attribute is: a value that *is*
    // an interpreted twin becomes the type standing in for it, and one that can still
    // reach a twin cannot be carried at all. `Peer.peer` is the first and comes across
    // pointing at the class under the name; `Deep.many` is the second — `list[Node]`
    // holds the definition that is about to stop being `m.Node`.
    //
    // what is different about `__annotations__` is what happens then. an uncarried
    // attribute is absent, and absent is loud; an absent `__annotations__` is an empty
    // mapping python invents on the spot, which is a wrong answer wearing a right one's
    // clothes. so the refusal is written in, and it is the refusal a class that was never
    // a heap type gave for free.
    //
    // `method_descriptor` is what says the compiled type answered at all — a class that
    // fell back to its interpreted definition would carry every one of these because it
    // *is* the object the body wrote. and a plain class is still sealed: neither `setattr`
    // nor a subclass reaches it, which is what licenses the direct method call
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_annreach");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Node:
    def tag(self) -> str:
        return \"node\"


class Peer:
    peer: Node
    plain: list[int]

    def tag(self) -> str:
        return \"peer\"


class Deep:
    many: list[Node]

    def tag(self) -> str:
        return \"deep\"
";
    let built = match build_source(
        source,
        "by_diff_annreach",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_annreach as m\n\
         print(m.Peer.__annotations__, m.Peer.__annotations__['peer'] is m.Node)\n\
         print(hasattr(m.Deep, '__annotations__'))\n\
         try:\n\
         \x20   m.Deep.__annotations__\n\
         except AttributeError as error:\n\
         \x20   print('refused', error)\n\
         try:\n\
         \x20   m.Deep.later = 1\n\
         except TypeError:\n\
         \x20   print('sealed')\n\
         try:\n\
         \x20   type('X', (m.Deep,), {})\n\
         except TypeError:\n\
         \x20   print('no base')\n\
         print(type(m.Peer.tag).__name__, type(m.Deep.tag).__name__)\n",
    );
    assert_eq!(
        out,
        "{'peer': <class 'by_diff_annreach.Node'>, 'plain': list[int]} True\n\
         False\n\
         refused type object 'by_diff_annreach.Deep' has no attribute '__annotations__'\n\
         sealed\n\
         no base\n\
         method_descriptor method_descriptor"
    );
}

#[test]
fn an_annotation_that_never_resolves_is_refused_rather_than_emptied() {
    // 3.14 works a class's annotations out at the first read rather than at the `class`
    // statement, which is what lets one name something that is never defined at all. the
    // interpreted class raises `NameError` from the read; the compiled one has no
    // function to raise it, because carrying the annotations means settling them at
    // import. so what it carries is the refusal — the same one an unsafe value gets, and
    // the same reason: an empty mapping would be a wrong answer wearing a right one's
    // clothes.
    //
    // below 3.14 the class statement itself raises and the module never imports, so
    // there is nothing here to pin
    let Some((python, toolchain)) = environment() else {
        return;
    };
    if !supports(&toolchain, (3, 14)) {
        return;
    }
    let dir = std::env::temp_dir().join("by_diff_annlost");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Node:
    later: Never

    def tag(self) -> str:
        return \"node\"


class Fine:
    plain: int

    def tag(self) -> str:
        return \"fine\"
";
    let built = match build_source(
        source,
        "by_diff_annlost",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_annlost as m\n\
         try:\n\
         \x20   m.Node.__annotations__\n\
         except AttributeError as error:\n\
         \x20   print('refused', error)\n\
         print(m.Fine.__annotations__)\n\
         print(type(m.Node.tag).__name__, type(m.Fine.tag).__name__)\n",
    );
    assert_eq!(
        out,
        "refused type object 'by_diff_annlost.Node' has no attribute '__annotations__'\n\
         {'plain': <class 'int'>}\n\
         method_descriptor method_descriptor"
    );
}

#[test]
fn a_class_that_keeps_a_dunder_of_its_own_stays_a_static_type() {
    // an emitted class has no instance dict, so an attribute of the *instance* is a
    // descriptor in the type's dict — and the type machinery reads a heap type's
    // `__module__` out of that dict while it works a static type's out of `tp_name`. so a
    // field spelled as a dunder collides with the class's own answer on a heap type and
    // not on a static one: `functools.cached_property` keeps a `__module__`, and as a heap
    // type it answered its own descriptor where the class should answer `m`.
    //
    // such a class stays exactly what it was, refusal on `__annotations__` included —
    // that is the boundary of the fix, and it is the failure the class already gave
    // rather than a new one
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_anndunder");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Keeps:
    held: int

    def __init__(self, tag: str) -> None:
        self.tag = tag
        self.__module__ = \"elsewhere\"

    def label(self) -> str:
        return self.tag


class Plain:
    held: int

    def __init__(self, tag: str) -> None:
        self.tag = tag

    def label(self) -> str:
        return self.tag
";
    let built = match build_source(
        source,
        "by_diff_anndunder",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_anndunder as m\n\
         print(m.Keeps.__module__, m.Keeps('a').__module__, m.Keeps('a').label())\n\
         print(hasattr(m.Keeps, '__annotations__'), m.Plain.__annotations__)\n\
         print(type(m.Keeps.label).__name__, type(m.Plain.label).__name__)\n",
    );
    assert_eq!(
        out,
        "by_diff_anndunder elsewhere a\n\
         False {'held': <class 'int'>}\n\
         method_descriptor method_descriptor"
    );
}

#[test]
fn a_subclass_of_a_class_the_metaclass_gates_turn_down_is_built_here() {
    // both metaclass gates are asked while the layouts settle, so a class either of them
    // turns down leaves the layout set — and its subclass is then laid out on the
    // interpreted definition the way every other declining class's subclass is. asked
    // while the body was lowered instead, the base stayed in the set and each subclass
    // cascaded behind it, which is what this pair of declines used to be four of.
    //
    // the bases here carry `ABCMeta`, so `PyType_FromSpecWithBases` is closed to the
    // subclass and its metaclass builds it — `method_descriptor` is what says that
    // construction happened at all, since a subclass that fell back would answer every
    // value here from a `function`
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_gatedbase");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
from abc import ABCMeta


class Decorated(metaclass=ABCMeta):
    @staticmethod
    def label() -> str:
        return \"decorated\"


class BelowDecorated(Decorated):
    def size(self) -> int:
        return 1


class Constant(metaclass=ABCMeta):
    TAG = 1

    def label(self) -> str:
        return \"constant\"


class BelowConstant(Constant):
    def size(self) -> int:
        return 2
";
    let built = match build_source(
        source,
        "by_diff_gatedbase",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    // exactly the two bases, and nothing behind them: a subclass in this list is the
    // cascade this move exists to stop
    assert_eq!(
        built
            .declined
            .iter()
            .map(|declined| declined.name.as_str())
            .collect::<Vec<_>>(),
        ["Decorated", "Constant"]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_gatedbase as m\n\
         print(m.BelowDecorated().size(), m.BelowDecorated.label())\n\
         print(m.BelowConstant().size(), m.BelowConstant.TAG, m.BelowConstant().label())\n\
         print([b.__name__ for b in m.BelowConstant.__mro__])\n\
         print(isinstance(m.BelowConstant(), m.Constant), isinstance(m.BelowDecorated(), m.Decorated))\n\
         print(type(m.BelowDecorated.size).__name__, type(m.BelowConstant.size).__name__)\n",
    );
    assert_eq!(
        out,
        "1 decorated\n\
         2 1 constant\n\
         ['BelowConstant', 'Constant', 'object']\n\
         True True\n\
         method_descriptor method_descriptor"
    );
}

#[test]
fn a_pair_the_body_cross_links_agrees_when_a_gate_took_their_base_out_of_the_layouts() {
    // `urllib.parse`'s shape, and the one that reverted this move the first time: the
    // base declines at the class-level-constant gate, so both result classes are built
    // here — over an interpreted base whose metaclass is `type`, which means a real
    // emitted type replaces each twin in the namespace. `_pair` then runs against the
    // twins, because the whole module body runs before module init installs anything.
    //
    // what makes it agree is that an emitted type carries what the body gave its twin
    // *and* remaps a carried twin to the type standing in for it — without the remap
    // `Text._encoded_counterpart()` builds something `isinstance` says is not a
    // `m.Bytes`
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_gatedpair");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Root:
    def kind(self) -> str:
        return \"root\"


class Extra:
    def extra(self) -> str:
        return \"extra\"


class Mixin(Root, Extra):
    TAG = 1

    def tag(self) -> int:
        return self.TAG


class Text(Mixin):
    def label(self) -> str:
        return \"text\"


class Bytes(Mixin):
    def label(self) -> str:
        return \"bytes\"


def _pair() -> None:
    decoded = Text
    encoded = Bytes
    decoded._encoded_counterpart = encoded
    encoded._decoded_counterpart = decoded


_pair()
";
    let built = match build_source(
        source,
        "by_diff_gatedpair",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    // `Root` and `Extra` follow `Mixin` down because an emitted class cannot have an
    // interpreted subclass. `Text` and `Bytes` are the two that must not be here
    assert_eq!(
        built
            .declined
            .iter()
            .map(|declined| declined.name.as_str())
            .collect::<Vec<_>>(),
        ["Mixin", "Root", "Extra"]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_gatedpair as m\n\
         print(m.Text._encoded_counterpart is m.Bytes, m.Bytes._decoded_counterpart is m.Text)\n\
         print(isinstance(m.Text._encoded_counterpart(), m.Bytes), m.Text._encoded_counterpart().label())\n\
         print(m.Text().label(), m.Text().tag(), m.Text().kind(), m.Text().extra(), m.Text.TAG)\n\
         print([b.__name__ for b in m.Text.__mro__])\n\
         print(type(m.Text.label).__name__, type(m.Bytes.label).__name__)\n",
    );
    assert_eq!(
        out,
        "True True\n\
         True bytes\n\
         text 1 root extra 1\n\
         ['Text', 'Mixin', 'Root', 'Extra', 'object']\n\
         method_descriptor method_descriptor"
    );
}

#[test]
fn a_private_name_in_a_class_body_is_mangled() {
    // python binds an identifier of two leading underscores written in `class C` as
    // `_C__spam`, whatever it names. the compiler read the written name, so the compiled
    // class published `__read` and `__buffer` where python publishes `_Printer__read`
    // and `_Printer__buffer` — `symtable.Function` lost five class attributes and three
    // methods that way, and every read of one inside a method raised `AttributeError`
    // because the name it looked for was never bound
    //
    // `_Printer` also exercises the class's *own* leading underscore, which the mangling
    // strips
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_privatemangle");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class _Printer:
    __LIMIT = 4

    def __init__(self, source: str) -> None:
        self.__buffer = source
        self.plain = 0

    def __read(self) -> str:
        return self.__buffer

    def take(self) -> str:
        return self.__read()

    def limit(self) -> int:
        return self.__LIMIT
";
    let built = match build_source(
        source,
        "by_diff_privatemangle",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_privatemangle as m\n\
         p = m._Printer('hi')\n\
         print(p.take(), p.limit(), getattr(p, '_Printer__buffer'))\n\
         print(hasattr(p, '__buffer'), hasattr(m._Printer, '__read'))\n\
         print(m._Printer._Printer__LIMIT, callable(m._Printer._Printer__read))\n\
         print(type(m._Printer.take).__name__)\n",
    );
    assert_eq!(
        out,
        "hi 4 hi\n\
         False False\n\
         4 True\n\
         method_descriptor"
    );
}

#[test]
fn a_name_that_is_both_a_class_constant_and_a_field_keeps_the_interpreted_class() {
    // the constant is copied into the type's dict *after* `PyType_Ready` put the field's
    // descriptor there, so the constant wins and every instance answers the class-level
    // value instead of its own. that is a silent wrong answer, so the class declines and
    // the interpreted definition — which python's own rules already get right — answers
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_constantfieldclash");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Tagged:
    KIND: str = \"class-level\"

    def __init__(self, kind: str) -> None:
        self.KIND = kind

    def read(self) -> str:
        return self.KIND
";
    let built = match build_source(
        source,
        "by_diff_constantfieldclash",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(
        built.declined.iter().any(|declined| declined
            .reason
            .contains("both a class-level constant and a field")),
        "declined: {:?}",
        built.declined
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_constantfieldclash as m\n\
         print(m.Tagged('mine').KIND, m.Tagged('mine').read(), m.Tagged.KIND)\n\
         print(type(m.Tagged.read).__name__)\n",
    );
    assert_eq!(out, "mine mine class-level\nfunction");
}

#[test]
fn fields_past_a_python_base_leave_the_module_to_its_interpreted_definition() {
    // `Wrapped` keeps its fields past a `codecs.IncrementalDecoder` instance, so it
    // supplies `tp_dealloc`, `tp_traverse` and `tp_clear` and each calls the base's.
    // that base is a class statement's type, whose three are python's own dispatchers:
    // each resolves which base to chain to from `Py_TYPE(self)`, finds `Wrapped`'s
    // function there, and calls it straight back. the two then called each other until
    // the stack ran out — a segfault at the first deallocation, and another at the first
    // collection, which 56 stdlib modules took.
    //
    // which type a base name stands for is the running interpreter's answer, so the
    // refusal is one too: the module is left as the interpreted definition already built
    // it, because its compiled functions read `Wrapped`'s fields at an offset only the
    // spec-built type has. `stays` proves it — a compiled module answers
    // `builtin_function_or_method` there
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_pythonbasestorage");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
import codecs


class Wrapped(codecs.IncrementalDecoder):
    def __init__(self, decoder=None):
        self.decoder = decoder

    def get(self):
        return self.decoder


class Stored(Exception):
    # the boundary: `Exception` is a type python allocates and frees itself, so chaining
    # to it terminates and this one keeps its fields past the instance as before
    def __init__(self, code: int) -> None:
        self.code = code

    def bumped(self) -> int:
        return self.code + 1


def stays() -> int:
    return 1
";
    let built = match build_source(
        source,
        "by_diff_pythonbasestorage",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    // the collection and the deallocation are separate crashes — `tp_traverse` and
    // `tp_dealloc` are dispatched the same way — so both are reached here
    let out = run(
        &python,
        &dir,
        "import gc\n\
         import by_diff_pythonbasestorage as m\n\
         w = m.Wrapped(7)\n\
         print(w.get(), w.decoder, gc.collect() >= 0)\n\
         del w\n\
         print(gc.collect() >= 0)\n\
         print(m.Stored(4).bumped(), type(m.stays).__name__, type(m.Wrapped.get).__name__)\n",
    );
    assert_eq!(
        out,
        "7 7 True\n\
         True\n\
         5 function function"
    );
}

#[test]
fn a_spec_that_cannot_place_the_dict_leaves_the_module_to_its_interpreted_definition() {
    // `Wrapped` keeps its fields past a `complex` instance, and the spec built for it
    // takes `complex` for the layout while `codecs.Codec` — a class statement's type,
    // with a managed `__dict__` — is where the dict offset comes from. the type then
    // claims a dict there is no room for, which is why the construction asks the finished
    // type where its dict ended up.
    //
    // the refusal has to be the whole module's. answering with the interpreted definition
    // instead put a type under the name whose instances stop where `complex`'s do, while
    // `made` went on allocating from it and writing `tag` at the offset the spec-built
    // type would have had — eight bytes past the end of the object, which a guard page
    // catches and an ordinary run corrupts silently.
    //
    // `stays` is the proof: a compiled module answers `builtin_function_or_method` there
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_specdictplacement");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
import codecs


class Wrapped(complex, codecs.Codec):
    def __init__(self) -> None:
        self.tag = 1

    def get(self) -> int:
        return self.tag


def made() -> int:
    return Wrapped().get()


def stays() -> int:
    return 2
";
    let built = match build_source(
        source,
        "by_diff_specdictplacement",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_specdictplacement as m\n\
         print(m.made(), m.stays(), m.Wrapped().get())\n\
         print(type(m.stays).__name__, type(m.Wrapped.get).__name__)\n",
    );
    assert_eq!(
        out,
        "1 2 1\n\
         function function"
    );
}

#[test]
fn a_finalizer_over_fields_answers_for_a_construction_that_raised() {
    // `Held()` raises before `self.path` is written, and python releases the half-built
    // object — which runs `__del__` over fields that are still the zeroes `tp_alloc`
    // left. read as always written, the null went straight to `print`, which took `wave`
    // and `tarfile` down with a segfault. every field of a layout a finalizer can reach
    // carries a byte saying whether it was written, so the read answers python's own
    // `AttributeError` instead
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_finalizerfields");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Held:
    def __init__(self, path: str) -> None:
        self.path = path

    def read(self) -> str:
        return self.path

    def __del__(self) -> None:
        print(\"gone\", self.path)


class Apart:
    def __init__(self, n: int) -> None:
        self.n = n

    def doubled(self) -> int:
        return self.n * 2
";
    let built = match build_source(
        source,
        "by_diff_finalizerfields",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import sys\n\
         import by_diff_finalizerfields as m\n\
         seen = []\n\
         sys.unraisablehook = lambda hook: seen.append(str(hook.exc_value))\n\
         held = m.Held('here')\n\
         print(held.read())\n\
         del held\n\
         try:\n\
         \x20   m.Held()\n\
         except TypeError:\n\
         \x20   print('raised TypeError')\n\
         print(seen)\n\
         print(m.Apart(3).doubled(), type(m.Held.read).__name__, type(m.Apart.doubled).__name__)\n",
    );
    assert_eq!(
        out,
        "here\n\
         gone here\n\
         raised TypeError\n\
         [\"'Held' object has no attribute 'path'\"]\n\
         6 method_descriptor method_descriptor"
    );
}

#[test]
fn a_dict_offset_from_a_base_that_does_not_own_the_layout_keeps_the_interpreted_class() {
    // a spec adds neither a `__dict__` nor weakrefs — it takes the whole instance shape
    // from `float`, which python picks as the layout base — but `tp_dictoffset` is
    // inherited from whichever base *declares* one, and `codecs.Codec` declares a managed
    // one. the type then carried the offset of a dict there was no room for, and
    // `subtype_dealloc` released whatever stood at it: 24 of the `encodings` modules
    // segfaulted at the first deallocation of a codec.
    //
    // `Plain` is the boundary: one base, so the offsets are the layout base's and the
    // compiled type answers
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_borrowedoffset");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
import codecs


class Held(float, codecs.Codec):
    def label(self) -> str:
        return \"held\"


class Plain(codecs.Codec):
    def label(self) -> str:
        return \"plain\"
";
    let built = match build_source(
        source,
        "by_diff_borrowedoffset",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_borrowedoffset as m\n\
         held = m.Held()\n\
         # the dict the offset promised: the class python builds really has one, which is\n\
         # the whole of why the offset is there\n\
         held.extra = 3\n\
         print(held.label(), float(held), held.extra)\n\
         del held\n\
         print(m.Plain().label())\n\
         print(type(m.Held.label).__name__, type(m.Plain.label).__name__)\n",
    );
    assert_eq!(
        out,
        "held 0.0 3\n\
         plain\n\
         function method_descriptor"
    );
}

#[test]
fn a_class_whose_base_declined_declines_with_it() {
    // a class was emitted with its base *silently dropped* when that base declined:
    // codegen looked the base up among the emitted classes, found nothing, and built a
    // type with no bases at all. the subclass then lost everything the base brought,
    // which is a wrong answer rather than a slow one
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_declinedbase");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Outer:
    def __new__(cls):
        return super().__new__(cls)

    def label(self) -> str:
        return \"outer\"


class Inner(Outer):
    def tag(self) -> str:
        return \"inner\"
";
    let built = match build_source(
        source,
        "by_diff_declinedbase",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    let mut declined: Vec<(&str, &str)> = built
        .declined
        .iter()
        .map(|declined| (declined.name.as_str(), declined.reason.as_str()))
        .collect();
    declined.sort_unstable();
    assert_eq!(
        declined,
        vec![
            ("Inner", "`Outer` declined, so it is not a base to build on"),
            ("Outer", "`__new__` fills a type slot with no adapter yet"),
        ]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_declinedbase as m\n\
         print(issubclass(m.Inner, m.Outer), isinstance(m.Inner(), m.Outer))\n\
         print(m.Inner().label(), m.Inner().tag())\n",
    );
    assert_eq!(out, "True True\nouter inner");
}

#[test]
fn a_base_an_interpreted_class_extends_declines_with_it() {
    // the other direction, and the one the stdlib's `optparse` hit: the base compiled
    // and the class extending it declined. the interpreted definition then built that
    // class on the *interpreted* twin, which module init had already replaced in the
    // namespace — so there were two classes under one name, `Container.__init__(self)`
    // was a descriptor of the wrong one, and the construction raised
    //
    // an emitted class simply cannot have an interpreted subclass: its static type
    // object refuses to be a base at all, and the direct method call reads that
    // refusal as proof no override exists. so the base goes interpreted too
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_interpretedsub");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Container:
    def __init__(self, tag: str) -> None:
        self.tag = tag

    def label(self) -> str:
        return \"container:\" + self.tag


class Parser(Container):
    def __new__(cls, tag: str) -> \"Parser\":
        return object.__new__(cls)

    def __init__(self, tag: str) -> None:
        Container.__init__(self, tag)

    def label(self) -> str:
        return \"parser:\" + self.tag


def describe(item: Container) -> str:
    return item.label()
";
    let built = match build_source(
        source,
        "by_diff_interpretedsub",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    let mut declined: Vec<(&str, &str)> = built
        .declined
        .iter()
        .map(|declined| (declined.name.as_str(), declined.reason.as_str()))
        .collect();
    declined.sort_unstable();
    assert_eq!(
        declined,
        vec![
            (
                "Container",
                "`Parser` declined, so it extends the interpreted definition rather than this type"
            ),
            ("Parser", "`__new__` fills a type slot with no adapter yet"),
            // and the decline reaches the caller, which is what keeps `describe` from
            // calling the base's `label` directly past an override it cannot see
            ("describe", "`Container` declined, so it has no layout"),
        ]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_interpretedsub as m\n\
         print(issubclass(m.Parser, m.Container), isinstance(m.Parser('x'), m.Container))\n\
         print(m.Parser('x').label(), m.Container('y').label())\n\
         print(m.Parser('x').tag, m.describe(m.Parser('x')), m.describe(m.Container('y')))\n",
    );
    assert_eq!(
        out,
        "True True\n\
         parser:x container:y\n\
         x parser:x container:y"
    );
}

#[test]
fn a_zero_argument_super_is_lowered_to_the_two_argument_form() {
    // python's own compiler fills the class and the receiver in from the frame. a
    // compiled method has no frame, but the compiler knows both, so it fills them in
    // at the call instead. it used to raise `RuntimeError` there, with no decline to
    // warn that it would — and the same class hid a second wrong answer, because a
    // written `__init__` the base's `tp_init` shadowed simply never ran.
    //
    // `method_descriptor` is what says the *compiled* type answered: a class that
    // fell back to its interpreted definition answers identically, so the behaviour
    // alone cannot tell the two apart
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_zerosuper");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Holder(dict):
    def __init__(self) -> None:
        super().__init__()
        self[\"a\"] = 1

    def label(self) -> str:
        return \"holder\"


class Base:
    def lbl(self) -> str:
        return \"base\"


class Slot(Base):
    def whose(self) -> bool:
        other = Slot()
        self = other
        # python reads the *slot*, not the argument, so this is the reassigned
        # instance — and `super()` is a value here rather than a call target
        s = super()
        return s.__self__ is other
";
    let built = match build_source(
        source,
        "by_diff_zerosuper",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_zerosuper as m\n\
         print(m.Holder(), m.Holder().label())\n\
         print(m.Slot().whose())\n\
         print(type(m.Holder.__dict__['label']).__name__,\n\
         \x20     type(m.Slot.__dict__['whose']).__name__)\n",
    );
    assert_eq!(
        out,
        "{'a': 1} holder\n\
         True\n\
         method_descriptor method_descriptor"
    );
}

#[test]
fn a_zero_argument_super_follows_the_mro_of_the_instance() {
    // `super()` is `super(<the class the def is written in>, self)` — the *owner*,
    // never the owner's base. passing the base instead answers the same for a single
    // chain and diverges the moment an instance's mro puts something else after the
    // owner, which is what makes this the test that matters.
    //
    // the compiler declines a class with two emitted bases, so the diamond is closed
    // outside the module: `D(m.B, C)` puts `C` after `B`, and the compiled `B.go`
    // has to reach it rather than jumping straight to `A`
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_supermro");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class A:
    def go(self) -> str:
        return \"A\"


class B(A):
    def go(self) -> str:
        return \"B->\" + super().go()
";
    let built = match build_source(
        source,
        "by_diff_supermro",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_supermro as m\n\
         class C(m.A):\n\
         \x20   def go(self): return \"C->\" + super().go()\n\
         class D(m.B, C):\n\
         \x20   def go(self): return \"D->\" + super().go()\n\
         print(type(m.B.__dict__['go']).__name__)\n\
         print([c.__name__ for c in D.__mro__])\n\
         print(m.B().go())\n\
         print(D().go())\n",
    );
    assert_eq!(
        out,
        "method_descriptor\n\
         ['D', 'B', 'C', 'A', 'object']\n\
         B->A\n\
         D->B->C->A"
    );
}

#[test]
fn a_zero_argument_super_names_the_class_the_class_statement_made() {
    // python's `__class__` is a cell holding the class object, not a name lookup —
    // and a class decorator replaces the *namespace* entry, so the two are different
    // objects the moment a decorator returns something else.
    //
    // resolving the owner through the namespace would hand `super()` the wrapper,
    // whose next class in the mro is the wrapped class itself: `Dec.lbl` would call
    // `Dec.lbl` forever. so `RecursionError` here is the wrong answer this rules out
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_superowner");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Base:
    def lbl(self) -> str:
        return \"base\"


def rebind(cls: type) -> type:
    return type(\"Wrapped\", (cls,), {})


@rebind
class Dec(Base):
    def lbl(self) -> str:
        return \"dec->\" + super().lbl()
";
    let built = match build_source(
        source,
        "by_diff_superowner",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import sys\n\
         sys.setrecursionlimit(150)\n\
         import by_diff_superowner as m\n\
         owner = m.Dec.__mro__[1]\n\
         print(m.Dec.__name__, owner.__name__,\n\
         \x20     type(owner.__dict__['lbl']).__name__)\n\
         try:\n\
         \x20   print(m.Dec().lbl())\n\
         except RecursionError:\n\
         \x20   print('RecursionError: super() resolved the owner by name')\n",
    );
    assert_eq!(
        out,
        "Wrapped Dec method_descriptor\n\
         dec->base"
    );
}

#[test]
fn a_zero_argument_super_declines_where_slot_zero_is_not_the_receiver() {
    // the lowering is only sound where slot zero holds what python would read out of
    // it. each of these leaves something else there — or nothing — and every one of
    // them is a case python itself either raises on or answers differently, so a
    // decline is the only right answer. widening any of them back out would compile
    // something that misbehaves at the call, which is what this whole feature undid
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_supernoslot");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class A:
    def go(self) -> str:
        return \"A\"

    @classmethod
    def cgo(cls) -> str:
        return \"Acgo\"


class Cm(A):
    @classmethod
    def cgo(cls) -> str:
        return \"Cm->\" + super().cgo()


class Sm(A):
    @staticmethod
    def sgo() -> str:
        return str(super())


class Isub(A):
    def __init_subclass__(cls, **kw) -> None:
        super().__init_subclass__(**kw)


class Nested(A):
    def go(self) -> str:
        def inner() -> str:
            return super().go()
        return inner()


class Lam(A):
    def go(self) -> str:
        f = lambda: super().go()
        return f()


class Genex(A):
    def go(self) -> str:
        return \"\".join(super().go() for _ in range(1))


class Gen(A):
    def stream(self):
        yield super().go()


class Starred(A):
    def go(*args) -> str:
        return \"star\" + super().go()


class Kwonly(A):
    def go(*, k: int = 1) -> str:
        return str(super())


class Listc(A):
    def go(self) -> str:
        return \"listc->\" + \"\".join([super().go() for _ in range(1)])
";
    let built = match build_source(
        source,
        "by_diff_supernoslot",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    let declined: Vec<(&str, &str)> = built
        .declined
        .iter()
        .map(|declined| (declined.name.as_str(), declined.reason.as_str()))
        .collect();
    assert_eq!(
        declined,
        vec![
            (
                "Cm",
                "`classmethod` and `staticmethod` both leave something other than the receiver in slot zero"
            ),
            (
                "Sm",
                "`classmethod` and `staticmethod` both leave something other than the receiver in slot zero"
            ),
            (
                "Isub",
                "python makes this method implicitly static or class, so slot zero holds the class rather than a receiver"
            ),
            (
                "Nested",
                "a `super()` with no arguments reads the nested function's own slot zero, not the method's receiver"
            ),
            (
                "Lam",
                "a `super()` with no arguments reads the nested function's own slot zero, not the method's receiver"
            ),
            (
                "Genex",
                "a `super()` in a comprehension reads that comprehension's own frame, which only python 3.12 and later fold into the method's"
            ),
            (
                "Gen",
                "a `super()` with no arguments reads slot zero, which a generator's resume frame fills with its state"
            ),
            (
                "Starred",
                "a `super()` with no arguments reads slot zero, and this method has no positional parameter to fill one"
            ),
            (
                "Kwonly",
                "a `super()` with no arguments reads slot zero, and this method has no positional parameter to fill one"
            ),
            (
                "Listc",
                "a `super()` in a comprehension reads that comprehension's own frame, which only python 3.12 and later fold into the method's"
            ),
            // `A` is the base every class above extends, and `Cm` is the first of them
            // to decline — so the base goes interpreted with it, or `Cm`'s interpreted
            // definition would be subclassing a compiled type that refuses to be a base
            (
                "A",
                "`Cm` declined, so it extends the interpreted definition rather than this type"
            ),
        ]
    );
    // a declined class still answers, through the interpreted definition the fallback
    // left behind. only the two whose answer does not turn on the interpreter version
    // are called: python raises in most of the rest, and differently across versions.
    //
    // `Cm->Acgo` is also what says the decline was needed — a compiled `classmethod`
    // is a method descriptor behind one, which raises `TypeError` when a *type* is
    // what reaches it
    let out = run(
        &python,
        &dir,
        "import by_diff_supernoslot as m\n\
         print(m.Cm.cgo(), list(m.Gen().stream()))\n",
    );
    assert_eq!(out, "Cm->Acgo ['A']");
}

#[test]
fn a_shadowed_super_is_called_the_way_python_calls_it() {
    // zero-argument `super()` compiles to `LOAD_GLOBAL super` and a call with no
    // arguments: the frame-reading magic lives in `super.__init__`, not in the call.
    // so a module that binds the name gets *its* `super`, called with the nought
    // arguments written — where filling two in would call it with two
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_supershadow");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def super() -> str:
    return \"shadow\"


class Base:
    def lbl(self) -> str:
        return \"base\"


class Shadowed(Base):
    def lbl(self) -> str:
        return \"shadowed->\" + super()


class LocalShadow(Base):
    def lbl(self) -> str:
        super = lambda: \"local\"
        return \"local->\" + super()
";
    let built = match build_source(
        source,
        "by_diff_supershadow",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_supershadow as m\n\
         print(m.Shadowed().lbl(), m.LocalShadow().lbl())\n\
         print(type(m.Shadowed.__dict__['lbl']).__name__,\n\
         \x20     type(m.LocalShadow.__dict__['lbl']).__name__)\n",
    );
    assert_eq!(
        out,
        "shadowed->shadow local->local\n\
         method_descriptor method_descriptor"
    );
}

#[test]
fn a_field_named_after_a_c_keyword_agrees() {
    // python has no reserved attribute names, C has forty-odd reserved words, and the
    // struct member took the attribute name verbatim — so `self.int = 1` emitted
    // `PyObject * int;`, which is not C. `argparse` and `_pydecimal` both have one and
    // both failed to build on it
    agree(
        "c_keywords",
        "\
class Holder:
    def __init__(self, value: int) -> None:
        self.int = value
        self.const = value + 1
        self.default = value + 2
        self.register = value + 3
        self.switch = value + 4
        self.union = value + 5
        self.static = value + 6
        self.volatile = value + 7

    def total(self) -> int:
        return (
            self.int
            + self.const
            + self.default
            + self.register
            + self.switch
            + self.union
            + self.static
            + self.volatile
        )
",
        &[
            "m.Holder(1).total()",
            "(m.Holder(2).int, m.Holder(2).const, m.Holder(2).default)",
            "[m.Holder(n).total() for n in (0, 10)]",
        ],
    );
}

#[test]
fn an_attribute_a_path_may_skip_agrees() {
    // python has no fixed layout, so an `if` with no `else` simply leaves the attribute
    // off that instance and a read raises. a compiled class keeps its layout and carries
    // one byte beside the field saying whether it was ever written
    agree(
        "optional_field",
        "\
class Maybe:
    def __init__(self, n: int) -> None:
        self.always = n
        if n > 0:
            self.positive = n * 2
        while n > 5:
            self.big = n
            n = 0
        try:
            self.risky = 1 // n
        except ZeroDivisionError:
            pass

    def read_positive(self) -> int:
        return self.positive

    def has_big(self) -> bool:
        return hasattr(self, \"big\")


class Holder:
    # a field the constructor may skip is still released when it *was* written, and
    # left alone when it was not — a leak or a double free would show under repetition
    def __init__(self, n: int) -> None:
        self.name = \"x\" * n
        if n > 1:
            self.spare = \"y\" * n
",
        &[
            "[m.Maybe(n).always for n in (0, 3, 9)]",
            "[m.Maybe(n).has_big() for n in (0, 3, 9)]",
            "m.Maybe(3).read_positive()",
            "[type(e).__name__ for e in [_capture(m.Maybe(0).read_positive)]]",
            "[str(e) for e in [_capture(m.Maybe(0).read_positive)]]",
            "[getattr(m.Maybe(0), 'positive', 'absent'), getattr(m.Maybe(4), 'positive', 'absent')]",
            "[m.Holder(n).name for n in (1, 3)]",
            "[getattr(m.Holder(n), 'spare', 'absent') for n in (1, 3)]",
        ],
    );
}

#[test]
fn an_inherited_attribute_a_path_may_skip_agrees() {
    // a subclass's struct *begins* with its base's, so a field the base only may have
    // costs the subclass the same presence byte. dropping it laid the subclass out
    // smaller than the base, which cpython rejects when the type is built:
    // `tp_basicsize for type 'Sub' is too small for base 'Base'`. `optparse`, `bdb` and
    // `queue` each failed to import on it
    agree(
        "optional_field_inherited",
        "\
class Base:
    def __init__(self, n: int) -> None:
        self.always = n
        if n > 0:
            self.maybe = n * 2

    def read_maybe(self) -> int:
        return self.maybe

    def has_maybe(self) -> bool:
        return hasattr(self, \"maybe\")


class Sub(Base):
    # its own `__init__`, which still starts from the base's layout — the byte saying
    # whether `maybe` was written is inherited along with the field it belongs to
    def __init__(self, n: int) -> None:
        self.always = n + 100
        if n > 1:
            self.maybe = n * 3
        self.spare = \"y\" * n

    def total(self) -> int:
        return self.always + len(self.spare)


class Deeper(Sub):
    # and again a level down, where the byte has been inherited twice
    def __init__(self, n: int) -> None:
        self.always = n
        self.spare = \"z\"
        self.tail = n * 5


class Bare(Base):
    # no field of its own, so the byte is the whole of what its struct adds. dropping
    # it laid `Bare` out *smaller* than `Base`, which cpython refuses when it builds
    # the type — the module then fails to import at all
    def __init__(self, n: int) -> None:
        self.always = n
        if n > 3:
            self.maybe = n


def through_base(b: Base) -> bool:
    return b.has_maybe()
",
        &[
            // the base is unchanged by having been subclassed
            "[m.Base(n).has_maybe() for n in (0, 3)]",
            // the inherited field is read at the offset the base gave it
            "[m.Sub(n).always for n in (0, 2)]",
            "[m.Sub(n).total() for n in (0, 2)]",
            "[m.Sub(n).spare for n in (0, 2)]",
            "m.Sub(2).read_maybe()",
            // the presence byte survived inheritance, so both answers are the base's
            "[m.Sub(n).has_maybe() for n in (0, 2)]",
            "[getattr(m.Sub(n), 'maybe', 'absent') for n in (0, 2)]",
            // python names the class the *instance* is, not the one that declared the
            // field — read from a method, and read from python through the getter
            "[type(e).__name__ for e in [_capture(m.Sub(0).read_maybe)]]",
            "[str(e) for e in [_capture(m.Sub(0).read_maybe)]]",
            "[str(e) for e in [_capture(getattr, m.Sub(0), 'maybe')]]",
            "[str(e) for e in [_capture(getattr, m.Deeper(1), 'maybe')]]",
            // a field the subclass never writes is absent on every one of its instances
            "[getattr(m.Deeper(n), 'maybe', 'absent') for n in (0, 4)]",
            "[m.Deeper(n).tail for n in (0, 4)]",
            "[m.Deeper(n).has_maybe() for n in (0, 4)]",
            // a subclass adding nothing but the inherited byte
            "[m.Bare(n).always for n in (1, 5)]",
            "[m.Bare(n).has_maybe() for n in (1, 5)]",
            "[getattr(m.Bare(n), 'maybe', 'absent') for n in (1, 5)]",
            // and the layout still admits the subclass where the base is asked for
            "[m.through_base(x) for x in (m.Base(1), m.Sub(0), m.Sub(2), m.Deeper(1), m.Bare(5))]",
            "(isinstance(m.Deeper(1), m.Base), issubclass(m.Deeper, m.Sub))",
        ],
    );
}

#[test]
fn two_definitions_of_one_name_agree() {
    // a `try` / `except` pair each defining the same nested function is real python and
    // appears in the stdlib. the name binds whichever one ran, so a *direct* call cannot
    // know which function it means — and both would mangle to one C symbol, which is
    // how this was found. it declines, and the interpreted definition answers
    agree_with_declines(
        "twodefs",
        "\
def choose(fail: bool) -> str:
    try:
        if fail:
            raise ValueError(\"no\")

        def pick() -> str:
            return \"first\"

    except ValueError:

        def pick() -> str:
            return \"second\"

    return pick()
",
        &["[m.choose(f) for f in (False, True)]"],
    );
}

#[test]
fn a_result_wider_than_its_place_agrees() {
    // three defects the verifier caught as ill-formed lowerings, so they declined
    // rather than miscompiled: a bare `yield` handing back the *unboxed* `None`, and an
    // augmented assignment writing a wider result straight into a narrower register
    agree(
        "wider_result",
        "\
from typing import Iterator


def bare_yields() -> Iterator[None]:
    yield
    yield


async def bare_async_yields():
    yield
    yield


def formatted(n: int, sep: str) -> str:
    # `%` goes through the object protocol, so the sum is an `object` while `s` is a
    # `str` — the in-place form used to write it straight back
    s = \"\"
    s += \"%s%02d\" % (sep, n)
    s += \"!\"
    return s


def widened(n: int) -> float:
    total = 0.0
    total += n
    return total


def bare_return(flag: bool) -> object:
    if flag:
        return
    return 1


def falls_off_the_end(flag: bool) -> object:
    # python returns `None` implicitly, and an `object` return can hold one
    if flag:
        return 1


def shared_cell(a: object, n: int) -> object:
    # `a` is a parameter *and* shared with a closure, so it seeds a cell. the cell holds
    # objects and `a` is already one — boxing it again was ill-formed
    def inner() -> object:
        return a

    a = inner()
    return (a, n)
",
        &[
            "list(m.bare_yields())",
            "[m.formatted(n, s) for n, s in ((7, \":\"), (0, \"-\"))]",
            "[m.widened(n) for n in (0, 3)]",
            "[m.bare_return(f) for f in (True, False)]",
            "[m.falls_off_the_end(f) for f in (True, False)]",
            "m.shared_cell(\"x\", 3)",
            "__import__('asyncio').run(_drain(m.bare_async_yields()))",
        ],
    );
}

#[test]
fn a_method_default_that_is_not_a_literal_agrees() {
    // a default that is not an immediate is evaluated once, at definition time — the
    // interpreted twin already did that and holds the one object every call must share.
    // a module-level function handed such a call to its twin already; a *method* has one
    // too, as an attribute of the interpreted class, and was declining for want of it
    agree(
        "method_defaults",
        "\
SEP = \":\" + \"|\"


class Joiner:
    def __init__(self, n: int) -> None:
        self.n = n

    def join(self, parts: object, sep: str = SEP) -> str:
        return sep.join(parts) + str(self.n)

    def collect(self, items: object, into: object = []) -> object:
        # a mutable default is shared across calls — the interpreted definition holds
        # the one object every call has to see, which is the whole reason the call is
        # handed to it rather than given a fresh value
        for x in items:
            into.append(x)
        return into
",
        &[
            "[m.Joiner(7).join([\"a\", \"b\"]), m.Joiner(7).join([\"a\", \"b\"], \"-\")]",
            // the same list on both calls, which is what proves it is the twin's object
            "(lambda j: [j.collect([1]), j.collect([2]), j.collect([3], [])])(m.Joiner(0))",
        ],
    );
}

#[test]
fn the_numeric_slots_agree() {
    // every binary numeric dunder fills a `nb_*` slot, and python never looks one up by
    // name — so a class defining `__or__` without an adapter simply had no `|`. only
    // four of the nine had one. `__pow__` is still absent on purpose: its slot takes the
    // optional modulus, so it is ternary and does not share this shape
    agree(
        "numeric_slots",
        "\
class Bits:
    def __init__(self, v: int) -> None:
        self.v = v

    def __or__(self, other: Bits) -> Bits:
        return Bits(self.v | other.v)

    def __and__(self, other: Bits) -> Bits:
        return Bits(self.v & other.v)

    def __xor__(self, other: Bits) -> Bits:
        return Bits(self.v ^ other.v)

    def __lshift__(self, n: int) -> Bits:
        return Bits(self.v << n)

    def __rshift__(self, n: int) -> Bits:
        return Bits(self.v >> n)

    def __mod__(self, n: int) -> int:
        return self.v % n

    def __floordiv__(self, n: int) -> int:
        return self.v // n

    def __divmod__(self, n: int) -> object:
        return divmod(self.v, n)

    def __ior__(self, other: Bits) -> Bits:
        return Bits(self.v | other.v | 1)

    def __repr__(self) -> str:
        return \"Bits(\" + str(self.v) + \")\"


def in_place(a: int, b: int) -> object:
    c = Bits(a)
    c |= Bits(b)
    return c
",
        &[
            "[repr(m.Bits(12) | m.Bits(10)), repr(m.Bits(12) & m.Bits(10))]",
            "[repr(m.Bits(12) ^ m.Bits(10)), repr(m.Bits(3) << 2), repr(m.Bits(12) >> 2)]",
            "[m.Bits(12) % 5, m.Bits(12) // 5, divmod(m.Bits(12), 5)]",
            "repr(m.in_place(4, 10))",
        ],
    );
}

#[test]
fn a_dunder_python_looks_up_by_name_agrees() {
    // a dunder is special to the emitter only when python reads it out of a *type
    // slot*: `repr(x)` reads `tp_repr` and never consults the name, so that one needs an
    // adapter. `__reduce__`, `__format__`, `__copy__`, `__sizeof__` and `__dir__` are
    // found by name, which is exactly what the method table already provides — they were
    // declined by a rule that listed what was allowed rather than what was required
    agree(
        "lookup_dunders",
        "\
class Point:
    def __init__(self, x: int) -> None:
        self.x = x

    def __repr__(self) -> str:
        return \"Point(\" + str(self.x) + \")\"

    def __reduce__(self) -> object:
        return (Point, (self.x,))

    def __format__(self, spec: str) -> str:
        return \"P\" + format(self.x, spec)

    def __copy__(self) -> object:
        return Point(self.x)

    def __sizeof__(self) -> int:
        return 42

    def __dir__(self) -> object:
        return [\"x\"]
",
        &[
            "repr(m.Point(7))",
            "format(m.Point(7), \"03d\")",
            "m.Point(7).__sizeof__()",
            "__import__('copy').copy(m.Point(7)).x",
            "__import__('pickle').loads(__import__('pickle').dumps(m.Point(7))).x",
            "sorted(m.Point(7).__dir__())",
        ],
    );
}

#[test]
fn an_unboxed_counter_agrees() {
    // a counter that starts at a literal and only steps by one lives in a register as
    // an `int64_t`. what has to survive that is every *other* thing a program does
    // with it: reading it back out, doing arithmetic that is not the step, and a
    // bound too large for the register — where the comparison boxes the counter and
    // runs the general one, which is the path a short bound never takes
    agree(
        "unboxed_counter",
        "\
def returned(n: int) -> int:
    i = 0
    while i < n:
        i = i + 1
    return i


def squared(n: int) -> int:
    acc = 0
    i = 0
    while i < n:
        acc = acc + i * i
        i = i + 1
    return acc


def huge_bound(n: int) -> int:
    # the bound is far outside the register, so every trip takes the general
    # comparison — and the loop still ends where `n` says
    limit = 10 ** 40
    i = 0
    total = 0
    while i < limit:
        total = total + i
        i = i + 1
        if i >= n:
            return total
    return -1


def stepping_down(n: int) -> int:
    i = 0
    while i > -n:
        i = i - 1
    return i


def escapes_into_a_list(n: int) -> list[int]:
    out = []
    i = 0
    while i < n:
        out.append(i)
        i = i + 2
    return out


def not_a_counter(n: int) -> int:
    # written from a call, so it keeps the tagged representation
    i = int(str(n))
    total = 0
    while i > 0:
        total = total + i
        i = i - 1
    return total


def parameter_bound(limit: int, cap: int) -> int:
    # nothing in the loop writes the bound, so the loop is duplicated and entered
    # through a narrowing. which copy runs depends on the argument, and the two have
    # to agree — a bound past the register takes the general one
    i = 0
    total = 0
    while i < limit:
        total = total + i
        i = i + 1
        if i >= cap:
            break
    return total


def nested_bounds(rows: int, columns: int) -> int:
    # two loops, each with its own invariant bound, so the outer body is duplicated
    # with the inner duplicate already inside it
    total = 0
    y = 0
    while y < rows:
        x = 0
        while x < columns:
            total = total + x * y
            x = x + 1
        y = y + 1
    return total


def bound_reassigned(n: int) -> int:
    # the bound is written *inside* the loop, so it is not invariant and the loop
    # must be left alone: a narrowing taken once would describe a stale value
    limit = n
    i = 0
    while i < limit:
        i = i + 1
        if i * i > n:
            limit = i
    return limit
",
        &[
            "[m.returned(n) for n in (0, 1, 7)]",
            "[m.squared(n) for n in (0, 1, 10, 50)]",
            "[m.huge_bound(n) for n in (1, 5, 20)]",
            "[m.stepping_down(n) for n in (0, 1, 9)]",
            "[m.escapes_into_a_list(n) for n in (0, 1, 8)]",
            "[m.not_a_counter(n) for n in (1, 4, 30)]",
            "[m.parameter_bound(limit, 5) for limit in (0, 3, 10 ** 40)]",
            "[m.parameter_bound(limit, 10 ** 40) for limit in (0, 4, 9)]",
            "[m.nested_bounds(r, c) for r in (0, 1, 5) for c in (0, 1, 6)]",
            "[m.bound_reassigned(n) for n in (0, 1, 9, 40)]",
        ],
    );
}

#[test]
fn an_indexed_write_keeps_the_buffer_agrees() {
    // `xs[i] = v` *mutates* — it binds no local name. the representation pass walked
    // every name in an assignment target, so the base was recorded as though it had
    // been assigned, and merging that with the buffer widened it back to a real list.
    // a sieve costs 1.75x for it, which no benchmark in the set could see because they
    // only ever read and append
    agree(
        "indexed_write",
        "\
def sieve(limit: int) -> int:
    flags = []
    i = 0
    while i < limit:
        flags.append(True)
        i = i + 1
    count = 0
    n = 2
    while n < limit:
        if flags[n]:
            count = count + 1
            m = n + n
            while m < limit:
                flags[m] = False
                m = m + n
        n = n + 1
    return count


def scaled(n: int) -> float:
    xs = []
    i = 0
    while i < n:
        xs.append(1.5)
        i = i + 1
    j = 0
    while j < n:
        xs[j] = xs[j] * 2.0
        j = j + 1
    return xs[n - 1]


def tuple_target(n: int) -> int:
    xs = []
    i = 0
    while i < n:
        xs.append(0)
        i = i + 1
    # a tuple target still binds `a`, and `xs[0]` still does not
    a, xs[0] = 5, 7
    return a + xs[0]
",
        &[
            "[m.sieve(n) for n in (2, 10, 100, 5000)]",
            "[m.scaled(n) for n in (1, 8)]",
            "m.tuple_target(3)",
        ],
    );
}

#[test]
fn a_counted_loop_indexes_without_a_bounds_check() {
    // `while i < len(A)` with a counting `i` puts every `A[i]` inside it in range, so
    // the read needs no test. everything the guard does *not* prove keeps one — and
    // still raises `IndexError` at the same iteration
    agree(
        "counted",
        "\
def proven(xs: list[float]) -> float:
    out = 0.0
    i = 0
    while i < len(xs):
        out = out + xs[i]
        i = i + 1
    return out


def bounded_by_a_parameter(xs: list[float], n: int) -> float:
    out = 0.0
    i = 0
    while i < n:
        out = out + xs[i]
        i = i + 1
    return out


def rebound(xs: list[float], ys: list[float]) -> float:
    out = 0.0
    i = 0
    while i < len(xs):
        out = out + xs[i]
        xs = ys
        i = i + 1
    return out


def stepped_back(xs: list[float]) -> float:
    out = 0.0
    i = 0
    while i < len(xs):
        out = out + xs[i]
        i = i - 1
        i = i + 2
    return out


def other_array(xs: list[float], ys: list[float]) -> float:
    # the guard proves `xs` and says nothing about `ys`, so only one read loses its
    # check — and a short `ys` still raises
    out = 0.0
    i = 0
    while i < len(xs):
        out = out + xs[i] * ys[i]
        i = i + 1
    return out
",
        &[
            "m.proven([1.0, 2.0, 3.0])",
            "m.proven([])",
            "m.proven([0.5])",
            "m.bounded_by_a_parameter([1.0, 2.0, 3.0], 2)",
            "str(_capture(lambda: m.bounded_by_a_parameter([1.0], 5)))",
            "m.rebound([1.0, 2.0], [9.0, 9.0])",
            "m.stepped_back([1.0, 2.0, 3.0])",
            "m.other_array([1.0, 2.0], [3.0, 4.0])",
            "str(_capture(lambda: m.other_array([1.0, 2.0], [3.0])))",
        ],
    );
}

#[test]
fn a_buffer_reaches_a_callee_without_being_boxed() {
    // one body cannot have two representations, so a function whose `list[T]`
    // parameter never escapes is lowered twice: the boxed edition python reaches,
    // and an unboxed one an in-unit caller with a buffer in hand reaches. the buffer
    // is *borrowed* across that call, which is exactly what never-escapes licenses
    //
    // `.by` rather than `.py` because python's `float` annotation admits an `int`,
    // so a `list[float]` there holds `int | float` and has no unboxed element type
    // to make a buffer of — a `.py` module needs `strict-float` for any of this
    agree(
        "buffercall",
        "\
def total(xs: list[float]) -> float:
    out = 0.0
    i = 0
    while i < len(xs):
        out = out + xs[i]
        i = i + 1
    return out


def doubled(xs: list[float]) -> float:
    i = 0
    while i < len(xs):
        xs[i] = xs[i] * 2.0
        i = i + 1
    return total(xs)


def built(n: int) -> float:
    xs = [0.0]
    i = 0
    while i < n:
        xs.append(i * 0.5)
        i = i + 1
    # the same buffer handed over twice: a callee that took ownership would free it
    return total(xs) + total(xs) + doubled(xs)


def escapes(xs: list[float]) -> object:
    return xs


def handed(n: int) -> object:
    xs = [0.0]
    i = 0
    while i < n:
        xs.append(i * 1.5)
        i = i + 1
    return escapes(xs)


def counted(xs: list[int]) -> int:
    out = 0
    i = 0
    while i < len(xs):
        out = out + xs[i]
        i = i + 1
    return out


def sums(n: int) -> int:
    xs = [0]
    i = 0
    while i < n:
        xs.append(i)
        i = i + 1
    return counted(xs)
",
        &[
            "m.total([1.0, 2.0, 3.0])",
            "m.total([])",
            "m.built(0)",
            "m.built(1)",
            "m.built(100)",
            "m.handed(4)",
            "m.handed(0)",
            "m.sums(10)",
            "m.counted([1, 2, 3])",
            // the boxed edition is still the one python reaches, with its own checks
            "[(type(e).__name__, str(e)) for e in [_capture(lambda: m.total(1))]]",
            "[m.built(n) for n in [0, 1, 2, 3]]",
            "_repeated(lambda: m.built(50), 200)",
        ],
    );
}

#[test]
fn a_generic_list_parameter_earns_an_edition_from_its_call_sites() {
    // a `list[T]` parameter pins no element representation of its own, so the unit's
    // own call sites are what say which editions to build — monomorphisation, keyed
    // by what the callers actually supply. two callers supplying different element
    // types get an edition each, and the call site picks by the representation the
    // argument's register already has
    agree(
        "genericbuffer",
        "\
def total[T: float](xs: list[T]) -> float:
    out = 0.0
    i = 0
    while i < len(xs):
        out = out + xs[i]
        i = i + 1
    return out


def counted[T](xs: list[T]) -> int:
    n = 0
    i = 0
    while i < len(xs):
        n = n + 1
        i = i + 1
    return n


def tallied[T](xs: list[T]) -> int:
    n = 0
    i = 0
    while i < len(xs):
        n = n + 1
        i = i + 1
    return n


def first[T](xs: list[T]) -> T:
    return xs[0]


def floats(n: int) -> float:
    xs = [0.5]
    i = 0
    while i < n:
        xs.append(i * 0.5)
        i = i + 1
    return total(xs) + float(counted(xs)) + first(xs)


def both(n: int) -> int:
    xs = [0.5]
    ys = [False]
    i = 0
    while i < n:
        xs.append(i * 0.5)
        ys.append(i > 2)
        i = i + 1
    # two element types at one callee, which gets an edition each — a `[float]` and
    # a `[bool]`, both routed, and both lists here stay buffers
    return tallied(xs) + tallied(ys)
",
        &[
            "m.floats(0)",
            "m.floats(1)",
            "m.floats(10)",
            "m.both(0)",
            "m.both(5)",
            "m.tallied([1.0, 2.0])",
            "m.tallied([True, False, True])",
            "[m.floats(n) for n in [0, 1, 2, 3]]",
            "m.total([1.0, 2.0])",
            "m.counted([1, 2, 3])",
            "m.tallied([1, 2, 3])",
            "m.first([7.5, 1.0])",
            "_repeated(lambda: m.floats(20), 100)",
        ],
    );
}

#[test]
fn an_attribute_assigned_on_every_path_earns_a_field() {
    // a field is always present, where python raises `AttributeError` for one never
    // written — so the layout may hold what *every* path through `__init__` fills,
    // which is more than what the top level does
    agree_python(
        "everypath",
        "\
class Branched:
    def __init__(self, flag: bool) -> None:
        if flag:
            self.x = 1
        else:
            self.x = 2

    def read(self) -> int:
        return self.x


class Chained:
    def __init__(self, n: int) -> None:
        if n > 10:
            self.tag = 'big'
        elif n > 0:
            self.tag = 'small'
        else:
            self.tag = 'none'

    def read(self) -> str:
        return self.tag


class Early:
    def __init__(self, flag: bool) -> None:
        if flag:
            self.v = 1
            return
        self.v = 2

    def read(self) -> int:
        return self.v


class Guarded:
    # the validate-or-raise shape: the branch that raises produces no object at all,
    # so it has nothing to say about the layout
    def __init__(self, flag: bool) -> None:
        if flag:
            self.v = 1
        else:
            raise ValueError('no')

    def read(self) -> int:
        return self.v
",
        &[
            "(m.Branched(True).read(), m.Branched(False).read())",
            "[m.Chained(n).read() for n in [20, 5, -1]]",
            "(m.Early(True).read(), m.Early(False).read())",
            "m.Guarded(True).read()",
            "[(type(e).__name__, str(e)) for e in [_capture(lambda: m.Guarded(False))]]",
            "[m.Branched(f).x for f in [True, False]]",
        ],
    );
}

#[test]
fn a_local_a_path_may_skip_agrees() {
    // a local bound only inside an `if` is simply absent on the paths that skipped it,
    // and reading one raises. the compiled function carries a byte saying whether it
    // has been written, and the message is the *running* python's — the wording changed
    // in 3.11, so a compiled module has to say what the interpreter beside it says
    agree(
        "maybelocal",
        "\
def picked(flag: bool, n: int) -> int:
    if flag:
        value = n
    return value


def reads_a_loop_binding(n: int) -> int:
    i = 0
    while i < n:
        seen = i
        i = i + 1
    return seen


def guarded(flag: bool, n: int) -> int:
    if flag:
        value = n
    else:
        value = 0
    return value
",
        &[
            "m.picked(True, 5)",
            "m.guarded(False, 5)",
            "m.guarded(True, 5)",
            "[(type(e).__name__,) for e in [_capture(lambda: m.picked(False, 5))]]",
            "[str(e) for e in [_capture(lambda: m.picked(False, 5))]]",
            "[m.reads_a_loop_binding(n) for n in (3,)]",
            "[type(e).__name__ for e in [_capture(m.reads_a_loop_binding, 0)]]",
        ],
    );
}

#[test]
fn an_attribute_a_path_may_skip_is_declined() {
    // an `if` with no `else` leaves a path that assigns nothing, and a struct field
    // has no way to be absent — so this stays interpreted and keeps raising
    agree_with_declines(
        "maybeattr",
        "\
class Partial:
    def __init__(self, flag: bool) -> None:
        self.a = 0
        if flag:
            self.b = 1
",
        &[
            "m.Partial(True).b",
            "m.Partial(False).a",
            "[(type(e).__name__,) for e in [_capture(lambda: m.Partial(False).b)]]",
        ],
    );
}

#[test]
fn a_resumable_frame_may_be_nested() {
    // a nested generator keeps its captures in the *state* object rather than
    // reaching back through the environment: the frame outlives the call that made
    // it, and a copy is what a capture already is
    agree_python(
        "nestedgen",
        "\
from typing import Any


def make(step: int, limit: int) -> Any:
    def counted() -> Any:
        i = 0
        while i < limit:
            yield i * step
            i = i + 1
    return counted


def make_async(step: int) -> Any:
    async def total(n: int) -> int:
        out = 0
        i = 0
        while i < n:
            out = out + i * step
            i = i + 1
        return out
    return total


def make_stream(step: int) -> Any:
    async def streamed(n: int) -> Any:
        i = 0
        while i < n:
            yield i * step
            i = i + 1
    return streamed
",
        &[
            "list(m.make(3, 4)())",
            "list(m.make(2, 0)())",
            // the same closure twice: a state object per call, not one shared
            "[list(g()) for g in [m.make(3, 4)]] + [list(m.make(3, 4)())]",
            "[list(m.make(s, 3)()) for s in [1, 2]]",
            "_run(m.make_async(5)(4))",
            "_run(m.make_async(5)(0))",
            "_run(_drain(m.make_stream(4)(3)))",
            "_run(_drain(m.make_stream(4)(0)))",
        ],
    );
}

#[test]
fn a_generator_sharing_a_cell_is_declined_and_still_runs() {
    // a *shared* capture is a cell both frames write, which a copy cannot be — so
    // this one stays interpreted rather than being quietly given a stale value
    agree_with_declines(
        "sharedcell",
        "\
from typing import Any


def counter(n: int) -> Any:
    seen = 0

    def gen() -> Any:
        nonlocal seen
        i = 0
        while i < n:
            seen = seen + 1
            yield i
            i = i + 1

    def read() -> int:
        return seen

    return (gen, read)
",
        &["_counted(m.counter(3))", "_counted(m.counter(0))"],
    );
}

#[test]
fn an_async_generator_agrees() {
    // `__anext__` hands back an *awaitable*, because the body may `await` before it
    // reaches its next `yield`. one resume method serves both, and a field records
    // which of the two suspensions just happened: a yield finishes that awaitable
    // with the item, an await has to reach the event loop instead
    agree_python(
        "asyncgen",
        "\
from typing import Any


async def counted(n: int) -> Any:
    i = 0
    while i < n:
        yield i * 2
        i = i + 1


async def waited(source: Any, n: int) -> Any:
    i = 0
    while i < n:
        v = await source(i)
        yield v + 1
        i = i + 1


async def guarded(log: list[str], n: int) -> Any:
    try:
        i = 0
        while i < n:
            yield i
            i = i + 1
    except ValueError:
        log.append('caught')
    finally:
        log.append('closed')


async def echoed(n: int) -> Any:
    i = 0
    while i < n:
        got = yield i
        if got is not None:
            yield str(got)
        i = i + 1
",
        &[
            "_run(_drain(m.counted(4)))",
            "_run(_drain(m.counted(0)))",
            "_run(_drain(m.counted(1)))",
            "_run(_comprehended(m.counted(3)))",
            "_run(_awaited(m.waited, 3))",
            "_run(_stepped(m.counted(1)))",
            "_run(_closed_async(m.guarded, 5))",
            "_run(_drained_with_log(m.guarded, 2))",
            "_run(_echoed(m.echoed(3)))",
            "_run(_thrown(m.guarded, ValueError))",
            "_run(_thrown(m.guarded, KeyError))",
        ],
    );
}

#[test]
fn a_generator_or_coroutine_may_be_a_method() {
    // the state object holds `self` like any other parameter, so the body reads
    // fields through it exactly as a plain method does. the state *class* is
    // namespaced by the receiver's, because two classes may each have a `values`
    agree_python(
        "genmethod",
        "\
class Counter:
    def __init__(self, limit: int) -> None:
        self.limit = limit

    async def total(self) -> int:
        n = 0
        i = 0
        while i < self.limit:
            n = n + i
            i = i + 1
        return n

    def values(self) -> object:
        i = 0
        while i < self.limit:
            yield i * 2
            i = i + 1

    def __iter__(self) -> object:
        return iter(self.values())


class Other:
    def __init__(self, tag: str) -> None:
        self.tag = tag

    def values(self) -> object:
        yield self.tag
        yield self.tag + '!'
",
        &[
            "_run(m.Counter(5).total())",
            "list(m.Counter(5).values())",
            "list(m.Counter(0).values())",
            "list(m.Counter(3))",
            "list(m.Other('x').values())",
            "[list(a) for a in [m.Counter(2).values(), m.Other('y').values()]]",
            "[next(m.Counter(3).values())]",
            "[list(c.values()) for c in [m.Counter(1), m.Counter(2)]]",
        ],
    );
}

#[test]
fn a_compiled_class_may_be_a_context_manager_or_an_async_iterator() {
    // `__enter__` and its three relatives are not slots: python reaches them by an
    // ordinary type lookup, which finds the method table without an adapter
    agree_python(
        "ctxclass",
        "\
from typing import Any


class Sync:
    def __init__(self, log: list[Any]) -> None:
        self.log = log

    def __enter__(self) -> str:
        self.log.append('enter')
        return 'held'

    def __exit__(self, kind: object, value: object, tb: object) -> bool:
        self.log.append(('exit', kind))
        return False


class Async:
    def __init__(self, log: list[Any]) -> None:
        self.log = log

    async def __aenter__(self) -> str:
        self.log.append('enter')
        return 'held'

    async def __aexit__(self, kind: object, value: object, tb: object) -> bool:
        self.log.append(('exit', kind))
        return False


class Range:
    def __init__(self, limit: int) -> None:
        self.limit = limit
        self.at = 0

    def __aiter__(self) -> object:
        return self

    async def __anext__(self) -> int:
        if self.at >= self.limit:
            raise StopAsyncIteration
        self.at = self.at + 1
        return self.at


def uses(manager: Any, log: list[Any]) -> str:
    with manager as name:
        log.append('body ' + str(name))
        return 'returned'
    return 'suppressed'


async def awaits(manager: Any, log: list[Any]) -> str:
    async with manager as name:
        log.append('body ' + str(name))
        return 'returned'
    return 'suppressed'


async def drains(source: Any) -> list[int]:
    out: list[int] = []
    async for v in source:
        out.append(v)
    return out
",
        &[
            "_own(lambda log: m.uses(m.Sync(log), log))",
            "_own(lambda log: _run(m.awaits(m.Async(log), log)))",
            "_run(m.drains(m.Range(4)))",
            "_run(m.drains(m.Range(0)))",
            "_own(lambda log: [m.uses(m.Sync(log), log), m.uses(m.Sync(log), log)])",
        ],
    );
}

#[test]
fn leaving_a_context_early_runs_its_exit_with_no_exception() {
    // a resumable frame's `return` *is* a `StopIteration`, so raising it under the
    // enclosing `with`'s error target handed that exception to `__exit__` as though
    // the block had failed. the cleanups run first now, and the raise leaves without
    // a target of its own
    agree_python(
        "earlyexit",
        "\
from typing import Any


def returned(manager: Any, log: list[str], n: int) -> Any:
    with manager as name:
        i = 0
        while i < n:
            yield str(name) + str(i)
            i = i + 1
        return 'done'


def bare(manager: Any, log: list[str]) -> Any:
    with manager as name:
        yield str(name)
        return


async def returns(manager: Any, log: list[str]) -> str:
    async with manager as name:
        log.append('body ' + str(name))
        return 'returned'


async def falls(manager: Any, log: list[str]) -> str:
    async with manager as name:
        log.append('body ' + str(name))
    return 'fell'


async def raises(manager: Any, log: list[str]) -> str:
    async with manager as name:
        log.append('body ' + str(name))
        raise ValueError('boom')


async def breaks(manager: Any, log: list[str], n: int) -> str:
    i = 0
    while i < n:
        async with manager as name:
            log.append('body ' + str(name) + str(i))
            if i == 1:
                break
        i = i + 1
    return 'broke'


async def nested(outer: Any, inner: Any, log: list[str]) -> str:
    async with outer as a:
        async with inner as b:
            log.append('body ' + str(a) + str(b))
            return 'deep'


async def pair(manager: Any, log: list[str]) -> str:
    async with manager as a, manager as b:
        log.append('body ' + str(a) + str(b))
        return 'pair'
",
        &[
            "_logged(lambda t, log: list(m.returned(t, log, 2)))",
            "_logged(lambda t, log: _value(m.returned(t, log, 2)))",
            "_logged(lambda t, log: _value(m.bare(t, log)))",
            "_logged(lambda t, log: _run(m.returns(t, log)))",
            "_logged(lambda t, log: _run(m.falls(t, log)))",
            "_logged(lambda t, log: _run(m.raises(t, log)))",
            "_logged(lambda t, log: _run(m.breaks(t, log, 4)))",
            "_logged(lambda t, log: _run(m.pair(t, log)))",
            "_nested(lambda a, b, log: _run(m.nested(a, b, log)))",
        ],
    );
}

#[test]
fn the_in_place_arithmetic_slots_agree() {
    // `a += b` rebinds `a` to whatever the method returned, so returning `self` and
    // returning a fresh object are both correct and both have to work — the identity
    // is the observation. a class with no in-place form falls back to the binary one,
    // which python rebinds the same way
    agree_python(
        "inplace",
        "\
class Acc:
    def __init__(self, total: int) -> None:
        self.total = total

    def __iadd__(self, other: int) -> object:
        self.total = self.total + other
        return self

    def __isub__(self, other: int) -> object:
        return Acc(self.total - other)

    def __imul__(self, other: int) -> object:
        self.total = self.total * other
        return self

    def __itruediv__(self, other: int) -> object:
        return Acc(self.total // other)

    def __repr__(self) -> str:
        return 'Acc(' + str(self.total) + ')'


class Plain:
    def __init__(self, total: int) -> None:
        self.total = total

    def __add__(self, other: int) -> object:
        return Plain(self.total + other)

    def __repr__(self) -> str:
        return 'Plain(' + str(self.total) + ')'


def kept(start: int, by: int) -> object:
    a = Acc(start)
    before = a
    a += by
    return (a, a is before)


def replaced(start: int, by: int) -> object:
    a = Acc(start)
    before = a
    a -= by
    return (a, a is before)


def fallback(start: int, by: int) -> object:
    p = Plain(start)
    before = p
    p += by
    return (p, p is before)


def extended(n: int) -> object:
    xs = [1]
    before = xs
    xs += [n]
    return (xs, xs is before)


def concatenated(tail: str) -> object:
    s = 'a'
    s += tail
    return s


def numeric(n: int, f: float) -> object:
    n += 2
    f += 0.5
    return (n, f)
",
        &[
            "m.kept(10, 5)",
            "m.replaced(10, 3)",
            "m.fallback(1, 2)",
            "_inplace(lambda a: a.__imul__(3))",
            "_inplace(lambda a: a.__itruediv__(2))",
            // a list of them, so the slot is reached through more than one shape
            "[m.kept(n, 1) for n in [0, 1, 2]]",
            "[(type(e).__name__,) for e in [_capture(lambda: m.Plain(1).__isub__(1))]]",
            // a `list` extends *itself*, which the plain binary operation would not
            "m.extended(2)",
            "m.concatenated('b')",
            "m.numeric(1, 1.5)",
        ],
    );
}

#[test]
fn the_unary_and_call_slots_agree() {
    // a slot cannot be filled from the method table, so each of these is installed
    // twice — and `tp_call` is the one that has to *bind*, because it is handed a
    // tuple and a dict where the method wrapper wants a vector
    agree_python(
        "unarycall",
        "\
class Money:
    def __init__(self, cents: int) -> None:
        self.cents = cents

    def __neg__(self) -> object:
        return Money(-self.cents)

    def __pos__(self) -> object:
        return Money(self.cents)

    def __abs__(self) -> object:
        return Money(abs(self.cents))

    def __invert__(self) -> int:
        return ~self.cents

    def __repr__(self) -> str:
        return 'Money(' + str(self.cents) + ')'


class Adder:
    def __init__(self, base: int) -> None:
        self.base = base

    def __call__(self, x: int, step: int = 1, *rest: int) -> int:
        return self.base + x * step + len(rest)
",
        &[
            "-m.Money(5)",
            "+m.Money(-5)",
            "abs(m.Money(-7))",
            "~m.Money(6)",
            "m.Money(4).__neg__()",
            "callable(m.Adder(1))",
            "m.Adder(10)(1)",
            "m.Adder(10)(2, 3)",
            "m.Adder(10)(x=4)",
            "m.Adder(10)(2, step=5)",
            "m.Adder(10)(2, 3, 4, 5)",
            "m.Adder(10).__call__(1)",
            "[f(2) for f in [m.Adder(1), m.Adder(2)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(lambda: m.Adder(1)())]]",
            "[(type(e).__name__, str(e)) for e in [_capture(lambda: m.Adder(1)(1, bad=2))]]",
        ],
    );
}

#[test]
fn an_arity_error_counts_the_receiver() {
    // python describes the *function*, whose first parameter is `self`, rather than
    // the call the caller wrote — so a method's counts are one higher than its
    // wrapper binds
    agree_python(
        "arity",
        "\
class Box:
    def __init__(self) -> None:
        self.n = 0

    def take(self, x: int, step: int = 1) -> int:
        return x + step

    def one(self, x: int) -> int:
        return x


def free(x: int, step: int = 1) -> int:
    return x + step
",
        &[
            "str(_capture(lambda: m.Box().take(1, 2, 3)))",
            "str(_capture(lambda: m.Box().one(1, 2)))",
            "str(_capture(lambda: m.Box.take(m.Box(), 1, 2, 3)))",
            "str(_capture(lambda: m.Box().one()))",
            "str(_capture(lambda: m.free(1, 2, 3)))",
            "str(_capture(lambda: m.free()))",
        ],
    );
}

#[test]
fn a_with_inside_a_generator_agrees() {
    // the manager has to survive every suspension the body makes, so it lives in a
    // field — and `__exit__` runs however the frame leaves: off the end, through
    // `close()`, or by being *abandoned*, which python finalizes by closing
    agree_python(
        "genwith",
        "\
def stepped(manager: object, n: int) -> object:
    with manager as name:
        i = 0
        while i < n:
            yield str(name) + str(i)
            i = i + 1


def unstarted(bad: object) -> object:
    n = len(bad)
    yield n


def guarded(log: list[str], n: int) -> object:
    try:
        i = 0
        while i < n:
            yield i
            i = i + 1
    finally:
        log.append('closed')
",
        &[
            "_traced(lambda t: list(m.stepped(t, 3)))",
            "_traced(lambda t: list(m.stepped(t, 0)))",
            // stopped part way and closed: `__exit__` still runs
            "_traced(lambda t: _closed(m.stepped(t, 5), 2))",
            // and abandoned, which python finalizes by closing
            "_traced(lambda t: _abandoned(m.stepped(t, 5), 2))",
            "_traced(lambda t: _raised(m.stepped(t, 5)))",
            "(lambda log: (list(m.guarded(log, 3)), log))([])",
            "(lambda log: (_abandoned(m.guarded(log, 5), 2), log))([])",
            // a generator that raised before ever suspending has nothing to unwind:
            // finalizing it must not run the body a second time
            "type(_capture(lambda: next(m.unstarted(5)))).__name__",
            "_discarded(m.unstarted(5))",
            "_discarded(m.unstarted([1, 2]))",
        ],
    );
}

/// an exception leaving a generator's frame finishes it, exactly as python does
///
/// the cleanup the exception unwound on its way out has already run, so the machine
/// must not look suspended afterwards: finalizing one that does resumes it and runs
/// every `finally` and every `__exit__` a second time
#[test]
fn an_exception_out_of_a_generator_finishes_it() {
    agree_python(
        "genfinished",
        "\
def guarded(log: list[str], n: int) -> object:
    try:
        i = 0
        while i < n:
            yield i
            i = i + 1
    finally:
        log.append('closed')


def held(manager: object, n: int) -> object:
    with manager as name:
        i = 0
        while i < n:
            yield str(name) + str(i)
            i = i + 1
",
        &[
            "(lambda log: (_after_raising(m.guarded(log, 5)), log))([])",
            "_traced(lambda t: _after_raising(m.held(t, 5)))",
            // the same question the other way round: an exhausted generator is
            // finished too, and closing or dropping one runs nothing more
            "(lambda log: (list(m.guarded(log, 3)), log))([])",
        ],
    );
}

#[test]
fn async_comprehensions_agree() {
    // the same machine `async for` is, driving the rest of the comprehension where
    // the statement form drives a body. the accumulator has to survive every
    // suspension too, so it lives in a field and the register is narrowed back from
    // the object one hands out
    agree_python(
        "asynccomp",
        "\
async def seen(source: object) -> set[int]:
    return {v async for v in source}


async def mapped(source: object) -> dict[int, int]:
    return {v: v * 2 async for v in source}


async def filtered(source: object, limit: int) -> set[int]:
    return {v * 2 async for v in source if v > limit}


async def listed(source: object) -> list[int]:
    return [v async for v in source]
",
        &[
            "sorted(_run(m.seen(_counter(4))))",
            "sorted(_run(m.seen(_counter(0))))",
            "_run(m.mapped(_counter(3)))",
            "sorted(_run(m.filtered(_counter(5), 2)))",
            "sorted(_run(m.filtered(_counter(2), 9)))",
            "_run(m.listed(_counter(4)))",
            "_run(m.listed(_counter(0)))",
            "_capture_async(m.seen, 5)",
        ],
    );
}

#[test]
fn async_for_agrees() {
    // `__aiter__` hands back the iterator without awaiting, and each step is
    // `await it.__anext__()`. the loop ends when that raises
    // `StopAsyncIteration`, which is why the step runs under an error target:
    // unlike `__next__` there is no sentinel to test, and the end surfaces *after*
    // a suspension rather than before one.
    //
    // the iterable comes from the caller because an async iterator needs
    // `async def __anext__`, and a coroutine *method* is its own open gap
    agree_python(
        "asyncfor",
        "\
async def total(source: object) -> int:
    out = 0
    async for v in source:
        out = out + v
    return out


async def early(source: object, limit: int) -> int:
    out = 0
    async for v in source:
        if v > limit:
            break
        out = out + v
    else:
        out = out + 100
    return out
",
        &[
            "_run(m.total(_counter(4)))",
            "_run(m.total(_counter(0)))",
            // the `else` runs when nothing broke out
            "_run(m.early(_counter(5), 2))",
            "_run(m.early(_counter(2), 9))",
            // anything that is not `StopAsyncIteration` goes on out
            "_capture_async(m.total, _Boom())",
            // and the errors are `async for`'s own, not an attribute lookup's
            "_capture_async(m.total, 5)",
            "_capture_async(m.total, _NoNext())",
        ],
    );
}

#[test]
fn a_list_built_by_appending_earns_a_buffer() {
    // an empty display says nothing about its elements, so the checker does — and
    // the escape check still governs: a buffer that leaves the function it was
    // built in would have to be *copied*, and a copy is a different list
    agree(
        "appendbuffer",
        "\
def running(xs: list[float]) -> float:
    out = []
    total = 0.0
    for x in xs:
        total = total + x
        out.append(total)
    return out[len(out) - 1]

def counted(n: int) -> int:
    seen = []
    i = 0
    while i < n:
        seen.append(i * 2)
        i = i + 1
    return len(seen)

def escapes(n: int) -> list[int]:
    out = []
    i = 0
    while i < n:
        out.append(i)
        i = i + 1
    return out

def at(n: int, i: int) -> int:
    out = []
    k = 0
    while k < n:
        out.append(k * 10)
        k = k + 1
    return out[i]

def grown(n: int) -> int:
    out = []
    i = 0
    while i < n:
        out.append(i)
        i = i + 1
    return len(out) + out[len(out) - 1]
",
        &[
            "m.running([1.5, 2.5, 3.5])",
            "m.running([1.0])",
            "str(_capture(m.running, []))",
            "m.counted(5)",
            "m.counted(0)",
            // returned, so it never earns a buffer — a copy would be a different list
            "m.escapes(4)",
            "m.escapes(0)",
            "(lambda v: (v, v is m.escapes(3)))(m.escapes(3))",
            // a buffer indexes exactly as the list it stands in for does
            "m.at(4, 0)",
            "m.at(4, 3)",
            "m.at(4, -1)",
            "m.at(4, -4)",
            "str(_capture(m.at, 4, 4))",
            "str(_capture(m.at, 4, -5))",
            "str(_capture(m.at, 0, 0))",
            // past the growth threshold, several times over
            "m.grown(1)",
            "m.grown(9)",
            "m.grown(1000)",
        ],
    );
}

#[test]
fn a_conjunction_pattern_agrees() {
    // basedpython's `case P and Q:` — every one has to match the *same* subject.
    // it is the mirror of `P | Q` and needs no restriction on what the alternatives
    // bind, because all of them run
    agree(
        "andpattern",
        "\
def kind(v: object) -> str:
    match v:
        case [a, b] and [1, x]:
            return 'one then ' + str(x) + ' of ' + str(a) + ',' + str(b)
        case [a, b]:
            return 'pair ' + str(a) + ',' + str(b)
        case int() and n:
            return 'int ' + str(n)
        case _:
            return 'other'
",
        &[
            "[m.kind(v) for v in [[1, 2], [3, 4], [1], [1, 2, 3]]]",
            "[m.kind(v) for v in [7, True, 'x', None, 1.5]]",
            "[m.kind(v) for v in [(1, 9), (2, 9)]]",
        ],
    );
}

#[test]
fn a_nested_function_may_be_decorated_or_generic() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 12))) {
        return;
    }
    // a decorator on a nested function wraps the closure where the `def` stands,
    // outermost last — the same order the statement itself applies them. the name
    // is resolved the way the module-level ones are, as `LOAD_GLOBAL` would.
    //
    // a type parameter is erased here as anywhere else, so a generic nested
    // function needed nothing beyond dropping the decline
    agree_python(
        "nesteddec",
        "\
from typing import Callable


def twice(fn: Callable[[int], int]) -> Callable[[int], int]:
    def wrapper(x: int) -> int:
        return fn(x) + fn(x)
    return wrapper


def shout(fn: Callable[[int], int]) -> Callable[[int], str]:
    def wrapper(x: int) -> str:
        return str(fn(x)) + '!'
    return wrapper


def offset(n: int) -> Callable[[int], int]:
    @twice
    def inner(x: int) -> int:
        return x + n

    return inner


def stacked(n: int) -> Callable[[int], str]:
    @shout
    @twice
    def inner(x: int) -> int:
        return x + n

    return inner


def generic_inner(n: int) -> int:
    def echo[T](x: T) -> T:
        return x

    return echo(n) + echo(1)


def counted(n: int) -> list[int]:
    out = []
    for i in range(n):
        @twice
        def each(x: int) -> int:
            return x + i

        out.append(each(0))
    return out
",
        &[
            "m.offset(10)(1)",
            "m.offset(0)(5)",
            "m.stacked(10)(1)",
            "m.stacked(0)(5)",
            "m.generic_inner(5)",
            "m.generic_inner(0)",
            // each iteration decorates its own closure
            "m.counted(4)",
            "m.counted(0)",
        ],
    );
}

#[test]
fn generic_functions_and_classes_agree() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 12))) {
        return;
    }
    // a type parameter is erased at runtime: `T` is an object like any other type
    // the mapper cannot narrow, and the body is the same code either way. what is
    // *not* erased is the namespace — `Box[int]` has to keep working, and a type
    // built in C answers that through `__class_getitem__`.
    //
    // the alias *class* differs and is not compared: this hands back the
    // `types.GenericAlias` a builtin generic gives, where `class Box[T]:` inherits
    // `Generic`'s and gives a `typing._GenericAlias`. everything either is asked —
    // `get_origin`, `get_args`, `__origin__`, `str` — answers the same
    agree_python(
        "generics",
        "\
def first[T](xs: list[T]) -> T:
    return xs[0]


def pair[A, B](a: A, b: B) -> tuple[A, B]:
    return (a, b)


class Box[T]:
    def __init__(self, value: T) -> None:
        self.value = value

    def get(self) -> T:
        return self.value

    def replaced(self, other: T) -> T:
        old = self.value
        self.value = other
        return old


class Pair[A, B]:
    def __init__(self, a: A, b: B) -> None:
        self.a = a
        self.b = b

    def swapped(self) -> str:
        return str(self.b) + str(self.a)
",
        &[
            "m.first([1, 2, 3])",
            "m.first(['a', 'b'])",
            "m.pair(1, 'x')",
            "type(_capture(m.first, [])).__name__",
            "(lambda b: (b.get(), b.replaced('x'), b.get()))(m.Box(5))",
            "m.Box(5).value",
            "m.Pair(1, 'y').swapped()",
            // the alias is the same one `list[int]` produces
            "str(m.Box[int])",
            "str(m.Pair[int, str])",
            "__import__('typing').get_args(m.Box[int])",
            "__import__('typing').get_origin(m.Box[int]) is m.Box",
            "__import__('typing').get_args(m.Pair[int, str])",
            "m.Box[int].__origin__ is m.Box",
            "type(m.Box(1)).__name__",
        ],
    );
}

#[test]
fn mapping_patterns_agree() {
    // `case {}:` matches *any* mapping rather than an empty one — a mapping
    // pattern names the keys it cares about and ignores the rest, which is the
    // opposite of how a sequence pattern reads. `**rest` is what is left, as a
    // plain dict whatever the subject was
    agree_python(
        "mappings",
        "\
def route(v: object) -> str:
    match v:
        case {'kind': 'user', 'id': n}:
            return 'user ' + str(n)
        case {'kind': k, **rest}:
            return k + ' with ' + str(sorted(rest.items()))
        case {}:
            return 'any mapping'
        case _:
            return 'other'


def nested(v: object) -> str:
    match v:
        case {'at': [x, y]}:
            return str(x) + ',' + str(y)
        case {'at': {'x': x}}:
            return 'x is ' + str(x)
        case _:
            return 'no'
",
        &[
            "[m.route(v) for v in [{'kind': 'user', 'id': 7}, {'kind': 'admin', 'x': 1, 'y': 2}]]",
            "[m.route(v) for v in [{'kind': 'bare'}, {}, {'a': 1}]]",
            "[m.route(v) for v in [[1], 'x', None, 5, (1, 2), {1, 2}]]",
            "m.route({'kind': 'user'})",
            "m.route({'id': 7})",
            "m.nested({'at': [1, 2]})",
            "m.nested({'at': {'x': 9}})",
            "m.nested({'at': 'ab'})",
            "m.nested({})",
            // the rest dict is a copy, so mutating it cannot reach the subject
            "(lambda d: (m.route(d), sorted(d.items())))({'kind': 'k', 'a': 1})",
        ],
    );
}

#[test]
fn positional_class_patterns_agree() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        return;
    }
    // a positional sub-pattern names its attribute through the class's
    // `__match_args__`, which only exists at runtime — and the builtins that have
    // none match the subject *whole*, so `case int(n):` binds the int itself.
    //
    // an attribute the subject simply does not have is **no match**, not an error:
    // `case Point(z=1):` falls through to the next case
    agree_python(
        "positional",
        "\
class Point:
    __match_args__ = ('x', 'y')
    origin_name = 'origin'
    dimensions = 2

    def __init__(self, x: int, y: int) -> None:
        self.x = x
        self.y = y


def where(v: object) -> str:
    match v:
        case Point(0, 0):
            return 'origin'
        case Point(0, b):
            return 'y axis at ' + str(b)
        case Point(a, b):
            return 'at ' + str(a) + ',' + str(b)
        case int(n):
            return 'the int ' + str(n)
        case str(s):
            return 'the str ' + s
        case _:
            return 'other'


def missing(v: object) -> str:
    match v:
        case Point(z=1):
            return 'has z'
        case Point():
            return 'a point'
        case _:
            return 'other'


def mixed(v: object) -> str:
    match v:
        case Point(0, y=n):
            return 'y is ' + str(n)
        case _:
            return 'no'
",
        &[
            "[m.where(v) for v in [m.Point(0, 0), m.Point(0, 5), m.Point(3, 4)]]",
            "[m.where(v) for v in [7, 'hi', None, [1], 1.5, True]]",
            "[m.missing(v) for v in [m.Point(1, 2), 7, None]]",
            "[m.mixed(v) for v in [m.Point(0, 9), m.Point(1, 9), 7]]",
            // a class-level constant reaches the compiled type, whatever the
            // expression was — the interpreted definition evaluated it already
            "m.Point.__match_args__",
            "m.Point.origin_name",
            "m.Point.dimensions",
            "m.Point(1, 2).dimensions",
            "type(m.Point(1, 2)).__name__",
        ],
    );
}

#[test]
fn sequence_and_class_patterns_agree() {
    // a sequence pattern matches what the interpreter's own `MATCH_SEQUENCE`
    // matches — a type *flagged* as a sequence, which `str`, `bytes` and
    // `bytearray` deliberately are not, and `range` is. an element is read once,
    // because binding it separately would run `__getitem__` twice
    agree_python(
        "patterns",
        "\
class Point:
    def __init__(self, x: int, y: int) -> None:
        self.x = x
        self.y = y


def shape(v: object) -> str:
    match v:
        case []:
            return 'empty'
        case [a]:
            return 'one ' + str(a)
        case [1, b]:
            return 'starts one, then ' + str(b)
        case [a, b]:
            return 'pair ' + str(a) + ',' + str(b)
        case Point(x=0, y=0):
            return 'origin'
        case Point(x=0):
            return 'on the y axis'
        case Point():
            return 'a point'
        case _:
            return 'other'


def split(v: object) -> str:
    match v:
        case [first, *rest]:
            return str(first) + ' then ' + str(rest)
        case _:
            return 'no'


def ends(v: object) -> str:
    match v:
        case [a, *middle, b]:
            return str(a) + '|' + str(middle) + '|' + str(b)
        case [*only]:
            return 'all ' + str(only)
        case _:
            return 'no'


def nested(v: object) -> str:
    match v:
        case [[a, b], c]:
            return str(a) + str(b) + str(c)
        case [Point(x=n)]:
            return 'point at ' + str(n)
        case _:
            return 'no'
",
        &[
            "[m.shape(v) for v in [[], [5], [1, 2], [3, 4], [1, 2, 3]]]",
            "[m.shape(v) for v in [m.Point(0, 0), m.Point(0, 9), m.Point(1, 2)]]",
            // a string is not a sequence pattern's subject, however indexable
            "[m.shape(v) for v in ['ab', b'ab', bytearray(b'ab'), 'a']]",
            "[m.shape(v) for v in [(1, 2), (3, 4), range(2), 7, None]]",
            "m.nested([[1, 2], 3])",
            "m.nested([m.Point(4, 5)])",
            "m.nested([m.Point(4, 5), 1])",
            "m.nested('abc')",
            // a star binds a *list*, whatever the subject was
            "[m.split(v) for v in [[], [1], [1, 2], [1, 2, 3], (1, 2, 3)]]",
            "[m.ends(v) for v in [[], [1], [1, 2], [1, 2, 3], (1, 2, 3)]]",
            "[m.split(v) for v in ['abc', range(3), 5, b'ab']]",
            "[m.ends(v) for v in ['abc', range(3), 5]]",
        ],
    );
}

#[test]
fn a_match_statement_agrees() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        return;
    }
    // a case is a test and a set of bindings, and the two are separate: python
    // binds before it evaluates a guard and leaves the binding behind when the
    // guard then fails. a singleton pattern is an *identity* test, so `0` does not
    // match `case False:` however equal the two are
    agree_python(
        "matchstmt",
        "\
def name(code: int) -> str:
    match code:
        case 0:
            return 'zero'
        case 1 | 2 | 3:
            return 'small'
        case n if n < 0:
            return 'negative ' + str(n)
        case n:
            return 'big ' + str(n)


def kind(v: object) -> str:
    match v:
        case None:
            return 'none'
        case True:
            return 'yes'
        case False:
            return 'no'
        case 'x':
            return 'the letter x'
        case _:
            return 'other'


def bound(v: object) -> str:
    out = 'none'
    match v:
        case x if isinstance(x, int) and x > 10:
            out = 'big ' + str(x)
        case x:
            out = 'kept ' + str(x)
    return out


def falls_through(v: int) -> str:
    out = 'unset'
    match v:
        case 1:
            out = 'one'
        case 2:
            out = 'two'
    return out
",
        &[
            "[m.name(c) for c in [0, 1, 2, 3, 4, -5, 100]]",
            "[m.kind(v) for v in [None, True, False, 'x', 'y', 0, 1, [], 1.0]]",
            "[m.bound(v) for v in [11, 5, 'a', None]]",
            "[m.falls_through(v) for v in [1, 2, 3]]",
            "m.name(-1)",
            "m.kind(0.0)",
            "m.kind('xx')",
        ],
    );
}

#[test]
fn identity_is_not_equality() {
    agree_python(
        "identity",
        "\
def same(a: object, b: object) -> bool:
    return a is b

def different(a: object, b: object) -> bool:
    return a is not b
",
        &[
            "m.same(None, None)",
            "m.same(0, False)",
            "m.same(1, True)",
            "m.same('a', 'a')",
            "m.different(0, False)",
            "m.different([], [])",
            "m.same([], [])",
            "(lambda x: m.same(x, x))([1])",
        ],
    );
}

#[test]
fn a_computed_parameter_default_agrees() {
    // python evaluates a default once, at definition time — which is what makes a
    // mutable one shared by every call that omits it. the interpreted definition
    // already did that and holds the object, so a call omitting the parameter is
    // handed to it rather than given a second object no other call sees
    agree_python(
        "computeddefault",
        "\
LIMIT = 10


def bump(xs: list[int] = [], n: int = LIMIT) -> list[int]:
    xs.append(n)
    return xs


def twice(n: int = LIMIT * 2) -> int:
    return n


def joined(parts: list[str] = ['a'], sep: str = '-'.join(['x', 'y'])) -> str:
    return sep.join(parts)


def caller() -> int:
    return twice() + twice(1)
",
        &[
            "m.twice()",
            "m.twice(1)",
            "m.twice(n=5)",
            "m.caller()",
            // the same list, every time, growing
            "[m.bump(), m.bump(), m.bump()]",
            "m.bump([9])",
            "m.bump([9], 1)",
            "m.bump(n=3)",
            "m.bump(xs=[1], n=2)",
            "m.joined()",
            "m.joined(['p', 'q'])",
            "m.joined(['p', 'q'], '+')",
            "m.joined(sep='!')",
            "type(_capture(m.twice, 1, 2)).__name__",
        ],
    );
}

#[test]
fn a_constructors_computed_default_agrees() {
    // the constructor is a boundary of its own — `tp_init` binds from a tuple and a
    // dict rather than from a vector — and marking such a parameter *required* there
    // made `Holder()` an arity error against a definition that has no such error
    agree_python(
        "computedinit",
        "\
_sentinel = object()

SHARED = [0]


class Holder:
    def __init__(self, value=_sentinel):
        self.value = value


class Grower:
    def __init__(self, tag: str, seen=SHARED):
        self.tag = tag
        self.seen = seen
        seen.append(len(seen))
",
        &[
            // the object the interpreted definition already evaluated, not a second one
            "m.Holder().value is m._sentinel",
            "m.Holder(1).value",
            "m.Holder(value=2).value",
            // the same list, every time, growing — the whole point of evaluating once
            "[m.Grower('a').seen, m.Grower('b').seen, m.Grower('c', []).seen]",
            "m.Grower('d').seen is m.SHARED",
            "m.Grower('e', ['x']).tag",
            "m.Grower(tag='f', seen=['y']).seen",
            // the arity is still the written one in both directions
            "type(_capture(m.Holder, 1, 2)).__name__",
            "type(_capture(m.Grower)).__name__",
        ],
    );
}

#[test]
fn a_constructor_that_defers_is_still_the_compiled_type() {
    // `agree` cannot tell which build answered: a class that fell back to its
    // interpreted definition agrees with itself. `wrapper_descriptor` is what says
    // the slot is filled, so the boundary under test is the one that ran
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_computedinitslot");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
_sentinel = object()


class Holder:
    def __init__(self, value=_sentinel):
        self.value = value
";
    let built = match build_source(
        source,
        "by_diff_computedinitslot",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_computedinitslot as m\n\
         print(m.Holder().value is m._sentinel, m.Holder(1).value)\n\
         print(type(m.Holder.__init__).__name__)\n",
    );
    assert_eq!(out, "True 1\nwrapper_descriptor");
}

#[test]
fn a_positional_only_constructor_still_fills_its_instance() {
    // a `/` moves the receiver into the positional-only list, and reading slot zero
    // off the first *ordinary* parameter instead found no attribute assignment at
    // all — the class laid out nothing and `__init__` had nowhere to store
    agree_python(
        "posonlyinit",
        "\
class Thing:
    def __init__(self, a, /, b=1):
        self.a = a
        self.b = b


class Pair:
    def __init__(self, left, right, /):
        self.left = left
        self.right = right
",
        &[
            "(m.Thing(1).a, m.Thing(1).b)",
            "(m.Thing(2, 3).a, m.Thing(2, 3).b)",
            "(m.Pair('l', 'r').left, m.Pair('l', 'r').right)",
            // and the arity is still the written one in both directions
            "_capture(m.Thing)",
            "_capture(m.Thing, 1, 2, 3)",
        ],
    );
}

#[test]
fn a_variadic_constructor_agrees() {
    // the constructor slot bound `*args` and `**kwargs` as if they were ordinary named
    // parameters: a call that supplied neither was an arity error against a definition
    // that has none, and one that supplied a single positional handed the body that
    // argument rather than the tuple holding it
    agree_python(
        "variadicinit",
        "\
class Var:
    def __init__(self, *names):
        self.names = names


class Kw:
    def __init__(self, **options):
        self.options = options


class Slot:
    def __init__(self, a, /, *rest, tag='t', **extra):
        self.a = a
        self.rest = rest
        self.tag = tag
        self.extra = extra
",
        &[
            "(m.Var().names, m.Var('x').names, m.Var('x', 'y').names)",
            "(m.Kw().options, m.Kw(x=1).options, m.Kw(options=1).options)",
            "(m.Slot(1).a, m.Slot(1).rest, m.Slot(1).tag, m.Slot(1).extra)",
            "m.Slot(1, 2, 3).rest",
            "(m.Slot(1, 2, tag='z', q=9).tag, m.Slot(1, 2, tag='z', q=9).extra)",
            // `a` is positional-only, so a keyword spelling it lands in `**extra`
            "m.Slot(1, a=2).extra",
            "_capture(m.Slot)",
            "_capture_kw(m.Var, (), {'names': 1})",
        ],
    );
}

#[test]
fn a_positional_only_parameter_is_unreachable_by_name_from_either_boundary() {
    // `posonly` counts the receiver, and a boundary handed that receiver separately
    // has to shift it — leaving it unshifted made the parameter *after* the marker
    // unreachable by name too
    agree_python(
        "posonlyshift",
        "\
class Probe:
    def __init__(self, a, /, b=0):
        self.total = a + b

    def m(self, a, /, b):
        return a + b


def free(a, /, b):
    return a + b
",
        &[
            "m.Probe(1, 2).total",
            "m.Probe(1, b=2).total",
            // with no `**kwargs` to take it, a keyword spelling `a` is refused —
            // by the constructor slot as much as by the method wrapper
            "_capture_kw(m.Probe, (), {'a': 1, 'b': 2})",
            "m.Probe(1).m(3, b=4)",
            "m.Probe(1).m(3, 4)",
            "_capture_kw(m.Probe(1).m, (), {'a': 3, 'b': 4})",
            "m.free(3, b=4)",
            "_capture_kw(m.free, (), {'a': 3, 'b': 4})",
        ],
    );
}

#[test]
fn a_call_a_boundary_binds_nothing_from_is_rejected_in_pythons_wording() {
    // a boundary with no named parameters used to phrase its own refusal rather than
    // going through the binding, and one with only a `*args` did not refuse at all —
    // a keyword nothing could take was silently dropped
    agree_python(
        "emptybind",
        "\
def nothing():
    return 7


def only_var(*args):
    return len(args)
",
        &[
            "m.nothing()",
            "m.only_var(1, 2)",
            "_capture(m.nothing, 1)",
            "_capture_kw(m.nothing, (), {'x': 1})",
            "_capture_kw(m.only_var, (), {'x': 1})",
        ],
    );
}

#[test]
fn the_arithmetic_dunders_reach_both_directions() {
    // python hands `nb_add` its operands in the order they were written whichever
    // type it asked, so the adapter works out which side is ours before it knows
    // whether this is `__add__` or `__radd__`. one direction the class does not
    // define answers `NotImplemented`
    agree_python(
        "arith",
        "\
class Vec:
    def __init__(self, x: int, y: int) -> None:
        self.x = x
        self.y = y

    def __add__(self, other: object) -> object:
        if isinstance(other, Vec):
            return Vec(self.x + other.x, self.y + other.y)
        return NotImplemented

    def __mul__(self, other: object) -> object:
        if isinstance(other, int):
            return Vec(self.x * other, self.y * other)
        return NotImplemented

    def __rmul__(self, other: object) -> object:
        return self.__mul__(other)

    def __truediv__(self, other: object) -> object:
        if isinstance(other, int):
            return Vec(self.x // other, self.y // other)
        return NotImplemented

    def __repr__(self) -> str:
        return 'Vec(' + str(self.x) + ', ' + str(self.y) + ')'
",
        &[
            "m.Vec(1, 2) + m.Vec(3, 4)",
            "m.Vec(1, 2) * 3",
            // the reflected direction, which only `__rmul__` answers
            "3 * m.Vec(1, 2)",
            "m.Vec(6, 8) / 2",
            "repr(m.Vec(1, 2).__add__('x'))",
            "type(_capture(lambda a, b: a + b, m.Vec(1, 2), 1)).__name__",
            "type(_capture(lambda a, b: a * b, m.Vec(1, 2), 'x')).__name__",
            // no `__sub__` and no `__rsub__` at all
            "type(_capture(lambda a, b: a - b, m.Vec(1, 2), m.Vec(1, 2))).__name__",
            "type(_capture(lambda a, b: a / b, 2, m.Vec(1, 2))).__name__",
            "sum([m.Vec(1, 1), m.Vec(2, 2)], m.Vec(0, 0))",
        ],
    );
}

#[test]
fn the_power_dunder_carries_the_modulus_through_its_slot() {
    // `nb_power` is the one *ternary* numeric slot: `pow(a, b, m)` passes the modulus
    // through it, and python spells `a ** b` as a `None` modulus. so the adapter has
    // to tell the two apart — a three-argument power reaches only the left operand's
    // `__pow__`, never `__rpow__`, and a class whose `__pow__` takes two parameters
    // raises there rather than silently dropping the modulus
    agree_python(
        "power",
        "\
class Mod:
    def __init__(self, n: int) -> None:
        self.n = n

    def __pow__(self, other: object, modulus: object = None) -> object:
        if not isinstance(other, int):
            return NotImplemented
        if modulus is None:
            return Mod(self.n ** other)
        return Mod(pow(self.n, other, modulus))

    def __rpow__(self, other: object) -> object:
        if isinstance(other, int):
            return Mod(other ** self.n)
        return NotImplemented

    def __repr__(self) -> str:
        return 'Mod(' + str(self.n) + ')'


class Binary:
    def __init__(self, n: int) -> None:
        self.n = n

    def __pow__(self, other: object) -> object:
        return self.n

    def __repr__(self) -> str:
        return 'Binary(' + str(self.n) + ')'
",
        &[
            "m.Mod(2) ** 5",
            "pow(m.Mod(2), 5)",
            // the modulus, which only the ternary slot carries
            "pow(m.Mod(2), 10, 7)",
            "pow(m.Mod(3), 4, 5)",
            // the reflected direction, which the binary form reaches and the
            // three-argument one never does
            "2 ** m.Mod(5)",
            "type(_capture(lambda a, b: a ** b, m.Mod(2), 'x')).__name__",
            "type(_capture(pow, 2, m.Mod(3), 5)).__name__",
            // a two-parameter `__pow__` has nowhere to put the modulus
            "m.Binary(4) ** 2",
            "type(_capture(pow, m.Binary(4), 2, 3)).__name__",
        ],
    );
}

#[test]
fn the_power_dunder_is_answered_by_the_compiled_type() {
    // `wrapper_descriptor` says the slot itself is filled: `PyType_Ready` builds one
    // only for a slot that is, and it shadows the method table entry
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_powslot");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Mod:
    def __init__(self, n: int) -> None:
        self.n = n

    def __pow__(self, other: object, modulus: object = None) -> object:
        if modulus is None:
            return self.n ** other
        return pow(self.n, other, modulus)
";
    let built = match build_source(
        source,
        "by_diff_powslot",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_powslot as m\n\
         print(m.Mod(2) ** 5, pow(m.Mod(2), 10, 7))\n\
         print(type(m.Mod.__dict__['__pow__']).__name__)\n",
    );
    assert_eq!(out, "32 2\nwrapper_descriptor");
}

#[test]
fn the_containment_operator_agrees() {
    // `in` is the container's own protocol rather than a comparison: `__contains__`
    // where the type has one, and a scan of the iterator otherwise. so it reads its
    // operands in the opposite order to every other comparison — `value in container`
    agree_python(
        "inop",
        "\
class Once:
    def __init__(self, only: int) -> None:
        self.only = only

    def __contains__(self, value: object) -> bool:
        return value == self.only


def has(xs: list[int], v: int) -> bool:
    return v in xs


def lacks(xs: list[int], v: int) -> bool:
    return v not in xs


def in_text(s: str, part: str) -> bool:
    return part in s


def in_map(d: dict[str, int], k: str) -> bool:
    return k in d


def in_custom(c: Once, v: int) -> bool:
    return v in c


def counted(xs: list[int], wanted: list[int]) -> int:
    n = 0
    for w in wanted:
        if w in xs:
            n = n + 1
    return n
",
        &[
            "[m.has([1, 2, 3], v) for v in [1, 3, 9]]",
            "m.has([], 1)",
            "[m.lacks([1, 2, 3], v) for v in [2, 9]]",
            "[m.in_text('hello', p) for p in ['ell', 'z', '', 'hello']]",
            "[m.in_map({'a': 1}, k) for k in ['a', 'b']]",
            "[m.in_custom(m.Once(4), v) for v in [4, 5]]",
            "m.counted([1, 2, 3], [1, 9, 3])",
            "m.counted([], [1])",
            // an unhashable probe into a dict is a TypeError, not a False
            "type(_capture(m.in_map, {'a': 1}, [])).__name__",
            // the probe goes straight to `PyDict_Contains` for an *exact* dict, so a
            // subclass that overrides `__contains__` is what says the guard holds
            "[m.in_map(type('D', (dict,), {'__contains__': lambda s, k: True})(), k) \
             for k in ['a', 'b']]",
        ],
    );
}

#[test]
fn the_container_dunders_fill_their_slots() {
    // `len(g)`, `g[i]`, `g[i] = v`, `v in g` and iteration all read *slots* — the
    // method table is never consulted for any of them. `__getitem__` and
    // `__setitem__` share the mapping sub-table with `__len__`, and `__contains__`
    // needs a sequence table of its own
    agree_python(
        "containers",
        "\
class Grid:
    def __init__(self, items: list[int]) -> None:
        self.items = items

    def __len__(self) -> int:
        return len(self.items)

    def __getitem__(self, key: int) -> int:
        return self.items[key]

    def __setitem__(self, key: int, value: int) -> None:
        self.items[key] = value

    def __contains__(self, value: object) -> bool:
        for v in self.items:
            if v == value:
                return True
        return False

    def __iter__(self) -> object:
        return iter(self.items)


def mutated(items: list[int], at: int, to: int) -> list[int]:
    g = Grid(items)
    g[at] = to
    return list(g)
",
        &[
            "len(m.Grid([1, 2, 3]))",
            "len(m.Grid([]))",
            "m.Grid([1, 2, 3])[0]",
            "m.Grid([1, 2, 3])[-1]",
            "str(_capture(lambda g: g[9], m.Grid([1, 2, 3])))",
            "2 in m.Grid([1, 2, 3])",
            "9 in m.Grid([1, 2, 3])",
            "9 not in m.Grid([1, 2, 3])",
            "list(m.Grid([1, 2, 3]))",
            "[x * 2 for x in m.Grid([1, 2, 3])]",
            "sum(m.Grid([1, 2, 3]))",
            "max(m.Grid([3, 1, 2]))",
            "sorted(m.Grid([3, 1, 2]))",
            "m.mutated([1, 2, 3], 1, 7)",
            "m.mutated([1, 2, 3], -1, 7)",
            // filling the assignment slot is what gives the type a `__delitem__` at
            // all, and python's own slot answers a missing one with `AttributeError`
            // naming the method
            "type(_capture(lambda g: g.__delitem__(0), m.Grid([1]))).__name__",
            "(lambda e: (type(e).__name__, str(e)))(_capture(_delete_first, m.Grid([1])))",
        ],
    );
}

#[test]
fn the_assignment_slot_carries_both_of_its_methods() {
    // `mp_ass_subscript` is one slot for `__setitem__` and `__delitem__` — a NULL
    // value is the delete — so a class with only one of them still has to fill it,
    // and the half it does not have raises what python raises
    agree_python(
        "asssub",
        "\
class Bag:
    def __init__(self, items: list[int]) -> None:
        self.items = items

    def __delitem__(self, key: int) -> None:
        del self.items[key]


class Log:
    def __init__(self) -> None:
        self.seen = []

    def __setitem__(self, key: object, value: object) -> None:
        self.seen.append((key, value))

    def __delitem__(self, key: object) -> None:
        self.seen.append(('del', key))


def assigned(target: object, key: object, value: object) -> None:
    target[key] = value


def deleted(target: object, key: object) -> None:
    del target[key]


def dropped(items: list[int], at: int) -> list[int]:
    b = Bag(items)
    del b[at]
    return b.items
",
        &[
            "m.dropped([1, 2, 3], 1)",
            "m.dropped([1, 2, 3], -1)",
            "(lambda e: (type(e).__name__, str(e)))(_capture(m.dropped, [1, 2, 3], 9))",
            // only `__delitem__`, so assigning finds the missing half
            "(lambda e: (type(e).__name__, str(e)))(_capture(m.assigned, m.Bag([1]), 0, 1))",
            // both halves, dispatched on whether the value is there
            "(lambda g: (m.assigned(g, 'k', 1), m.deleted(g, 'k'), g.seen))(m.Log())",
        ],
    );
}

#[test]
fn the_numeric_conversion_dunders_fill_their_slots() {
    // `int()`, `float()` and every use of `__index__` read `tp_as_number`, never
    // the method table
    agree_python(
        "convert",
        "\
class Cell:
    def __init__(self, n: int) -> None:
        self.n = n

    def __int__(self) -> int:
        return self.n

    def __float__(self) -> float:
        return float(self.n) + 0.5

    def __index__(self) -> int:
        return self.n


class Loose:
    def __index__(self) -> object:
        return 1.5
",
        &[
            "int(m.Cell(7))",
            "float(m.Cell(7))",
            "[hex(m.Cell(255)), bin(m.Cell(5)), oct(m.Cell(8))]",
            "[10, 20, 30][m.Cell(1)]",
            "list(range(m.Cell(3)))",
            "'abc'[m.Cell(2)]",
            // the slot has to hand back an int, and it is python that says so —
            // reaching that message is how we know the slot was the thing called
            "(lambda e: (type(e).__name__, str(e)))(_capture(hex, m.Loose()))",
        ],
    );
}

#[test]
fn the_comparison_dunders_share_one_slot() {
    // python does not look these up by name: it calls `tp_richcompare` with an
    // opcode. so all six are one function, one the class does not define answers
    // `NotImplemented` — which is what lets `a > b` reach the *other* operand's
    // `__lt__` — and defining `__eq__` without `__hash__` makes the class
    // unhashable, which a type built from a spec has to do for itself
    agree_python(
        "richcmp",
        "\
class Money:
    def __init__(self, cents: int) -> None:
        self.cents = cents

    def __eq__(self, other: object) -> bool:
        if isinstance(other, Money):
            return self.cents == other.cents
        return NotImplemented

    def __lt__(self, other: object) -> bool:
        if isinstance(other, Money):
            return self.cents < other.cents
        return NotImplemented

    def __repr__(self) -> str:
        return 'Money(' + str(self.cents) + ')'


class Hashed:
    def __init__(self, n: int) -> None:
        self.n = n

    def __eq__(self, other: object) -> bool:
        return isinstance(other, Hashed) and self.n == other.n

    def __hash__(self) -> int:
        return self.n * 7
",
        &[
            "m.Money(5) == m.Money(5)",
            "m.Money(5) == m.Money(7)",
            "m.Money(5) != m.Money(7)",
            "m.Money(5) < m.Money(7)",
            "m.Money(7) < m.Money(5)",
            // the reflected direction: there is no `__gt__`, so python swaps and
            // uses the other operand's `__lt__`
            "m.Money(7) > m.Money(5)",
            "m.Money(5) == 'x'",
            "m.Money(5) != 'x'",
            "'x' == m.Money(5)",
            "sorted([m.Money(3), m.Money(1), m.Money(2)])",
            "min([m.Money(3), m.Money(1)])",
            "m.Money(5) in [m.Money(1), m.Money(5)]",
            // `__eq__` and no `__hash__` is unhashable
            "type(_capture(hash, m.Money(1))).__name__",
            "hash(m.Hashed(3))",
            "m.Hashed(3) == m.Hashed(3)",
            "m.Hashed(3) == m.Hashed(4)",
            "len({m.Hashed(1), m.Hashed(1), m.Hashed(2)})",
            "m.Hashed(1) in {m.Hashed(1)}",
            "m.Money(1).__eq__(m.Money(1))",
        ],
    );
}

#[test]
fn a_complex_conversion_is_found_by_name() {
    // `PyNumberMethods` has `nb_int`, `nb_float` and `nb_index` and no complex field
    // at all, so `complex(x)` looks the name up on the type — which is exactly what
    // the method table answers. it was declined by a rule that listed `__complex__`
    // among the slots on the strength of its siblings rather than of CPython's
    agree_python(
        "complexname",
        "\
class Cell:
    def __init__(self, n: int) -> None:
        self.n = n

    def __complex__(self) -> object:
        return complex(self.n, 1)


class Loose:
    def __complex__(self) -> object:
        return 'not a complex'
",
        &[
            "complex(m.Cell(3))",
            "complex(m.Cell(0))",
            "abs(complex(m.Cell(3)))",
            "m.Cell(3).__complex__()",
            // python checks what came back, and reaching its message is how we know
            // python's own conversion ran rather than the method being called directly
            "(lambda e: (type(e).__name__, str(e)))(_capture(complex, m.Loose()))",
        ],
    );
}

#[test]
fn a_complex_conversion_is_answered_by_the_compiled_type() {
    // `method_descriptor` is what says the compiled type answered: a class that fell
    // back to its interpreted definition answers `complex(x)` identically
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_complexslot");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Cell:
    def __init__(self, n: int) -> None:
        self.n = n

    def __complex__(self) -> object:
        return complex(self.n, 1)
";
    let built = match build_source(
        source,
        "by_diff_complexslot",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_complexslot as m\n\
         print(complex(m.Cell(3)))\n\
         print(type(m.Cell.__dict__['__complex__']).__name__)\n",
    );
    assert_eq!(out, "(3+1j)\nmethod_descriptor");
}

#[test]
fn an_await_method_fills_the_async_slot() {
    // `await x` reads `am_await` out of the async sub-table and never consults the
    // name, so without the slot the class is simply not awaitable — a `TypeError`
    // where the interpreted twin hands back a value
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_awaitslot");
    let _ = std::fs::remove_dir_all(&dir);
    // the awaited iterator is a class of its own rather than a generator, so what
    // this exercises is `am_await` alone
    let source = "\
class Done:
    def __init__(self, value: int) -> None:
        self.value = value

    def __iter__(self) -> object:
        return self

    def __next__(self) -> object:
        raise StopIteration(self.value)


class Answer:
    def __init__(self, n: int) -> None:
        self.n = n

    def __await__(self) -> object:
        return Done(self.n * 2)
";
    let built = match build_source(
        source,
        "by_diff_awaitslot",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    // `wrapper_descriptor` is the stronger signal that `method_descriptor` would be:
    // `PyType_Ready` builds a slot wrapper only for a slot that is *filled*, and it
    // shadows the method table entry. the interpreted twin answers `function`, so
    // this still says which build ran
    let out = run(
        &python,
        &dir,
        "import asyncio\n\
         import by_diff_awaitslot as m\n\
         async def go(x):\n\
         \x20   return await x\n\
         print(asyncio.run(go(m.Answer(3))))\n\
         print(type(m.Answer.__dict__['__await__']).__name__)\n",
    );
    assert_eq!(out, "6\nwrapper_descriptor");
}

#[test]
fn a_del_method_fills_the_finalizer_slot() {
    // `tp_finalize` is not reached by name and not reached by itself: `tp_dealloc`
    // has to call it, and a class that writes its own dealloc — which every compiled
    // class does — gets no cleanup at all unless it does.
    //
    // what has to hold besides "it ran": exactly once, with the fields still live,
    // with a raise reported rather than swallowed or propagated, and with a subclass
    // that inherits both the layout and the finalizer finalized too
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_delslot");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Closer:
    def __init__(self, log: list[str], tag: str) -> None:
        self.log = log
        self.tag = tag

    def __del__(self) -> None:
        self.log.append('closed:' + self.tag)


class Sub(Closer):
    def extra(self) -> str:
        return 'sub'


class Angry:
    def __init__(self, tag: str) -> None:
        self.tag = tag

    def __del__(self) -> None:
        raise ValueError('from ' + self.tag)
";
    let built = match build_source(
        source,
        "by_diff_delslot",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import sys\n\
         import by_diff_delslot as m\n\
         log = []\n\
         c = m.Closer(log, 'one')\n\
         del c\n\
         # a subclass declares a layout of its own and so writes its own dealloc,\n\
         # which has to reach the finalizer it inherited\n\
         s = m.Sub(log, 'two')\n\
         del s\n\
         # a python subclass is deallocated by `subtype_dealloc`, which calls the\n\
         # finalizer and *then* chains — so this is where a second call would show\n\
         class Py(m.Closer):\n\
         \x20   pass\n\
         p = Py(log, 'three')\n\
         del p\n\
         print(log)\n\
         # a raise out of a finalizer is unraisable: reported, and then dropped\n\
         seen = []\n\
         sys.unraisablehook = lambda hook: seen.append(str(hook.exc_value))\n\
         a = m.Angry('four')\n\
         del a\n\
         print(seen)\n\
         print(type(m.Closer.__dict__['__del__']).__name__)\n",
    );
    assert_eq!(
        out,
        "['closed:one', 'closed:two', 'closed:three']\n\
         ['from four']\n\
         wrapper_descriptor"
    );
}

#[test]
fn a_getattr_hook_stands_behind_the_ordinary_lookup() {
    // `__getattr__` fills `tp_getattro`, which *replaces* attribute lookup — so the
    // adapter has to run the ordinary one first and reach the method only where that
    // raised `AttributeError`, or a field and a method would both stop resolving.
    //
    // what else has to hold: another exception out of the lookup is the answer rather
    // than a reason to fall through, and a subclass inherits the hook
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_getattrhook");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Proxy:
    def __init__(self, tag: str) -> None:
        self.tag = tag

    def named(self) -> str:
        return 'named:' + self.tag

    def __getattr__(self, name: str) -> object:
        if name == 'boom':
            raise KeyError(name)
        return 'made:' + name
";
    let built = match build_source(
        source,
        "by_diff_getattrhook",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_getattrhook as m\n\
         p = m.Proxy('a')\n\
         # the field and the method still resolve the ordinary way\n\
         print(p.tag, p.named())\n\
         # and only what the ordinary lookup could not find reaches the hook\n\
         print(p.absent)\n\
         print(getattr(p, 'other', 'default'))\n\
         # a raise out of the hook that is not `AttributeError` is the answer\n\
         try:\n\
         \x20   p.boom\n\
         except KeyError as e:\n\
         \x20   print('KeyError', e)\n\
         print(type(m.Proxy.__dict__['__getattr__']).__name__)\n",
    );
    assert_eq!(
        out,
        "a named:a\n\
         made:absent\n\
         made:other\n\
         KeyError 'boom'\n\
         method_descriptor"
    );
}

#[test]
fn a_descriptor_get_fills_its_slot() {
    // an attribute lookup that finds a descriptor reads `tp_descr_get`, so a class
    // whose `__get__` only reaches the method table is not a descriptor at all: the
    // object itself comes back where the interpreted twin computes a value.
    //
    // the slot passes NULL for the instance when the attribute was read off the
    // class, which python's own wrapper turns into `None` — so does this one
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_descrget");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Doubler:
    def __init__(self, base: int) -> None:
        self.base = base

    def __get__(self, obj: object, owner: object) -> object:
        if obj is None:
            return ('class', self.base, owner.__name__)
        return ('instance', self.base * 2)
";
    let built = match build_source(
        source,
        "by_diff_descrget",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_descrget as m\n\
         class Holder:\n\
         \x20   twice = m.Doubler(21)\n\
         print(Holder.twice)\n\
         print(Holder().twice)\n\
         print(m.Doubler(1).__get__(None, Holder))\n\
         print(type(m.Doubler.__dict__['__get__']).__name__)\n",
    );
    assert_eq!(
        out,
        "('class', 21, 'Holder')\n\
         ('instance', 42)\n\
         ('class', 1, 'Holder')\n\
         wrapper_descriptor"
    );
}

#[test]
fn append_takes_the_list_fast_path_only_when_it_is_one() {
    // the fast path skips the attribute lookup, so anything that overrides or
    // merely *has* an `append` must still reach its own — a subclass, a type of
    // its own, and a receiver with no `append` at all
    agree_python(
        "append",
        "\
def fill(n: int) -> list[int]:
    out = []
    i = 0
    while i < n:
        out.append(i)
        i = i + 1
    return out

def onto(target: object, value: object) -> object:
    target.append(value)
    return target
",
        &[
            "m.fill(0)",
            "m.fill(4)",
            "m.onto([], 1)",
            "m.onto([1], 2)",
            // a subclass with its own `append` is not the exact list the fast path
            // is for, and has to reach the override
            "m.onto(_Loud(), 1)",
            "m.onto(_Loud(), 1).seen",
            "m.onto(_Counted(), 'x')",
            "type(_capture(m.onto, (1, 2), 3)).__name__",
            "type(_capture(m.onto, 1, 2)).__name__",
            "str(_capture(m.onto, [], 1, 2))",
            "type(_capture(m.onto, bytearray(), 1)).__name__",
        ],
    );
}

#[test]
fn constructing_a_class_this_module_emits_agrees() {
    // the direct path allocates and calls the class's own `__init__` rather than
    // resolving the name through the module namespace. everything the interpreted
    // path did still has to happen: a raising constructor, a constructor that
    // builds another instance, and the shapes the direct path declines to take —
    // a keyword, a default, the wrong arity — which fall back to it
    agree_python(
        "construct",
        "\
class Inner:
    def __init__(self, n: int) -> None:
        if n < 0:
            raise ValueError('negative: ' + str(n))
        self.n = n

    def doubled(self) -> int:
        return self.n * 2


class Outer:
    def __init__(self, a: int, b: int = 7) -> None:
        self.inner = Inner(a)
        self.b = b

    def total(self) -> int:
        return self.inner.doubled() + self.b


def build(n: int) -> int:
    out = 0
    i = 0
    while i < n:
        out = out + Outer(i).total()
        i = i + 1
    return out
",
        &[
            "m.Outer(1).total()",
            "m.Outer(1, 2).total()",
            "m.Outer(a=1, b=2).total()",
            "m.Outer(b=2, a=1).total()",
            "m.Inner(3).doubled()",
            "m.Inner(3).n",
            "m.build(5)",
            "m.build(0)",
            "str(_capture(m.Inner, -1))",
            "str(_capture(m.Outer, -1))",
            "type(_capture(m.Inner)).__name__",
            "type(_capture(m.Inner, 1, 2)).__name__",
            "type(_capture(m.Inner, 'x')).__name__",
            "type(m.Outer(1).inner).__name__",
            "isinstance(m.Outer(1).inner, m.Inner)",
        ],
    );
}

#[test]
fn a_plain_class_agrees() {
    // an ordinary python class: no marker decorator, a hand-written `__init__`,
    // and the layout is whatever that constructor assigns
    agree_python(
        "plainclass",
        "\
class Counter:
    def __init__(self, start: int, step: int = 1) -> None:
        self.value = start
        self.step = step
        self.history = []

    def bump(self) -> int:
        self.value = self.value + self.step
        self.history.append(self.value)
        return self.value

    def total(self) -> int:
        out = 0
        for v in self.history:
            out = out + v
        return out


class Named:
    def __init__(self, name: str) -> None:
        self.name = name

    def shout(self) -> str:
        return self.name + '!'


class Empty:
    def __init__(self) -> None:
        self.n = 0

    def bump(self) -> int:
        self.n = self.n + 1
        return self.n


def run(n: int) -> int:
    c = Counter(10)
    i = 0
    while i < n:
        c.bump()
        i = i + 1
    return c.total()
",
        &[
            "m.run(4)",
            "m.Counter(0).value",
            "m.Counter(0).step",
            "m.Counter(5, 3).bump()",
            "[m.Counter(1, 2).bump() for _ in range(1)]",
            "m.Counter(1).history",
            "m.Named('hi').shout()",
            "m.Named('hi').name",
            "type(m.Counter(1)).__name__",
            "type(_capture(m.Counter)).__name__",
            "type(_capture(m.Counter, 1, 2, 3)).__name__",
            // the constructor's own binding, in cpython's wording
            "str(_capture(m.Counter))",
            "m.Empty().n",
            "m.Empty().bump()",
            "str(_capture(m.Empty, 1))",
        ],
    );
}

#[test]
fn an_integer_index_keeps_its_register() {
    // the index never leaves its register on the fast path, so every case the
    // *boxed* lookup handled has to still land in the same place: a negative
    // index, one past the end, a huge one, and a container the fast path skips
    agree(
        "tagindex",
        "\
def at(xs: list[int], i: int) -> int:
    return xs[i]

def tup(t: tuple[int, str], i: int) -> object:
    return t[i]

def mapping(d: dict[int, str], i: int) -> str:
    return d[i]

def text(s: str, i: int) -> str:
    return s[i]

def put(xs: list[int], i: int, v: int) -> list[int]:
    xs[i] = v
    return xs

def put_map(d: dict[int, str], i: int, v: str) -> dict[int, str]:
    d[i] = v
    return d

def summed(xs: list[int]) -> int:
    out = 0
    i = 0
    while i < len(xs):
        out = out + xs[i]
        i = i + 1
    return out
",
        &[
            "m.at([1, 2, 3], 0)",
            "m.at([1, 2, 3], 2)",
            "m.at([1, 2, 3], -1)",
            "m.at([1, 2, 3], -3)",
            "str(_capture(m.at, [1, 2, 3], 3))",
            "str(_capture(m.at, [1, 2, 3], -4))",
            "str(_capture(m.at, [], 0))",
            "type(_capture(m.at, [1], 10**30)).__name__",
            "m.tup((7, 'a'), 0)",
            "m.tup((7, 'a'), -1)",
            "str(_capture(m.tup, (7, 'a'), 5))",
            "m.mapping({1: 'a', -2: 'b'}, 1)",
            "m.mapping({1: 'a', -2: 'b'}, -2)",
            "str(_capture(m.mapping, {1: 'a'}, 9))",
            "m.text('hello', 1)",
            "m.text('hello', -1)",
            "str(_capture(m.text, 'hi', 9))",
            "m.put([1, 2, 3], 0, 9)",
            "m.put([1, 2, 3], -1, 9)",
            "str(_capture(m.put, [1, 2], 5, 9))",
            "str(_capture(m.put, [], 0, 9))",
            "m.put_map({}, 3, 'x')",
            "m.put_map({1: 'a'}, -2, 'b')",
            "m.summed([1, 2, 3, 4])",
            "m.summed([])",
        ],
    );
}

#[test]
fn a_double_meeting_an_object_tests_it_rather_than_boxing() {
    // the checker knows `float.__add__` returns a float whatever it was handed, so
    // the double stays in its register and the *object* is tested. an exact float
    // takes the inline path; everything else has to go through the object protocol
    // exactly as before, or a type with its own `__radd__` would get the wrong answer
    agree_python(
        "floatobj",
        "\
def total(xs: list[float]) -> float:
    out = 0.0
    for x in xs:
        out = out + x
    return out

def diff(a: float, xs: list[float]) -> float:
    out = a
    for x in xs:
        out = out - x
    return out

def scaled(xs: list[float]) -> float:
    out = 1.0
    for x in xs:
        out = out * x
    return out

def ratio(a: float, xs: list[float]) -> float:
    out = a
    for x in xs:
        out = out / x
    return out

def biggest(xs: list[float], start: float) -> float:
    out = start
    for x in xs:
        if x > out:
            out = x
    return out

def below(xs: list[float], limit: float) -> int:
    n = 0
    for x in xs:
        if x < limit:
            n = n + 1
    return n

def reflected(xs: list[float], a: float) -> float:
    out = 0.0
    for x in xs:
        out = out + x * a - x / a
    return out
",
        &[
            "m.total([1.5, 2.5])",
            // an `int` element is legal where a `float` is annotated
            "m.total([1, 2, 3])",
            "m.total([1, 2.5, True])",
            "m.total([])",
            "m.diff(10.0, [1, 2.5])",
            "m.scaled([2, 2.5])",
            "m.ratio(10.0, [2, 2.5])",
            "type(_capture(m.ratio, 1.0, [0])).__name__",
            "type(_capture(m.ratio, 1.0, [0.0])).__name__",
            // a type of its own still reaches `__radd__`, which the inline path
            // would have skipped
            "m.total([_Reflected()])",
            "m.total([1.5, _Reflected()])",
            "type(_capture(m.total, ['x'])).__name__",
            "m.total([10**30])",
            // the object on the *left*: the checker still says `int | float`, and
            // the proof that it is a float is the other side's representation
            "m.reflected([1.5, 2.5], 2.0)",
            "m.reflected([1, 2], 2.0)",
            "m.reflected([], 2.0)",
            "m.reflected([1, 2.5], 4)",
            "type(_capture(m.reflected, [1.0], 0.0)).__name__",
            // a comparison is not a conversion: python compares an int against a
            // float exactly, so a huge one answers rather than overflowing
            "m.biggest([1.5, 3, 2.5], 0.0)",
            "m.biggest([], 9.0)",
            "m.biggest([10**400], 1.0)",
            "m.biggest([-(10**400)], 1.0)",
            "m.below([1, 2.5, 3], 2.5)",
            "m.below([10**400, -(10**400)], 1.0)",
            "m.below([True, False], 0.5)",
        ],
    );
}

#[test]
fn a_mixed_numeric_pair_agrees() {
    // python's numeric tower converts the `int` side and operates on doubles, and
    // so does the lowering — including the `OverflowError` an integer with no
    // float at all raises, and the `ZeroDivisionError` a zero divisor does
    agree_python(
        "mixnum",
        "\
def add(a: float, b: int) -> float:
    return a + b

def radd(a: int, b: float) -> float:
    return a + b

def mul(a: float, b: int) -> float:
    return a * b

def div(a: float, b: int) -> float:
    return a / b

def idiv(a: int, b: float) -> float:
    return a // b

def rem(a: float, b: int) -> float:
    return a % b

def power(a: float, b: int) -> float:
    return a ** b
",
        &[
            "m.add(1.5, 2)",
            "m.add(-1.5, 2)",
            "m.radd(2, 1.5)",
            "m.mul(0.1, 3)",
            "m.mul(-2.5, 0)",
            "m.div(1.0, 3)",
            "m.idiv(7, 2.0)",
            "m.idiv(-7, 2.0)",
            "m.rem(-7.5, 2)",
            "m.rem(7.5, -2)",
            "m.power(2.0, 10)",
            "m.power(2.0, -1)",
            "type(_capture(m.div, 1.0, 0)).__name__",
            "type(_capture(m.add, 1.5, 10**400)).__name__",
            // the conversion rounds exactly the way `float(x)` does
            "m.add(0.0, 2**53 + 1)",
            "m.mul(1.0, -(2**62))",
        ],
    );
}

#[test]
fn a_float_parameter_defers_to_the_interpreted_definition() {
    // python's `float` annotation admits an `int`, so the compiled body's `double`
    // is not a promise the caller has to keep. an argument that is not exactly a
    // float is handed to the interpreted definition rather than rejected or
    // converted — either of which would be a different program
    agree_python(
        "deferfloat",
        "\
def scale(x: float, by: float = 2.0) -> float:
    return x * by

def kind(x: float) -> str:
    return type(x).__name__

def exact(x: float) -> bool:
    return x == 1
",
        &[
            "m.scale(1.5)",
            "m.scale(1.5, 4.0)",
            "m.scale(3)",
            "m.scale(3, 4)",
            "m.scale(x=1.5, by=3.0)",
            "m.scale(x=3)",
            "m.scale(True)",
            "m.scale(10**30)",
            "m.kind(1.5)",
            // an `int` keeps being an `int` all the way through, which is exactly
            // what converting it at the boundary would have destroyed
            "m.kind(3)",
            "m.kind(True)",
            "m.kind(_tiny(2.0))",
            "m.exact(1.0)",
            "m.exact(1)",
            "m.scale(_tiny(2.0))",
        ],
    );
}

#[test]
fn a_plain_python_module_compiles_and_agrees() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    // ordinary python, no basedpython syntax at all. the interpreted leg is the
    // *same file* run by cpython — there is nothing to transpile, which is the
    // whole point: a `.py` source is already its own fallback
    let source = "\
def collatz(n: int) -> int:
    steps = 0
    while n != 1:
        if n % 2 == 0:
            n = n // 2
        else:
            n = 3 * n + 1
        steps += 1
    return steps

def total(xs: list[float]) -> float:
    out = 0.0
    for x in xs:
        out = out + x
    return out

def sliced(xs: list[int]) -> str:
    return str(xs[1:3])
";
    let dir = std::env::temp_dir().join("by_diff_plainpy");
    let interpreted = std::env::temp_dir().join("by_diff_plainpy_i");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&interpreted);
    std::fs::create_dir_all(&interpreted).expect("the directory is created");
    std::fs::write(interpreted.join("by_diff_plainpy.py"), source).expect("written");

    let options = Options {
        language: by_irbuild::Language::Python,
        ..Options::default()
    };
    let built = match build_source(source, "by_diff_plainpy", &toolchain, &dir, &options) {
        Ok(built) => built,
        Err(error) => {
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "{:?}", built.declined);

    for call in [
        "m.collatz(27)",
        "m.total([1.5, 2.5, 3.5])",
        "m.sliced([1, 2, 3, 4])",
        "[m.collatz(i) for i in range(1, 20)]",
    ] {
        let body = format!("import by_diff_plainpy as m\nprint(repr({call}))\n");
        assert_eq!(
            run(&python, &dir, &body),
            run(&python, &interpreted, &body),
            "{call}"
        );
    }
}

#[test]
fn an_unboxed_list_agrees() {
    // a `list` of values that own nothing lives in a buffer of its own — no
    // `PyObject *` per element, no protocol per index
    agree_with_declines(
        "unboxedlist",
        "\
def total(n: int) -> float:
    xs = [1.5, 2.5, 3.5, 4.5]
    out = 0.0
    i = 0
    while i < len(xs):
        out = out + xs[i]
        i = i + 1
    return out

def indexed(n: int) -> float:
    xs = [1.5, 2.5]
    return xs[0] + xs[-1]

def written(n: int) -> float:
    xs = [1.5, 2.5]
    xs[0] = 9.5
    return xs[0] + xs[1]

def out_of_range(n: int) -> float:
    xs = [1.0]
    return xs[5]

def escapes(n: int) -> object:
    xs = [1.0, 2.0]
    return xs

def sized(n: int) -> int:
    xs = [1.0, 2.0, 3.0]
    return len(xs)

def boolean(n: int) -> int:
    flags = [True, False, True]
    total = 0
    i = 0
    while i < len(flags):
        if flags[i]:
            total = total + 1
        i = i + 1
    return total
",
        &[
            "m.total(0)",
            "m.indexed(0)",
            "m.written(0)",
            "m.sized(0)",
            "m.boolean(0)",
            // the index is bounds-checked the way a list index is, message included
            "[(type(e).__name__, str(e)) for e in [_capture(m.out_of_range, 0)]]",
            // a list that escapes is a real list, and stays one
            "m.escapes(0)",
            "type(m.escapes(0)).__name__",
        ],
    );
}

#[test]
fn a_hoisted_immutable_read_still_agrees() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    // reading an immutable field of a parameter once at entry rather than every
    // iteration is invisible from outside — including when the loop runs zero times,
    // which is the case the hoist has to get right
    agree(
        "hoistread",
        "\
frozen data class Config:
    factor: int
    offset: int

    def apply(self, xs: list[int]) -> int:
        total = 0
        for x in xs:
            total = total + x * self.factor + self.offset
        return total

data class Loose:
    factor: int

    def apply(self, xs: list[int]) -> int:
        total = 0
        for x in xs:
            total = total + x * self.factor
        return total
",
        &[
            "m.Config(3, 7).apply([1, 2, 3])",
            // the loop never runs, and the read still must not be observable
            "m.Config(3, 7).apply([])",
            "m.Loose(3).apply([1, 2, 3])",
            "m.Config(10 ** 20, 1).apply([2])",
        ],
    );
}

#[test]
fn a_frozen_field_is_read_once_across_a_call() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    // the optimization is invisible from outside, which is the point — this pins
    // that it stays invisible
    agree(
        "frozenread",
        "\
frozen data class Vec2:
    x: float
    y: float

data class Loose:
    x: float

def frozen_reads(v: Vec2, note: object) -> float:
    a = v.x + v.y
    print(note)
    return a + v.x + v.y

def loose_reads(v: Loose, note: object) -> float:
    a = v.x
    print(note)
    return a + v.x
",
        &[
            "round(m.frozen_reads(m.Vec2(1.0, 2.0), 'x'), 1)",
            "round(m.loose_reads(m.Loose(1.0), 'x'), 1)",
        ],
    );
}

#[test]
fn a_final_receiver_still_agrees() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    // the direct call a `@final` place licenses must give the same answers as the
    // protocol would — including where the place is *not* final and an override wins
    agree(
        "finalplace",
        "\
from typing import final

data class Open:
    n: int

    def doubled(self) -> int:
        return self.n * 2

data class Derived(Open):
    extra: int

    def doubled(self) -> int:
        return self.n * 20

@final
data class Fixed(Open):
    label: str

    def tripled(self) -> int:
        return self.n * 3

def on_final(f: Fixed) -> int:
    return f.tripled()

def on_final_inherited(f: Fixed) -> int:
    return f.doubled()

def on_open(o: Open) -> int:
    return o.doubled()
",
        &[
            "m.on_final(m.Fixed(2, 'x'))",
            // an inherited method, called directly through the base's symbol
            "m.on_final_inherited(m.Fixed(2, 'x'))",
            "m.on_open(m.Open(2))",
            // the override is still seen where the place is not final
            "m.on_open(m.Derived(2, 0))",
            "m.on_open(type('Sub', (m.Open,), {'doubled': lambda self: 999})(2))",
        ],
    );
}

#[test]
fn inheritance_agrees() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    agree(
        "inheritance",
        "\
data class Shape:
    name: str

    def describe(self) -> str:
        return 'shape ' + self.name

data class Circle(Shape):
    radius: float

    def describe(self) -> str:
        return 'circle ' + self.name

    def area(self) -> float:
        return 3.14159 * self.radius * self.radius

data class Marked(Circle):
    label: str

data class Defaulted:
    x: int = 1

data class AlsoDefaulted(Defaulted):
    # an inherited field keeps its *default* too, so the subclass's constructor asks
    # for it no more than the base's did
    y: int = 2

def through_base(s: Shape) -> str:
    return s.describe()

def direct(c: Circle) -> str:
    return c.describe()

def upcast(c: Circle) -> str:
    return through_base(c)

def deep(n: float) -> str:
    m = Marked('unit', n, 'm')
    return m.name + m.label + str(round(m.area(), 2)) + m.describe()
",
        &[
            // a field of the base is read at its own offset in the subclass
            "m.Circle('c', 2.0).name",
            "m.Circle('c', 2.0).radius",
            "m.deep(1.0)",
            // an override is seen through the base, which is why an inherited class
            // gives up the direct call
            "m.through_base(m.Shape('s'))",
            "m.through_base(m.Circle('c', 1.0))",
            "m.direct(m.Circle('c', 1.0))",
            "m.upcast(m.Circle('c', 1.0))",
            // a defaulted field is inherited defaulted, so every arity the base
            // admitted the subclass admits too
            "(m.Defaulted().x, m.Defaulted(9).x)",
            "(m.AlsoDefaulted().x, m.AlsoDefaulted().y)",
            "(m.AlsoDefaulted(5).x, m.AlsoDefaulted(5).y)",
            "(m.AlsoDefaulted(5, 6).x, m.AlsoDefaulted(5, 6).y)",
            "(m.AlsoDefaulted(y=7).x, m.AlsoDefaulted(y=7).y)",
            // an inherited method resolves through the type's own mro
            "m.Marked('u', 1.0, 'm').describe()",
            "isinstance(m.Circle('c', 1.0), m.Shape)",
            "[c.__name__ for c in type(m.Marked('u', 1.0, 'm')).__mro__[:3]]",
            // and a *python* subclass can override a compiled method
            "m.through_base(type('Sub', (m.Circle,), {'describe': lambda self: 'sub ' + self.name})('x', 1.0))",
            "m.direct(type('Sub', (m.Circle,), {'describe': lambda self: 'sub ' + self.name})('x', 1.0))",
        ],
    );
}

#[test]
fn a_subclass_that_writes_no_init_runs_the_base_one() {
    // a subclass's fields *are* its base's, so reading "has fields" as "has something
    // to initialize" synthesized a constructor taking one argument per inherited
    // field — and the base's `__init__` then never ran at all. every side effect in
    // it was lost, and a private field name leaked into the arity message
    agree_python(
        "inheritedinit",
        "\
LOG = []


class Base:
    def __init__(self, x):
        self.x = x * 2
        self.tally = len(LOG)
        LOG.append(x)


class Quiet(Base):
    def read(self):
        return self.x


class Deeper(Quiet):
    def both(self):
        return (self.x, self.tally)


class Private:
    def __init__(self, _msg, _exception):
        self._msg = _msg
        self._exception = _exception


class Raising(Private):
    def message(self):
        return self._msg
",
        &[
            // the base's arithmetic ran, rather than the argument landing raw
            "m.Quiet(5).x",
            "m.Quiet(5).read()",
            "m.Deeper(5).both()",
            // a field the subclass never mentions is filled all the same
            "m.Quiet(5).tally",
            // the side effect the base's body has is a side effect the subclass has
            "(m.Quiet(1), m.Deeper(2), m.Base(3), m.LOG)[3]",
            // the arity message names the base's `__init__`, not the subclass and not
            // the private field names a synthesized one would have published
            "type(_capture(m.Quiet)).__name__",
            "str(_capture(m.Quiet))",
            "str(_capture(m.Raising))",
            "str(_capture(m.Raising, 1, 2, 3))",
            "m.Raising('a', 'b').message()",
            // and the constructor is still reached through the subclass's own mro
            "[c.__name__ for c in m.Deeper.__mro__[:3]]",
            "m.Deeper.__init__ is m.Base.__init__",
        ],
    );
}

#[test]
fn a_subclass_that_writes_no_init_inherits_the_slot() {
    // `agree` cannot say which build answered — a subclass that fell back to its
    // interpreted definition agrees with itself. `wrapper_descriptor` says a real
    // slot did, and the base's is the only one there is: the subclass fills none
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_inheritedinitslot");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
LOG = []


class Base:
    def __init__(self, x):
        self.x = x * 2
        LOG.append(x)


class Quiet(Base):
    def read(self):
        return self.x
";
    let built = match build_source(
        source,
        "by_diff_inheritedinitslot",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_inheritedinitslot as m\n\
         print(m.Quiet(5).x, m.LOG)\n\
         print(type(m.Quiet.__init__).__name__, type(m.Base.__init__).__name__)\n",
    );
    assert_eq!(out, "10 [5]\nwrapper_descriptor wrapper_descriptor");
    // the subclass publishes no constructor of its own, so nothing of its own can
    // stand between a construction and the base's
    let generated =
        std::fs::read_to_string(built.artifact.source).expect("the generated c is there");
    let (_, tail) = generated
        .split_once("By_by_diff_inheritedinitslot_Quiet_Type_slots[]")
        .expect("the subclass has a slot table");
    let (slots, _) = tail.split_once("};").expect("the slot table ends");
    assert!(!slots.contains("Py_tp_init"), "the subclass filled tp_init");
}

/// the classes the `__slots__` cases below are written against
///
/// one shape per question: a plain declaration, the bare-string form, the empty one, a
/// declaration inherited and added to, one a constructor half fills, and one whose names
/// are mangled against the body that wrote them
const SLOTTED: &str = "\
class Link:
    __slots__ = 'prev', 'next'


class One:
    __slots__ = \"x\"


class Empty:
    __slots__ = ()


class Base:
    __slots__ = ('a',)


class Sub(Base):
    __slots__ = ('b',)


class Both:
    __slots__ = ('kept', 'spare')

    def __init__(self, kept):
        self.kept = kept


class Private:
    __slots__ = ('__hidden',)

    def hide(self, value):
        self.__hidden = value

    def hidden(self):
        return self.__hidden


def linked(a, b):
    link = Link()
    link.prev = a
    link.next = b
    return (link.prev, link.next)
";

#[test]
fn the_attributes_a_slots_declaration_names_agree() {
    agree_python(
        "slots",
        SLOTTED,
        &[
            // the whole of the bug: a declared attribute is storage the instance has
            "m.linked(1, 'two')",
            "(lambda l: (setattr(l, 'prev', 5), l.prev)[1])(m.Link())",
            "(lambda o: (setattr(o, 'x', 3), o.x)[1])(m.One())",
            // and one never written answers python's own `AttributeError` rather than
            // handing on whatever `tp_alloc` left
            "type(_capture(lambda: m.Link().prev)).__name__",
            "type(_capture(lambda: m.One().x)).__name__",
            // the point of the declaration: a name it left out is still unreachable
            "type(_capture(lambda: setattr(m.Link(), 'nope', 1))).__name__",
            "type(_capture(lambda: setattr(m.Empty(), 'x', 1))).__name__",
            "type(_capture(lambda: setattr(m.Sub(), 'c', 1))).__name__",
            // an empty declaration adds nothing and stays a class with no fields
            "m.Empty.__slots__",
            "m.Link.__slots__",
            "m.One.__slots__",
            // a subclass reaches its base's declaration and its own
            "(lambda s: (setattr(s, 'a', 1), setattr(s, 'b', 2), (s.a, s.b))[2])(m.Sub())",
            "type(_capture(lambda: m.Sub().a)).__name__",
            // a declaration takes no constructor argument, whichever type shape the
            // class gets: `Link` is static, `Sub` is a heap type standing on a base.
            // only `Link` publishes an `__init__` of its own, so only its message is
            // one this module writes — `Sub` reaches `object.__init__`, which names the
            // instance's type by the dotted `tp_name` a type spec is given
            "str(_capture(m.Link, 1))",
            "type(_capture(m.Sub, 1)).__name__",
            // a name the constructor assigns keeps the constructor's answer, and the
            // one beside it is absent until something writes it
            "m.Both(4).kept",
            "type(_capture(lambda: m.Both(4).spare)).__name__",
            "(lambda b: (setattr(b, 'spare', 9), b.spare)[1])(m.Both(1))",
            // a declared name is mangled against the body that wrote it
            "(lambda p: (p.hide(7), p.hidden())[1])(m.Private())",
            "type(_capture(lambda: m.Private().hidden())).__name__",
            "sorted(n for n in dir(m.Private) if 'hidden' in n)",
        ],
    );
}

#[test]
fn a_slots_declaration_is_storage_the_emitted_type_owns() {
    // `agree` cannot say which build answered — a class that fell back to its interpreted
    // definition agrees with itself, and this one would have the very descriptors the
    // test is about. the *kind* of descriptor is what differs: python makes a
    // `member_descriptor` from `__slots__`, and a field of the emitted type is reached
    // through the getter it publishes
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_diff_slotsowned");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        SLOTTED,
        "by_diff_slotsowned",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            assert!(missing_toolchain(&error), "failed to build: {error:#}");
            eprintln!("skipping: no working C toolchain ({error})");
            return;
        }
    };
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_slotsowned as m\n\
         print(type(m.Link.__dict__['prev']).__name__)\n\
         print(type(m.Sub.__dict__['b']).__name__)\n\
         print(type(m.Private.__dict__['_Private__hidden']).__name__)\n",
    );
    assert_eq!(
        out,
        "getset_descriptor\ngetset_descriptor\ngetset_descriptor"
    );
    // and the storage is in the instance struct rather than anywhere else
    let generated =
        std::fs::read_to_string(built.artifact.source).expect("the generated c is there");
    let (_, tail) = generated
        .split_once("typedef struct By_by_diff_slotsowned_Link {")
        .expect("`Link` has an instance struct");
    let (fields, _) = tail.split_once('}').expect("the struct ends");
    assert!(
        fields.contains("by_f_prev") && fields.contains("by_p_prev"),
        "`Link` kept no room for `prev`: {fields}"
    );
}

#[test]
fn a_decorated_class_agrees() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    // a decorator may *mutate* the class, which a static type cannot allow — so a
    // decorated class is a heap type, and pays for it with the direct method call
    agree(
        "classdecorator",
        "\
def tagged(cls: type) -> type:
    cls.tag = 'seen'
    return cls

def counted(cls: type) -> type:
    cls.made = 0
    return cls

@tagged
data class Point:
    x: int
    y: int

    def total(self) -> int:
        return self.x + self.y

@tagged
@counted
data class Pair:
    a: str
    b: str

data class Plain:
    n: int

    def doubled(self) -> int:
        return self.n * 2

def use(n: int) -> str:
    p = Point(n, n + 1)
    return str(p.total()) + str(Point.tag)

def pairs(s: str) -> str:
    q = Pair(s, s)
    return q.a + q.b + str(Pair.tag) + str(Pair.made)
",
        &[
            "m.use(1)",
            "m.pairs('z')",
            "m.Point(1, 2).total()",
            "m.Point(1, 2).x",
            "m.Point.tag",
            // both decorators ran, innermost first
            "(m.Pair.tag, m.Pair.made)",
            // an undecorated class is untouched and keeps its direct method call
            "m.Plain(3).doubled()",
            // a decorated one is mutable, which is the whole reason it is a heap
            // type — an undecorated one stays static, and rejecting `setattr` there
            // is a difference from the interpreted class that predates this
            "[type(e).__name__ for e in [_capture(setattr, m.Point, 'extra', 1)]]",
            "m.Point.extra if hasattr(m.Point, 'extra') else None",
        ],
    );
}

#[test]
fn field_defaults_and_keyword_construction_agree() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    agree(
        "fielddefaults",
        "\
data class Point:
    x: int
    y: int = 10

data class Named:
    label: str = 'anon'
    weight: float = 1.5

def make(n: int) -> str:
    return str(Point(n).x) + ',' + str(Point(n).y)

def keyworded(n: int) -> str:
    p = Point(y=n, x=1)
    return str(p.x) + ',' + str(p.y)

def defaults(n: int) -> str:
    a = Named()
    b = Named('bob')
    c = Named(weight=2.5)
    return a.label + str(a.weight) + b.label + str(c.weight)
",
        &[
            "m.make(3)",
            "m.keyworded(7)",
            "m.defaults(0)",
            // python takes the fields positionally or by keyword, and so does this
            "m.Point(1, 2).y",
            "m.Point(1).y",
            "m.Point(x=1, y=2).x",
            "m.Named().label",
            // every binding error, in python's own wording
            "[(type(e).__name__, str(e)) for e in [_capture(m.Point)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.Point, 1, 2, 3)]]",
            "[(type(e).__name__, str(e)) for e in [_capture_kw(m.Point, (1,), {'z': 2})]]",
            "[(type(e).__name__, str(e)) for e in [_capture_kw(m.Point, (1,), {'x': 2})]]",
        ],
    );
}

#[test]
fn a_class_with_no_fields_agrees() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    agree(
        "nofields",
        "\
data class Marker:
    def describe(self) -> str:
        return 'marker'

data class Tag:
    pass

def use(n: int) -> str:
    return Marker().describe() + type(Tag()).__name__
",
        &[
            "m.use(0)",
            "m.Marker().describe()",
            "[(type(e).__name__, str(e)) for e in [_capture(m.Marker, 1)]]",
        ],
    );
}

#[test]
fn an_augmented_assignment_to_an_attribute_agrees() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    agree(
        "augtarget",
        "\
data class Counter:
    n: int

    def bump(self, by: int) -> int:
        self.n += by
        return self.n

def attribute(c: Counter) -> int:
    c.n += 5
    return c.n
",
        &["m.Counter(1).bump(2)", "m.attribute(m.Counter(1))"],
    );
}

#[test]
fn an_augmented_assignment_to_an_item_agrees() {
    agree(
        "augitem",
        "\
def items(xs: list[int]) -> str:
    xs[0] += 10
    xs[-1] *= 2
    return str(xs)

def once(xs: list[int], seen: list[int]) -> str:
    xs[_index(seen)] += 1
    return str(xs) + str(seen)

def _index(seen: list[int]) -> int:
    seen.append(1)
    return 0

def strings(d: dict[str, str]) -> str:
    d['a'] += '!'
    return str(sorted(d.items()))
",
        &[
            "m.items([1, 2, 3])",
            // the target's parts are evaluated once, so the index call happens once
            "m.once([1, 2], [])",
            "m.strings({'a': 'x'})",
            "[(type(e).__name__, str(e)) for e in [_capture(m.items, [])]]",
        ],
    );
}

#[test]
fn more_than_one_with_item_agrees() {
    agree_with_declines(
        "withitems",
        "\
class Recorder:
    def __init__(self, name: str, seen: list[str]) -> None:
        self.name = name
        self.seen = seen

    def __enter__(self) -> str:
        self.seen.append('enter ' + self.name)
        return self.name

    def __exit__(self, a: object, b: object, c: object) -> bool:
        self.seen.append('exit ' + self.name)
        return False

def two(seen: list[str]) -> str:
    out = ''
    with Recorder('a', seen) as x, Recorder('b', seen) as y:
        out = x + y
    return out + '|' + ','.join(seen)

def three(seen: list[str]) -> str:
    out = ''
    with Recorder('a', seen) as x, Recorder('b', seen) as y, Recorder('c', seen) as z:
        out = x + y + z
    return out + '|' + ','.join(seen)

def raising(seen: list[str]) -> str:
    try:
        with Recorder('a', seen) as x, Recorder('b', seen) as y:
            raise ValueError('boom')
    except ValueError:
        pass
    return ','.join(seen)
",
        &[
            // the managers exit innermost first, which is the whole point of the
            // nesting
            "m.two([])",
            "m.three([])",
            "m.raising([])",
        ],
    );
}

#[test]
fn keywords_python_has_to_bind_agree() {
    agree(
        "runtimekeywords",
        "\
def joined(s: str, xs: list[str]) -> str:
    return s.join(xs)

def sorted_down(xs: list[int]) -> str:
    return str(sorted(xs, reverse=True))

def rounded(x: float) -> str:
    return str(round(x, ndigits=2))

def through(f: object, x: str) -> object:
    return f(x, base=16)
",
        &[
            "m.joined('-', ['a', 'b'])",
            "m.sorted_down([3, 1, 2])",
            "m.rounded(1.239)",
            "m.through(int, 'ff')",
            // a keyword the callee has no parameter for is python's error to raise,
            // because python is the one binding
            "[(type(e).__name__, str(e)) for e in [_capture(m.through, int, 'zz')]]",
        ],
    );
}

#[test]
fn the_remaining_statement_forms_agree() {
    agree(
        "statementrest",
        "\
def declared(n: int) -> int:
    total: int
    total = n + 1
    return total

def computed_message(n: int) -> int:
    assert n > 0, f'bad {n}'
    return n

def bare_message(n: int) -> int:
    assert n > 0
    return n

def computed_spec(x: float, width: int) -> str:
    return f'{x:{width}.2f}'

def nested_spec(x: float, width: int, places: int) -> str:
    return f'{x:{width}.{places}f}'
",
        &[
            "m.declared(1)",
            "m.computed_message(1)",
            // a bare `assert` carries no message at all, which is not the same as
            // an empty one
            "[(type(e).__name__, repr(str(e)), e.args) for e in [_capture(m.bare_message, -1)]]",
            "[(type(e).__name__, repr(str(e)), e.args) for e in [_capture(m.computed_message, -1)]]",
            "m.computed_spec(1.239, 8)",
            "m.nested_spec(1.239, 10, 3)",
        ],
    );
}

#[test]
fn starred_displays_agree() {
    agree(
        "stardisplay",
        "\
def lists(xs: list[int]) -> str:
    return str([0, *xs, 9])

def only(xs: list[int]) -> str:
    return str([*xs])

def twice(xs: list[int], ys: list[int]) -> str:
    return str([*xs, 5, *ys])

def tuples(xs: list[int]) -> str:
    return str((0, *xs, 9))

def sets(xs: list[int]) -> str:
    return str(sorted({0, *xs, 9}))

def dicts(d: dict[str, int]) -> str:
    return str({'a': 0, **d, 'z': 9})

def dict_merge(d: dict[str, int], e: dict[str, int]) -> str:
    return str({**d, 'm': 5, **e})

def bad_star(n: int) -> str:
    return str([*n])

def bad_merge(n: int) -> str:
    return str({**n})
",
        &[
            "m.lists([1, 2])",
            "m.only([1, 2])",
            "m.only([])",
            "m.twice([1], [2])",
            "m.tuples([1, 2])",
            "m.sets([1, 2])",
            "m.dicts({'b': 1})",
            // a later key wins, which is what makes the order matter
            "m.dict_merge({'b': 1, 'm': 0}, {'b': 9})",
            // and the type errors are python's, wording included
            "[(type(e).__name__, str(e)) for e in [_capture(m.bad_star, 3)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.bad_merge, 3)]]",
        ],
    );
}

#[test]
fn splatting_at_a_call_agrees() {
    agree(
        "splatcall",
        "\
def add(a: int, b: int) -> int:
    return a + b

def splat(xs: list[int]) -> int:
    return add(*xs)

def splat_kw(d: dict[str, int]) -> int:
    return add(**d)

def both(xs: list[int], d: dict[str, int]) -> int:
    return add(*xs, **d)

def mixed(xs: list[int]) -> int:
    return add(1, *xs)

def builtin(xs: list[int]) -> int:
    return max(*xs)

def merged(d: dict[str, int], e: dict[str, int]) -> int:
    return add(**d, **e)

def through(f: object, xs: list[int]) -> object:
    return f(*xs)
",
        &[
            "m.splat([1, 2])",
            "m.splat_kw({'a': 1, 'b': 2})",
            "m.both([1], {'b': 2})",
            "m.mixed([2])",
            "m.builtin([3, 9, 4])",
            "m.merged({'a': 1}, {'b': 2})",
            "m.through(max, [1, 5])",
            // the binding happens at runtime, so its errors are python's own
            "[(type(e).__name__, str(e)) for e in [_capture(m.splat, [1])]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.splat, [1, 2, 3])]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.splat_kw, {'a': 1, 'zz': 2})]]",
        ],
    );
}

#[test]
fn keyword_only_and_positional_only_parameters_agree() {
    agree(
        "paramkinds",
        "\
def kwonly(a: int, *, b: int) -> int:
    return a * 10 + b

def kwonly_default(a: int, *, b: int = 5) -> int:
    return a * 10 + b

def posonly(a: int, /, b: int) -> int:
    return a * 10 + b

def both(a: int, /, b: int, *, c: int) -> int:
    return a * 100 + b * 10 + c

def posonly_kwargs(a: int, /, **rest: int) -> str:
    return str(a) + str(sorted(rest.items()))

def kwonly_varargs(a: int, *rest: int, b: int) -> str:
    return str(a) + str(rest) + str(b)

def calls_kwonly(n: int) -> int:
    return kwonly(n, b=2)

def calls_posonly(n: int) -> int:
    return posonly(n, b=2)
",
        &[
            "m.kwonly(1, b=2)",
            "m.kwonly_default(1)",
            "m.kwonly_default(1, b=9)",
            "m.posonly(1, 2)",
            "m.posonly(1, b=2)",
            "m.both(1, 2, c=3)",
            "m.posonly_kwargs(1, a=2, z=3)",
            "m.kwonly_varargs(1, 2, 3, b=4)",
            "m.kwonly_varargs(1, b=4)",
            "m.calls_kwonly(1)",
            "m.calls_posonly(1)",
            // every arity error, in python's own wording
            "[(type(e).__name__, str(e)) for e in [_capture(m.kwonly, 1, 2)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.kwonly, 1)]]",
            "[(type(e).__name__, str(e)) for e in [_capture_kw(m.posonly, (), {'a': 1, 'b': 2})]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.kwonly_varargs, 1)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.posonly, 1, 2, 3)]]",
        ],
    );
}

#[test]
fn a_comprehension_with_more_than_one_for_agrees() {
    agree(
        "nestedcomp",
        "\
def flat(rows: list[list[int]]) -> str:
    return str([x for row in rows for x in row])

def guarded(rows: list[list[int]]) -> str:
    return str([x for row in rows if len(row) > 1 for x in row if x > 1])

def paired(n: int) -> str:
    return str([(a, b) for a in range(3) for b in range(3) if a < b])

def triple(n: int) -> str:
    return str([a + b + c for a in [1, 2] for b in [10] for c in [100, 200]])

def mapped(rows: list[list[int]]) -> str:
    return str({x: len(row) for row in rows for x in row})

def set(rows: list[list[int]]) -> str:
    return str(sorted({x for row in rows for x in row}))

def unpacked(n: int) -> str:
    return str([a * b for a, b in [(1, 2), (3, 4)] for _ in range(2)])
",
        &[
            "m.flat([[1, 2], [3], [4, 5, 6]])",
            "m.guarded([[1, 2], [3], [4, 5, 6]])",
            "m.paired(0)",
            "m.triple(0)",
            "m.mapped([[1, 2], [3]])",
            "m.set([[1, 2], [2, 3]])",
            "m.unpacked(0)",
            "m.flat([])",
        ],
    );
}

#[test]
fn the_debug_f_string_form_agrees() {
    agree(
        "debugfstring",
        "\
def plain(x: int) -> str:
    return f'{x=}'

def spaced(x: int) -> str:
    return f'{x = }'

def strings(s: str) -> str:
    return f'{s=}'

def converted(s: str) -> str:
    return f'{s=!s}'

def specced(x: float) -> str:
    return f'{x=:.2f}'

def expression(x: int) -> str:
    return f'{x * 2=}'

def several(x: int, s: str) -> str:
    return f'before {x=} middle {s=} after'

def checked(d: dict[str, int]) -> str:
    return f'{d.get(chr(107))=}'
",
        &[
            "m.plain(5)",
            "m.spaced(5)",
            // the default is the *repr*, which is what makes it a debugging form
            "m.strings('hi')",
            "m.converted('hi')",
            "m.specced(1.239)",
            "m.expression(3)",
            "m.several(1, 'a')",
            // a checked expression inside one renders its *own* source, not the
            // check the transpiler wrapped it in
            "m.checked({'k': 1})",
        ],
    );
}

#[test]
fn the_assignment_surface_agrees() {
    agree(
        "unpacking",
        "\
def swap(a: int, b: int) -> str:
    a, b = b, a
    return str(a) + ',' + str(b)

def chained(n: int) -> str:
    a = b = n + 1
    return str(a) + ',' + str(b)

def from_list(xs: list[int]) -> int:
    a, b, c = xs
    return a + b + c

def starred(xs: list[int]) -> str:
    first, *rest = xs
    return str(first) + '|' + str(rest)

def middle_star(xs: list[int]) -> str:
    head, *body, tail = xs
    return str(head) + '|' + str(body) + '|' + str(tail)

def nested(xs: list[int]) -> int:
    a, (b, c) = xs[0], (xs[1], xs[2])
    return a * 100 + b * 10 + c

def as_a_list(xs: list[int]) -> int:
    [a, b] = xs
    return a - b

def from_a_string(s: str) -> str:
    a, b = s
    return b + a

def counted(n: int) -> int:
    a, b = n, n + 1
    return a * b
",
        &[
            "m.swap(1, 2)",
            "m.chained(4)",
            "m.from_list([1, 2, 3])",
            "m.starred([1, 2, 3])",
            // an empty tail still makes a list
            "m.starred([9])",
            "m.middle_star([1, 2, 3, 4])",
            "m.middle_star([1, 2])",
            "m.nested([7, 8, 9])",
            "m.as_a_list([9, 4])",
            "m.from_a_string('ab')",
            "m.counted(10 ** 20)",
            // the arity errors are python's, wording included
            "[(type(e).__name__, str(e)) for e in [_capture(m.from_list, [1, 2])]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.from_list, [1, 2, 3, 4])]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.middle_star, [1])]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.from_a_string, 'abc')]]",
        ],
    );
}

#[test]
fn a_target_list_that_is_not_a_sequence_agrees() {
    // unpacking drives the *iterator*, so an infinite one has to raise rather than
    // run out of memory — and a non-iterable is a `TypeError` with python's wording
    agree(
        "unpackiter",
        "\
def counter(limit: int) -> object:
    n = 0
    while n < limit:
        yield n
        n = n + 1

def two(xs: object) -> int:
    a, b = xs
    return a * 10 + b

def from_generator(limit: int) -> int:
    a, b = counter(limit)
    return a * 10 + b
",
        &[
            "m.two([1, 2])",
            "m.two((3, 4))",
            "m.from_generator(2)",
            "[(type(e).__name__, str(e)) for e in [_capture(m.from_generator, 1)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.from_generator, 5)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.two, 7)]]",
        ],
    );
}

#[test]
fn a_loop_over_a_target_list_agrees() {
    agree(
        "loopunpack",
        "\
def pairs(n: int) -> int:
    total = 0
    for a, b in [(1, 2), (3, 4)]:
        total = total + a * b
    return total

def comprehended(n: int) -> int:
    return sum([a * b for a, b in [(1, 2), (3, 4)]])

def items(d: dict[str, int]) -> str:
    out = ''
    for k, v in d.items():
        out = out + k + str(v)
    return out

def starred_rows(n: int) -> str:
    out = ''
    for head, *tail in [[1, 2, 3], [4, 5]]:
        out = out + str(head) + str(tail)
    return out

def nested_rows(n: int) -> int:
    total = 0
    for a, (b, c) in [(1, (2, 3)), (4, (5, 6))]:
        total = total + a * b * c
    return total

def indexed(xs: list[str]) -> str:
    out = ''
    for i, x in enumerate(xs):
        out = out + str(i) + x
    return out

def ragged(n: int) -> int:
    total = 0
    for a, b in [(1, 2), (3,)]:
        total = total + a
    return total
",
        &[
            "m.pairs(0)",
            "m.comprehended(0)",
            "m.items({'a': 1, 'b': 2})",
            "m.starred_rows(0)",
            "m.nested_rows(0)",
            "m.indexed(['x', 'y'])",
            "[(type(e).__name__, str(e)) for e in [_capture(m.ragged, 0)]]",
        ],
    );
}

#[test]
fn a_call_to_a_function_that_never_returns_agrees() {
    // its C function has no value to hand back, so its *representation* comes from
    // the annotation — a caller reading the error sentinel back as a value is a
    // silent wrong answer rather than a crash
    agree(
        "neverreturns",
        "\
def fail(reason: str) -> int:
    raise ValueError(reason)

def guarded(n: int) -> int:
    if n < 0:
        return fail('negative')
    return n * 2

def through(n: int) -> str:
    return str(guarded(n))
",
        &[
            "m.guarded(3)",
            "m.through(4)",
            "[(type(e).__name__, str(e)) for e in [_capture(m.guarded, -1)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.through, -1)]]",
        ],
    );
}

#[test]
fn the_exception_surface_agrees() {
    agree_with_declines(
        "raising",
        "\
class Boom(Exception):
    pass

def user(n: int) -> int:
    if n < 0:
        raise Boom('negative')
    return n

def computed(n: int) -> int:
    raise ValueError(f'bad {n}')

def rethrow(n: int) -> int:
    try:
        return user(n)
    except Boom:
        raise

def wrapped(n: int) -> int:
    try:
        return user(n)
    except Boom as e:
        raise RuntimeError('wrapped') from e

def cleanly(n: int) -> int:
    try:
        return user(n)
    except Boom:
        raise RuntimeError('clean') from None

def implicit(n: int) -> int:
    try:
        return user(n)
    except Boom:
        raise RuntimeError('implicit')

def indirect(n: int) -> int:
    try:
        return user(n)
    except Boom:
        return computed(n)

def after(n: int) -> int:
    try:
        user(n)
    except Boom:
        pass
    raise ValueError('fresh')

def tupled(n: int) -> str:
    try:
        if n == 0:
            raise ValueError('zero')
        raise Boom('other')
    except (ValueError, KeyError):
        return 'value'
    except Boom:
        return 'boom'

def held(n: int) -> str:
    e = Boom('prebuilt')
    try:
        raise e
    except Boom as caught:
        return str(caught)

def not_an_exception(n: int) -> int:
    raise n

def returned_from_handler() -> str:
    log = ''
    try:
        raise ValueError('x')
    except ValueError:
        return log + 'handler'
    finally:
        log = log + 'finally'

def raised_from_handler() -> str:
    marks = []
    try:
        try:
            raise ValueError('x')
        except ValueError:
            raise KeyError('y')
        finally:
            marks.append('inner')
    except KeyError:
        return ','.join(marks) + ',outer'

def broken_out(n: int) -> str:
    marks = []
    for i in range(3):
        try:
            raise ValueError('x')
        except ValueError:
            break
        finally:
            marks.append(str(i))
    return ','.join(marks)
",
        &[
            "m.user(1)",
            "_chain(_capture(m.user, -1))",
            "_chain(_capture(m.computed, 3))",
            "m.rethrow(2)",
            // a bare `raise` re-raises the same instance, chain and all
            "_chain(_capture(m.rethrow, -1))",
            "_chain(_capture(m.wrapped, -1))",
            // `from None` suppresses the context; `from e` sets the cause
            "_chain(_capture(m.cleanly, -1))",
            // no `from` at all still chains, which is what a traceback shows
            "_chain(_capture(m.implicit, -1))",
            // a raise inside a *called* function chains onto the handler too
            "_chain(_capture(m.indirect, -1))",
            // and once the block is left, it no longer does
            "_chain(_capture(m.after, -1))",
            "m.tupled(0)",
            "m.tupled(1)",
            "m.held(0)",
            // raising a non-exception is a `TypeError`, with python's wording
            "_chain(_capture(m.not_an_exception, 3))",
            // the `finally` runs on every way out of a handler
            "m.returned_from_handler()",
            "m.raised_from_handler()",
            "m.broken_out(0)",
        ],
    );
}

#[test]
fn an_unset_cell_names_the_error_the_way_python_does() {
    // a frame that *owns* the name sees a local; one that closes over it sees a free
    // variable, and python uses a different class and different wording for each
    agree_with_declines(
        "cellnaming",
        "\
def owner() -> str:
    def later() -> int:
        return n
    keep = later
    try:
        return str(n)
    except NameError as e:
        return type(e).__name__ + ': ' + str(e)

def closes_over() -> object:
    def read() -> str:
        try:
            return str(n)
        except NameError as e:
            return type(e).__name__ + ': ' + str(e)
    out = read
    n = 1
    return out
",
        &["m.owner()", "m.closes_over()()"],
    );
}

#[test]
fn a_closure_made_in_a_loop_binds_that_iteration() {
    // basedpython gives each iteration its own binding, so these do *not* all
    // observe the final value the way python's would
    agree(
        "loopbinding",
        "\
def defs(xs: list[int]) -> list[object]:
    out = []
    for i in xs:
        def show() -> int:
            return i
        out.append(show)
    return out

def lambdas(xs: list[int]) -> list[object]:
    out = []
    for i in xs:
        out.append(lambda: i)
    return out

def comprehended(xs: list[int]) -> list[object]:
    return [lambda: i for i in xs]

def unpacked(rows: list[int]) -> list[object]:
    out = []
    for a, b in [(1, 2), (3, 4)]:
        out.append(lambda: a * 10 + b)
    return out

def strings(xs: list[str]) -> list[object]:
    out = []
    for s in xs:
        out.append(lambda: s + '!')
    return out

def parameters(xs: list[int], step: int) -> list[object]:
    out = []
    for i in xs:
        out.append(lambda: i * step)
    return out
",
        &[
            "[f() for f in m.defs([1, 2, 3])]",
            "[f() for f in m.lambdas([1, 2, 3])]",
            "[f() for f in m.comprehended([1, 2, 3])]",
            "[f() for f in m.unpacked([])]",
            "[f() for f in m.strings(['a', 'b'])]",
            // a capture that is *not* a loop binding comes along unchanged
            "[f() for f in m.parameters([1, 2], 10)]",
            "[f() for f in m.defs([])]",
        ],
    );
}

#[test]
fn a_loop_binding_beside_a_shared_cell_agrees() {
    // one environment object cannot be both a fresh binding per iteration and a
    // cell that outlives them — so it is two, and the closure reaches the cell
    // through the same `$outer` chain a function nested two deep walks
    agree(
        "loopbindcell",
        "\
def alongside(xs: list[int]) -> list[object]:
    total = 100
    out = []
    for i in xs:
        out.append(lambda: i + total)
    total = 200
    return out

def defs(xs: list[int]) -> list[object]:
    n = 0
    out = []
    for i in xs:
        def show() -> int:
            return i * 10 + n
        out.append(show)
        n = n + 1
    return out

def strings(xs: list[str]) -> list[object]:
    tail = '?'
    out = []
    for s in xs:
        out.append(lambda: s + tail)
    tail = '!'
    return out

def nonlocal_write(xs: list[int]) -> str:
    seen = 0
    out = []
    for i in xs:
        def bump() -> int:
            nonlocal seen
            seen = seen + i
            return seen
        out.append(bump)
    for f in out:
        f()
    return str(seen)
",
        &[
            // the binding is this iteration's; the cell is the one the frame shares
            "[f() for f in m.alongside([1, 2])]",
            "[f() for f in m.defs([1, 2, 3])]",
            "[f() for f in m.strings(['a', 'b'])]",
            "m.nonlocal_write([1, 2, 3])",
            "[f() for f in m.alongside([])]",
        ],
    );
}

#[test]
fn closures_nested_more_than_one_deep_agree() {
    agree(
        "deepclosures",
        "\
def three(a: int) -> object:
    def middle(b: int) -> object:
        def inner(c: int) -> int:
            return a + b + c
        return inner
    return middle

def four(a: int) -> object:
    def l2(b: int) -> object:
        def l3(c: int) -> object:
            def l4(d: int) -> int:
                return a + b + c + d
            return l4
        return l3
    return l2

def lambdas(a: int) -> object:
    def middle(b: int) -> ((int) -> int):
        return lambda c: a + b + c
    return middle

def strings(prefix: str) -> object:
    def middle(mid: str) -> ((str) -> str):
        def inner(suffix: str) -> str:
            return prefix + mid + suffix
        return inner
    return middle

def mutated(start: int) -> object:
    n = start
    def middle() -> object:
        def inner() -> int:
            return n
        return inner
    n = n + 1
    return middle

def written_two_up(start: int) -> object:
    n = start
    def middle() -> object:
        def bump() -> int:
            nonlocal n
            n = n + 10
            return n
        return bump
    return middle

def counter() -> object:
    n = 0
    def middle() -> object:
        def step() -> int:
            nonlocal n
            n = n + 1
            return n
        return step
    return middle
",
        &[
            "m.three(1)(2)(3)",
            "m.four(1)(2)(3)(4)",
            "m.lambdas(1)(2)(3)",
            "m.strings('a')('b')('c')",
            // the write after the `def` is visible two frames down, so the cell up
            // there has to be *the* cell rather than a copy
            "m.mutated(0)()()",
            "m.mutated(10 ** 20)()()",
            // and a write from two frames down lands on the same cell
            "[f(), f()][1] if (f := m.written_two_up(1)()) else None",
            // three closures off one middle frame all step one counter
            "[[g(), g(), g()] for g in [m.counter()()]]",
            "[(lambda h: [h(), m.counter()()()])(m.counter()())]",
        ],
    );
}

#[test]
fn a_closure_inside_a_method_agrees() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 10))) {
        eprintln!("skipping: `data class` needs python 3.10");
        return;
    }
    agree_with_declines(
        "methodclosure",
        "\
data class Scaler:
    k: int

    def make(self) -> ((int) -> int):
        return lambda x: x * self.k

    def helper(self, n: int) -> int:
        def doubled(a: int) -> int:
            return a * 2
        return doubled(n) + self.k

    def counting(self, n: int) -> int:
        total = 0
        def add(v: int) -> int:
            nonlocal total
            total = total + v
            return total
        for i in range(n):
            add(self.k)
        return total
",
        &[
            // the lambda captures `self`, so the environment holds a native instance
            "m.Scaler(3).make()(4)",
            "m.Scaler(3).helper(5)",
            "m.Scaler(2).counting(3)",
            "[m.Scaler(a).make()(a) for a in (0, -3, 10 ** 20)]",
        ],
    );
}

#[test]
fn a_yield_inside_try_agrees() {
    agree_with_declines(
        "yieldtry",
        "\
def unstarted(bad: object) -> object:
    n = len(bad)
    yield n


def guarded(log: list[str], n: int) -> object:
    try:
        i = 0
        while i < n:
            yield i
            i = i + 1
    finally:
        log.append(\"closed\")

def caught(log: list[str]) -> object:
    try:
        yield 1
        yield 2
    except ValueError:
        log.append(\"caught\")
        yield 99

def layered(log: list[str]) -> object:
    try:
        try:
            yield 1
        finally:
            log.append(\"inner\")
    finally:
        log.append(\"outer\")
",
        &[
            // draining it runs the `finally`
            "[(log := [], list(m.guarded(log, 2)), log)[1:]]",
            // and so does `close()`, which is the case that needs the raise
            "[(log := [], (g := m.guarded(log, 9)), next(g), g.close(), log)[4:]]",
            // `throw` reaches the `except`, not the caller
            "[(log := [], (h := m.caught(log)), next(h), h.throw(ValueError()), log)[3:]]",
            // both `finally` blocks, innermost first
            "[(log := [], (k := m.layered(log)), next(k), k.close(), log)[4:]]",
            // and an abandoned generator still runs them, via the collector
            "[(log := [], (d := m.guarded(log, 9)), next(d), None)[3:]]",
        ],
    );
}

#[test]
fn an_unmatched_exception_reaches_the_outer_handler() {
    // another silent wrong answer in a shipped feature: a re-raise from an inner
    // handler jumped to the *function* exit, skipping every enclosing `except`
    agree(
        "nestedraise",
        "\
def layered(log: list[str]) -> str:
    try:
        try:
            raise ValueError(\"boom\")
        except KeyError:
            log.append(\"inner-key\")
        finally:
            log.append(\"inner-finally\")
    except ValueError:
        log.append(\"outer-value\")
    return \"done\"

def three_deep(log: list[str]) -> str:
    try:
        try:
            try:
                raise IndexError(\"deep\")
            except KeyError:
                log.append(\"a\")
        except TypeError:
            log.append(\"b\")
    except IndexError:
        log.append(\"c\")
    return \"done\"

def escapes(log: list[str]) -> str:
    try:
        raise ValueError(\"out\")
    except KeyError:
        log.append(\"no\")
    return \"unreachable\"
",
        &[
            "[(log := [], m.layered(log), log)[1:]]",
            "[(log := [], m.three_deep(log), log)[1:]]",
            // and with nothing enclosing it, it still leaves the function
            "[(log := [], type(_capture(m.escapes, log)).__name__, log)[1:]]",
        ],
    );
}

#[test]
fn an_unset_generator_local_agrees() {
    // a state field is unboxed only where it is provably assigned. each of these would
    // read a *zero* instead of raising if the rule were loose
    agree_with_declines(
        "genunset",
        "\
def reads_first(n: int) -> object:
    yield x
    x = 1

def conditional(n: int) -> object:
    if n > 0:
        y = 1
    yield y

def in_loop(n: int) -> object:
    i = 0
    while i < n:
        z = i
        i = i + 1
    yield z

def augmented(n: int) -> object:
    total += n
    yield total

def looped_target(values: list[int]) -> object:
    for v in values:
        pass
    yield v

def fine(n: int) -> object:
    a = 0
    b = a + n
    yield b

def carried(n: int) -> object:
    total = 0
    i = 0
    while i < n:
        total = total + i
        yield total
        i = i + 1
",
        &[
            "[type(e).__name__ for e in [_capture(list, m.reads_first(1))]]",
            "[type(e).__name__ for e in [_capture(list, m.conditional(0))]]",
            "list(m.conditional(1))",
            "[type(e).__name__ for e in [_capture(list, m.in_loop(0))]]",
            "list(m.in_loop(2))",
            "[type(e).__name__ for e in [_capture(list, m.augmented(1))]]",
            "[type(e).__name__ for e in [_capture(list, m.looped_target([]))]]",
            "list(m.looped_target([7]))",
            "list(m.fine(3))",
            // an unboxed carried local still keeps arbitrary precision
            "list(m.carried(4))",
            "list(m.carried(3))[-1] + 10 ** 20",
            "[list(m.fine(a)) for a in (0, -3, 10 ** 20)]",
        ],
    );
}
