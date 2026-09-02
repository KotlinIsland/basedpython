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
//! the property is over the programs `by check` accepts, which is why every call
//! written here is one it does. a compiled function checks its arguments at the
//! boundary and an interpreted one does not, so a call the checker rejects can be
//! answered by the twin and refused by the extension — and cpython is then
//! supplying the behaviour of a program ty had already said was wrong, which is
//! not an expected value for anything. the opt-in `parameters` soundness gate is
//! what lets the twin enforce the same contract; see
//! docs/basedpython/development/compilation/index.md.
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
///
/// only above `by_build::MINIMUM_PYTHON` — anything at or below the floor is already
/// guaranteed, because a toolchain below it never gets built
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

/// a temp directory of this process's own
///
/// every leg below builds into a fixed path under the system temp directory, named after
/// the test. nextest gives each *test* a process of its own, so one run never collides
/// with itself — but nothing stops two *runs* choosing the same directory, and a 3.13
/// sweep beside a 3.14 one does exactly that. the two then overwrite each other's sources
/// between the build and the read.
///
/// the failures that produces are the convincing kind. a collision during setup fails
/// fast enough to look like a missing toolchain, and one *after* the build fails having
/// genuinely compiled and compared, so it reads as a difference the compiler produced.
/// twenty-nine of those were chased as a regression before the shared path was noticed.
///
/// the module's own name is a separate argument from the directory holding it, so putting
/// the process id here changes where a test builds and nothing about what it builds
fn diff_root() -> PathBuf {
    std::env::temp_dir().join(format!("by_diff_p{}", std::process::id()))
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
    // an interpreter below `by_build::MINIMUM_PYTHON` is refused by the probe, so
    // every test that goes through here skips on one rather than failing: below the
    // floor there is no native leg to compare against, and the failure that used to
    // stand in for that answer was a wall of C compiler errors
    let toolchain = match Toolchain::probe(&python) {
        Ok(toolchain) => toolchain,
        Err(error) => {
            eprintln!("skipping: {error}");
            return None;
        }
    };
    Some((python, toolchain))
}

/// a helper both legs get, so a raising call can be compared by type and message
/// rather than only by the fact that it raised
const CAPTURE_HELPER: &str = "\
import gc
import warnings

# which module a warning was reported *against*, asked without naming a file
#
# `warn` counts frames back from its own caller to pick that module, and records what
# it showed in that module's `__warningregistry__`. so whether the module under test
# grew one is the whole of the question a `stacklevel` decides — and unlike the file
# name in the message, it is spelled the same in both legs. the compiled leg's
# fallback source runs through `PyRun_String` and names its frames `<string>`
def _warned_into(module, fn, *args):
    module.__dict__.pop('__warningregistry__', None)
    with warnings.catch_warnings():
        warnings.simplefilter('default')
        fn(*args)
    registry = module.__dict__.get('__warningregistry__', {})
    return sorted(key[0] for key in registry if isinstance(key, tuple))

# every warning a call emitted, in full
#
# `_warned_into` answers which module was blamed and nothing else, and a lowering that
# fills a warning's context in rather than counting it off frames has to be checked on
# all of that context. only the *base name* of the file, because the two legs are built
# in directories of their own — the compiled leg carries the absolute path of the
# source it was built from, exactly as a `.pyc` carries the path it was compiled from.
# a raise is part of the answer too: the filters turn some warnings into exceptions,
# and `warn` refuses several argument shapes outright
import os as _os
import types as _types

def _warnings_from(fn, *args):
    with warnings.catch_warnings(record=True) as seen:
        warnings.resetwarnings()
        warnings.simplefilter('always')
        raised = None
        try:
            fn(*args)
        except BaseException as e:
            raised = type(e).__name__ + ': ' + str(e)
    return (raised, [(str(w.message), w.category.__name__,
                      _os.path.basename(w.filename), w.lineno) for w in seen])

# how many times the same warning is actually shown under the default filters. the
# module's own `__warningregistry__` is what suppresses the repeats, so this says the
# registry being written is the one python would have written
def _repeated_warning(module, times=4):
    module.__dict__.pop('__warningregistry__', None)
    with warnings.catch_warnings(record=True) as seen:
        warnings.resetwarnings()
        warnings.simplefilter('default')
        for _ in range(times):
            module.once()
    return len(seen)

# as `_warned_into`, for a call that python may refuse outright
#
# `skip_file_prefixes` is both: before 3.12 writing it at all is a `TypeError`, and from
# 3.12 it forces the stack level to at least two. so the answer has to carry the raise
# as well as the module, or one version's half of the question goes unasked
def _warned_into_safely(module, fn, *args):
    module.__dict__.pop('__warningregistry__', None)
    raised = None
    with warnings.catch_warnings():
        warnings.resetwarnings()
        warnings.simplefilter('default')
        try:
            fn(*args)
        except BaseException as e:
            raised = type(e).__name__ + ': ' + str(e)
    registry = module.__dict__.get('__warningregistry__', {})
    return (raised, sorted(key[0] for key in registry if isinstance(key, tuple)))

# what a warning carried as its `source`, which is the object `tracemalloc` hangs an
# allocation traceback off. it reaches the record and nothing else, so it is asked for
# separately — and without the file name, since a declined function's frames are named
# `<string>` in the compiled leg
def _warned_source(fn, *args):
    with warnings.catch_warnings(record=True) as seen:
        warnings.resetwarnings()
        warnings.simplefilter('always')
        fn(*args)
    return [(str(w.message), w.category.__name__, repr(w.source)) for w in seen]

# the module a warning is blamed on, read back through a `module=` filter
#
# nothing on the record carries it, and the filters are the only place it shows — which
# is also the only place it *matters*: a warning blamed on the wrong module is shown
# when python would hide it, or hidden when python would show it. `warn` reads it out
# of the frame's globals, so the name is moved about here to reach the branches python
# takes for one that is missing or is not a string
_no_name = object()

def _blamed_module(module, fn, name=_no_name):
    saved = module.__dict__.get('__name__')
    if name is _no_name:
        del module.__dict__['__name__']
    else:
        module.__dict__['__name__'] = name
    try:
        out = []
        for pattern in (saved, 'nothing', '<string>', 'renamed'):
            module.__dict__.pop('__warningregistry__', None)
            with warnings.catch_warnings(record=True) as seen:
                warnings.resetwarnings()
                warnings.simplefilter('always')
                warnings.filterwarnings('ignore', module='^' + pattern + '$')
                fn()
            out.append((pattern, len(seen)))
        return out
    finally:
        module.__dict__['__name__'] = saved

# the registry carries a version, and changing the filters invalidates it: a warning
# already shown is shown again rather than stayed silent about
def _registry_after_a_filter_change(module):
    module.__dict__.pop('__warningregistry__', None)
    out = []
    with warnings.catch_warnings(record=True) as seen:
        warnings.resetwarnings()
        warnings.simplefilter('default')
        module.once()
        module.once()
        out.append(len(seen))
        warnings.simplefilter('default')
        module.once()
        out.append(len(seen))
    registry = module.__dict__.get('__warningregistry__', {})
    out.append(sorted(str(key) for key in registry))
    return out

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

# the same question `_renext` asks of a generator: a resumption that carries nothing
# has to leave the `yield` evaluating to `None`, not to whatever the last `asend` put
# there. it has to run one step *past* the value being echoed back, because that is
# the first resumption whose `yield` reads the field again
async def _reasend(source, first, steps):
    out = [await source.__anext__(), await source.asend(first)]
    for _ in range(steps):
        try:
            out.append(await source.asend(None))
        except BaseException as e:
            out.append(type(e).__name__)
    return out

# `athrow` and `aclose` against a machine with no suspension point: `steps` of zero
# leaves it never started, and enough of them leaves it finished. the log says whether
# any of the body ran, which is the half a wrong answer here would get wrong quietly
async def _athrown(source, error, steps):
    for _ in range(steps):
        try:
            await source.__anext__()
        except StopAsyncIteration:
            pass
    try:
        return ('returned', await source.athrow(error('boom')))
    except BaseException as e:
        return (type(e).__name__, str(e))

async def _aclosed(source, steps):
    for _ in range(steps):
        try:
            await source.__anext__()
        except StopAsyncIteration:
            pass
    return await source.aclose()

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

# run a machine to its end and hand it back, so what a *second* resumption does to a
# spent one can be asked directly rather than through a driver that hides it
def _spend(coro):
    try:
        coro.send(None)
    except StopIteration:
        pass
    return coro

def _drained(gen):
    for _ in gen:
        pass
    return gen

# await something that is not a coroutine — an `asend` awaitable, say — so a driver
# that insists on one can still reach it
async def _awaits(aw):
    return await aw

async def _asend_spent(agen):
    async for _ in agen:
        pass
    return await agen.asend(None)

# a refused resumption leaves a coroutine nobody awaited, and python warns about one
# that is merely dropped — so it is closed here, which is what the caller of a refused
# send has to do anyway. the warning would otherwise be the only thing the two legs
# disagreed about
def _refused(coro, fn_name, *args):
    try:
        return _escaped(getattr(coro, fn_name), *args)
    finally:
        coro.close()

# drive a coroutine by hand, without a loop under it: one `send` is the whole of what
# an await does to a coroutine that never suspends, and the value it finished with
# arrives on the `StopIteration` the send raises
def _sent_once(coro):
    try:
        coro.send(None)
    except StopIteration as e:
        return e.value
    coro.close()
    return 'suspended'

# closed rather than dropped, so a coroutine that is never awaited does not leave a
# `RuntimeWarning` behind for whichever leg happened to build it
def _is_coroutine(coro):
    out = asyncio.iscoroutine(coro)
    coro.close()
    return out

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

# call `fn` from a frame standing in a builtins namespace of its own: the real one
# with `overrides` applied over it. what a *callee* resolves must not move, because
# python binds a function's builtins to the module that defined it — so this changes
# the caller's namespace and nothing else
def _under_builtins(fn, **overrides):
    frame = {'__builtins__': dict(vars(__import__('builtins')), **overrides), 'fn': fn}
    exec('out = fn()', frame)
    return frame['out']

# import `name` afresh from such a frame and evaluate `call` against it there, with the
# module bound to `m`. what a module body binds is the *interpreter's* builtins, not the
# builtins of whoever asked for the import, so this too must leave resolution where it was
def _reimported_under_builtins(name, call, **overrides):
    __import__('sys').modules.pop(name, None)
    frame = {'__builtins__': dict(vars(__import__('builtins')), **overrides)}
    exec('import ' + name + ' as m\\nout = ' + call, frame)
    return frame['out']

# raising an exception and catching it again, which is the only way an exception class
# is asked for the traceback and the frames a `raise` hangs on the instance
def _raised_and_caught(kind, *args):
    try:
        raise kind(*args)
    except BaseException as e:
        return e

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

# what came out of a frame that raised, described down to the chaining
#
# pep 479 replaces a `StopIteration` leaving a generator with a `RuntimeError` and
# hangs the original off it as both `__cause__` and `__context__`. a conversion that
# built the new exception but dropped the chaining would still have the right class
# and the right message, so the class and the message alone do not pin it
def _escaped(fn, *args):
    try:
        fn(*args)
    except BaseException as e:
        return (
            type(e).__name__,
            str(e),
            type(e.__cause__).__name__ if e.__cause__ is not None else None,
            type(e.__context__).__name__ if e.__context__ is not None else None,
            e.__suppress_context__,
        )
    return None

# resume with no value at all, repeatedly
#
# `next(g)` is `g.send(None)`, so the `yield` it resumes has to evaluate to `None` —
# including when an earlier `send` put something else there. a generator that read
# the field without anything writing it would echo that earlier value back
def _renext(gen, first, steps):
    out = [next(gen), gen.send(first)]
    for _ in range(steps):
        try:
            out.append(next(gen))
        except BaseException as e:
            out.append(type(e).__name__)
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

# the whole life of an attribute a method deletes: what it held, that a delete really
# unbinds it, what a read and a second delete say once it is gone, and that a later
# write brings it back. an emitted class stores its attributes in a fixed layout, so
# every one of those is a different piece of machinery from the interpreted dict
def _deleted(h):
    out = [h.tag, h.kept, hasattr(h, 'tag')]
    h.drop()
    out.append(hasattr(h, 'tag'))
    out.append(repr(_capture(getattr, h, 'tag')))
    out.append(repr(_capture(h.drop)))
    h.put('second')
    out.append((h.tag, h.kept, hasattr(h, 'tag')))
    del h.tag
    out.append(hasattr(h, 'tag'))
    out.append(repr(_capture(delattr, h, 'tag')))
    # a private name is bound and deleted under the name python mangles it to, so a
    # rule that read the written name would leave this field with no way to be absent
    out.append(hasattr(h, '_Held__hidden'))
    h.drop_hidden()
    out.append(hasattr(h, '_Held__hidden'))
    out.append(repr(_capture(h.drop_hidden)))
    return out
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

    let compiled_dir = diff_root().join(format!("by_diff_{tag}_c"));
    let interpreted_dir = diff_root().join(format!("by_diff_{tag}_i"));
    let _ = std::fs::remove_dir_all(&compiled_dir);
    let _ = std::fs::remove_dir_all(&interpreted_dir);

    let module = format!("by_diff_{tag}");

    // the interpreted leg: for basedpython, the transpiler's own output run by
    // cpython, under the same config `by_build` uses — including the *target
    // version*, which has to be this interpreter's or the two legs are not the
    // same program. for python there is nothing to transpile — it already is one
    let interpreted_source = match language {
        by_irbuild::Language::BasedPython => {
            let mut config = Config::default();
            if let Some((major, minor)) = toolchain.version
                && let Ok(parsed) = format!("{major}.{minor}").parse()
            {
                config.min_version = parsed;
            }
            by_transforms::transpile(source, &config).expect("the source transpiles")
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
    let built = match build_source(source, module.as_str(), &toolchain, &compiled_dir, &options) {
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
fn arithmetic_that_leaves_the_short_range_agrees() {
    // a tagged integer holds one bit fewer than a machine word, so an operation whose
    // operands are both short can still produce a result that is not, and the answer
    // then has to come back through cpython's own arithmetic. the runtime keeps that
    // half of each operator out of line, so this is the boundary between two pieces of
    // code rather than two branches of one — including at the very edge, where the
    // result is exactly one past what a short can hold
    agree(
        "shortedge",
        "\
def add(a: int, b: int) -> int:
    return a + b

def sub(a: int, b: int) -> int:
    return a - b

def mul(a: int, b: int) -> int:
    return a * b
",
        &[
            "m.add(2**62 - 1, 1)",
            "m.add(-(2**62), -1)",
            "m.sub(-(2**62), 1)",
            "m.sub(2**62 - 1, -1)",
            "m.mul(2**31, 2**31)",
            "m.mul(-(2**31), 2**31)",
            "[m.add(a, b) for a in (2**62 - 1, -(2**62)) for b in (-1, 0, 1)]",
            "[m.mul(a, b) for a in (2**40, -(2**40)) for b in (2**40, -(2**40), 0)]",
        ],
    );
}

#[test]
fn an_operation_that_keeps_leaving_the_short_range_does_not_leak() {
    // each trip round this loop boxes both operands, calls cpython, and tags the
    // result. a reference dropped or kept anywhere along that path shows up as growth
    // and nowhere else, because the short path never allocates at all
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_bigleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def climb(a: int, n: int) -> int:
    total = 0
    i = 0
    while i < n:
        total = total + a * a
        i = i + 1
    return total
";
    if build_source(
        source,
        "by_diff_bigleak",
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
        "import gc, by_diff_bigleak as m\n\
         big = 2**80\n\
         m.climb(big, 50)\n\
         gc.collect(); before = len(gc.get_objects())\n\
         print(m.climb(big, 2000) == big * big * 2000)\n\
         gc.collect(); after = len(gc.get_objects())\n\
         print('stable' if after <= before + 100 else f'grew {before}->{after}')\n",
    );
    assert_eq!(out, "True\nstable");
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
    let dir = diff_root().join("by_diff_objerr");
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
    let dir = diff_root().join("by_diff_objleak");
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
    let dir = diff_root().join("by_diff_bufleak");
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
    let dir = diff_root().join("by_diff_argtemp");
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

/// the source a widened copy is read from, and the neighbours the borrow must not
/// disturb
///
/// `len(x)` widens its operand into a temporary of its own, because a length is
/// defined on anything at all. that temporary now borrows from the register it was
/// copied from rather than retaining, so what has to hold is that the source is
/// still holding the value at every use — including where the source is rebound
/// every trip, and where the thing being measured changes under the loop
const WIDENED_COPIES: &str = "\
def scan(line: str) -> int:
    best = 0
    run = 0
    i = 0
    while i < len(line):
        if line[i] == \" \":
            if run > best:
                best = run
            run = 0
        else:
            run = run + 1
        i = i + 1
    if run > best:
        best = run
    return best

# the copy's source is the parameter, and the parameter is rebound every trip —
# so the value the last trip measured is dropped while the loop is still running
def shrink(s: str) -> int:
    n = 0
    while len(s) > 0:
        s = s[1:]
        n = n + 1
    return n

# the length really does change under the loop, so it has to be asked afresh
def grow(n: int) -> int:
    out = []
    while len(out) < n:
        out.append(len(out))
    return len(out)

# a local whose value is replaced between one measurement and the next
def swapped(n: int) -> int:
    held = \"a\" * n
    first = len(held)
    held = \"bb\" * n
    return first + len(held)

def measure(o) -> int:
    return len(o)
";

#[test]
fn a_length_taken_of_a_widened_copy_agrees() {
    agree(
        "widened",
        WIDENED_COPIES,
        &[
            "[m.scan(s) for s in ('', ' ', 'a', 'abc', 'a bb ccc d', '   ', 'ab  cd')]",
            "m.scan('word0 word1 word2 ' * 40)",
            "[m.shrink(s) for s in ('', 'a', 'abcdef', 'é🎉z')]",
            "[m.grow(n) for n in (0, 1, 5, 40)]",
            "[m.swapped(n) for n in (0, 1, 7)]",
            "[m.measure(o) for o in ([], [1, 2], 'abc', {1: 2}, (1, 2, 3), range(9))]",
            // a subclass may have said its own thing about both the length and the
            // indexing, and a `str` annotation admits one
            "m.scan(type('S', (str,), {'__len__': lambda self: 3})('a b c d'))",
            "[(type(e).__name__, str(e)) for e in [_capture(m.measure, 5), _capture(m.measure, None)]]",
        ],
    );
}

#[test]
fn a_borrowed_copy_does_not_over_release_its_source() {
    // the copy no longer retains, so an argument whose only reference is the
    // caller's temporary is now being read through a register that owns nothing.
    // getting the window wrong frees it mid-loop, which is fatal rather than merely
    // wrong — and a stray release would show as a falling reference count
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_widened_rc");
    let _ = std::fs::remove_dir_all(&dir);
    if build_source(
        WIDENED_COPIES,
        "by_diff_widened_rc",
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
        "import sys, by_diff_widened_rc as m\n\
         kept = 'a bb ccc ' * 30\n\
         before = sys.getrefcount(kept)\n\
         for _ in range(2000):\n\
         \x20   m.scan(kept)\n\
         \x20   m.scan('x y ' * 20)\n\
         \x20   m.shrink('abcdef')\n\
         \x20   m.measure([1, 2, 3])\n\
         print('stable' if sys.getrefcount(kept) == before else 'moved')\n",
    );
    assert_eq!(out, "stable");
}

/// a copy inside a loop body `unswitch` duplicates, and a copy into a name
///
/// `unswitch` runs before the borrow pass and emits a second copy of every loop body,
/// reusing the same registers, so nothing written inside one is ever written once.
/// `table[key]` widens the key into an operand of its own three times a trip, and all
/// six of those copies used to retain. the same body also fills a *named* local from
/// a copy every trip, which the rule used to refuse on the name alone
const BORROWED_COPIES: &str = "\
def counted(table: dict[str, int], keys: list[str], passes: int) -> int:
    running = 0
    p = 0
    n = len(keys)
    while p < passes:
        i = 0
        while i < n:
            key = keys[i]
            running = running + table[key]
            table[key] = table[key] + 1
            i = i + 1
        p = p + 1
    return running


# `kept` is a name the source program gave, filled from a copy every trip
def held(text: str, n: int) -> int:
    total = 0
    i = 0
    while i < n:
        kept = text
        total = total + len(kept)
        i = i + 1
    return total


# the source is rebound between the copy and the use, so what the copy points at is
# whatever the rebinding released
def rebound(text: str, other: str, n: int) -> int:
    total = 0
    i = 0
    while i < n:
        kept = text
        text = other
        other = kept
        total = total + len(kept)
        i = i + 1
    return total


# one name filled from two different sources needs one answer for both
def either(a: str, b: str, flag: bool) -> int:
    if flag:
        kept = a
    else:
        kept = b
    return len(kept)


# a copy that leaves the frame is a reference handed out, however the loops above are
# compiled
def carried(text: str) -> str:
    kept = text
    return kept
";

#[test]
fn a_copy_in_a_duplicated_loop_body_agrees() {
    agree(
        "borrowedcopies",
        BORROWED_COPIES,
        &[
            "[m.counted({'a': 1, 'b': 2}, ['a', 'b', 'a'], n) for n in (0, 1, 3)]",
            "m.counted({'k': 0}, ['k'], 40)",
            "[m.held(s, 3) for s in ('', 'a', 'abcdef', 'é🎉z')]",
            "[m.rebound('ab', 'cde', n) for n in (0, 1, 2, 7)]",
            "[m.either('ab', 'cde', f) for f in (True, False)]",
            "[m.carried(s) for s in ('', 'abc')]",
            // a subclass may have said its own thing about the length, and a `str`
            // annotation admits one
            "m.held(type('S', (str,), {'__len__': lambda self: 3})('abcd'), 2)",
            "[(type(e).__name__, str(e)) for e in [_capture(m.counted, {}, ['a'], 1)]]",
        ],
    );
}

#[test]
fn a_borrowed_copy_in_a_duplicated_loop_body_does_not_move_its_source_references() {
    // the copies no longer retain, so what holds the key through the subscript is the
    // local it was read into. a stray release shows as a falling count and a missing
    // one as a climbing count — and a release per trip reaches zero long before the
    // loop ends, which is a crash rather than a wrong answer
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_borrowed_copies_rc");
    let _ = std::fs::remove_dir_all(&dir);
    if build_source(
        BORROWED_COPIES,
        "by_diff_borrowed_copies_rc",
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
        "import sys, by_diff_borrowed_copies_rc as m\n\
         kept = 'a bb ccc ' * 30\n\
         other = 'x y ' * 20\n\
         table = {kept: 0, other: 0}\n\
         keys = [kept, other]\n\
         before = (sys.getrefcount(kept), sys.getrefcount(other))\n\
         for _ in range(2000):\n\
         \x20   m.counted(table, keys, 2)\n\
         \x20   m.held(kept, 3)\n\
         \x20   m.rebound(kept, other, 2)\n\
         \x20   m.carried(kept)\n\
         print('stable' if (sys.getrefcount(kept), sys.getrefcount(other)) == before else 'moved')\n",
    );
    assert_eq!(out, "stable");
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
    let dir = diff_root().join("by_diff_shortcircuit");
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

/// a scan's counter never becomes an `int` object, so the comparison reads it as a
/// machine integer — a second index path, with its own range test and its own
/// negative-index arithmetic, and every answer the tagged one gives has to come
/// back out of it unchanged
#[test]
fn comparing_a_character_at_a_counter_agrees() {
    agree(
        "charcmpcounter",
        "\
def spaces(line: str) -> int:
    n = 0
    i = 0
    while i < len(line):
        if line[i] == \" \":
            n = n + 1
        i = i + 1
    return n

# counting down from zero, so every index after the first is negative
def spaces_from_the_end(line: str, length: int) -> int:
    n = 0
    i = 0
    while i > -length:
        if line[i] == \" \":
            n = n + 1
        i = i - 1
    return n

# the counter walks past the last character, so the index that has to raise is a
# machine one too
def up_to(line: str, stop: int) -> int:
    n = 0
    i = 0
    while i < stop:
        if line[i] == \" \":
            n = n + 1
        i = i + 1
    return n

# every operator at a counter, against characters a byte-wise or utf-16 ordering
# would place differently
def order_of_each(line: str) -> object:
    out = []
    i = 0
    while i < len(line):
        out.append((line[i] == \"m\", \"m\" == line[i], line[i] < \"m\", \"m\" < line[i], line[i] <= \"m\", line[i] > \"m\", line[i] >= \"m\"))
        i = i + 1
    return out

def astral_at(line: str) -> object:
    out = []
    i = 0
    while i < len(line):
        out.append(line[i] == \"\\U0001f389\")
        i = i + 1
    return out

# an index no tagged `int` holds without an object of its own, which is past the end
# of every text there is
def at_a_huge_index(line: str) -> bool:
    i = 4611686018427387903
    return line[i] == \" \"

def at_a_huge_negative_index(line: str) -> bool:
    i = 0
    i = i - 4611686018427387903
    return line[i] == \" \"
",
        &[
            // ascii, latin-1, two-byte and four-byte storage, and a text with
            // nothing in it at all
            "[m.spaces(s) for s in ('', ' ', 'a b c', '  ', 'é é', '\\u0100 \\u0100', '\\U0001f389 x', 'abc')]",
            "[m.spaces_from_the_end(s, len(s)) for s in ('', ' ', 'a b c', 'é é', '\\U0001f389 x')]",
            // the first and the last index of each, from both ends
            "[(m.up_to(s, len(s)), m.spaces_from_the_end(s, 1), m.up_to(s, 1)) for s in (' a', 'a ', '\\U0001f389 ', ' \\U0001f389')]",
            // one index past the end, and one index past the start
            "[(type(e).__name__, str(e)) for e in [_capture(m.up_to, 'ab', 3), _capture(m.up_to, '', 1), _capture(m.spaces_from_the_end, 'ab', 3)]]",
            "[m.order_of_each(s) for s in ('amz', 'éħ\\u0100', '\\U0001f389\\uffff')]",
            "[m.astral_at(s) for s in ('\\U0001f389', '\\U0001f38a', 'a', '\\ud83c\\udf89'.encode('utf-16', 'surrogatepass').decode('utf-16'))]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.at_a_huge_index, 'ab'), _capture(m.at_a_huge_negative_index, 'ab')]]",
            // a subclass may hand back any text at all from `__getitem__`, and the
            // machine index has to reach that the same way the tagged one does
            "[m.spaces(type('S', (str,), {'__getitem__': lambda s, i: ' '})('ab'))]",
            "[m.spaces(type('S', (str,), {'__getitem__': lambda s, i: '  '})('ab'))]",
            "[m.spaces(type('S', (str,), {'__getitem__': lambda s, i: ''})('ab'))]",
            "[m.spaces(type('P', (str,), {})('a b')), m.spaces_from_the_end(type('P', (str,), {})('a b'), 3)]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.at_a_huge_index, type('P', (str,), {})('ab'))]]",
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
    let dir = diff_root().join("by_diff_nulstr_which");
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
    let dir = diff_root().join("by_diff_callcheck");
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
    let dir = diff_root().join("by_diff_nameerr");
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

/// a builtin is resolved through the module the function was defined in, and never
/// through whichever frame called it
///
/// python binds a function's builtins once, at the `def`, by reading
/// `__globals__['__builtins__']`; the caller's namespace never enters into it. an
/// emitted function pushes no frame at all, so a runtime that asked the interpreter
/// which builtins were in scope was asking about the caller — and one call site could
/// then be reached under two namespaces without either of them being written to.
///
/// this used to pin that wrong answer, on the grounds that the memo must not widen a
/// divergence older than itself. the divergence is gone, so what it pins now is the
/// twin's answer
#[test]
fn a_builtin_resolves_through_the_defining_module_and_not_the_caller() {
    agree_python(
        "callerbuiltins",
        "\
def digits(n: int) -> str:
    return 'k' + str(n)

def absentee() -> object:
    return nowhere
",
        &[
            // the first call arms a site against the module's own builtins, the second
            // is made from a frame standing in another namespace entirely, and the
            // third is what a site left holding the impostor would fail
            "[m.digits(7), _under_builtins(lambda: m.digits(7), str=lambda n: 'F'), m.digits(7)]",
            // a name the caller's builtins bind and the module's do not is still a
            // `NameError` — the wrong namespace answering would produce a value
            "_under_builtins(lambda: repr(_capture(m.absentee)), nowhere='reachable')",
            // and the frame that asks for the import has no more say than the frame
            // that calls: a module body binds the interpreter's builtins either way
            "_reimported_under_builtins(m.__name__, 'm.digits(7)', str=lambda n: 'F')",
        ],
    );
}

/// deleting a module global falls the read through to the module's own builtins
///
/// the two namespaces a global can be answered by are reached in order, so this is
/// the case where the second one is consulted for a name the first used to bind —
/// under a caller that binds it differently again, to keep the fall-through honest
/// about which builtins it falls through to
#[test]
fn a_deleted_module_global_falls_through_to_the_defining_module_s_builtins() {
    agree_python(
        "deletedfallthrough",
        "\
def read() -> object:
    return picked
",
        &["(setattr(__import__('builtins'), 'picked', 'builtins'), \
              m.__dict__.__setitem__('picked', 'module'), \
              m.read(), \
              m.__dict__.pop('picked'), \
              _under_builtins(m.read, picked='caller'), \
              m.read())[2:]"],
    );
}

/// the `__builtins__` a module carries is the interpreter's, and reading it happens
/// once
///
/// python fills the entry in when it executes a module body and a function made by
/// that body holds what stood there *then*. rebinding the entry afterwards is
/// therefore not a way to redirect anything already defined — which is what lets the
/// memo below treat the namespace as fixed. the entry is allowed to be the `builtins`
/// module or its dict, and neither form is a redirection either
#[test]
fn a_module_s_builtins_entry_is_the_interpreter_s_and_is_read_once() {
    agree_python(
        "builtinsentry",
        "\
def digits(n: int) -> str:
    return 'k' + str(n)
",
        &[
            "[m.__dict__['__builtins__'] is vars(__import__('builtins')), m.digits(7)]",
            "(m.__dict__.__setitem__('__builtins__', {'str': lambda n: 'X'}), m.digits(7))[1]",
            "(m.__dict__.__setitem__('__builtins__', __import__('builtins')), m.digits(7))[1]",
            // a reload runs an interpreted module's body again over the namespace it
            // already has, where an extension module's exec slot is not run a second
            // time at all. so the two builds arrive at this answer by different roads
            // — and the entry standing here as the `builtins` module rather than its
            // dict is the one way the road the extension does not currently take
            // would be told apart
            "(m.__dict__.__setitem__('__builtins__', __import__('builtins')), \
              __import__('importlib').reload(m).digits(7))[1]",
        ],
    );
}

/// a builtin rebound while a call site is already holding it is seen at once
///
/// the site remembers which namespace answered, so *that* namespace has to be one
/// this build hears about being written to. a builtins namespace is only ever
/// reached after the module's has missed, so a test that moves a module binding
/// underneath a site never gets to the case where it is builtins that moved
#[test]
fn a_rebound_builtin_is_seen_while_a_site_still_holds_it() {
    agree_python(
        "builtinrebind",
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
            "(setattr(__import__('builtins'), 'picked', lambda: 'first'), \
             m.twice(lambda: setattr(__import__('builtins'), 'picked', lambda: 'second')))[1]",
        ],
    );
}

/// a name that is bound nowhere is asked about again the next time
///
/// a refusal has no dict entry behind it, so there is no write that could invalidate
/// a memo of one. a `NameError` is therefore derived afresh every time rather than
/// remembered — otherwise a name defined after the first read would never be found
#[test]
fn a_name_that_resolved_to_nothing_is_not_remembered() {
    agree_python(
        "namelater",
        "\
def read() -> object:
    return arriving
",
        &[
            "[type(_capture(m.read)).__name__, \
              (m.__dict__.__setitem__('arriving', 'here'), m.read())[1], \
              (m.__dict__.pop('arriving'), type(_capture(m.read)).__name__)[1]]",
            // the same over *builtins*, which is the case a memo of the refusal would
            // survive: the module namespace is written to on the way out of the first
            // read above, and a write is the one thing that could throw such a memo
            // away. nothing this module resolves ever reaches builtins, so nothing has
            // asked to hear about writes to it either
            "[type(_capture(m.read)).__name__, \
              (setattr(__import__('builtins'), 'arriving', 'here'), m.read())[1], \
              (delattr(__import__('builtins'), 'arriving'), \
               type(_capture(m.read)).__name__)[1]]",
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
fn a_list_subclass_is_iterated_through_its_own_dunders() {
    // a `for` over an exact list is walked by index, which is only sound because the
    // exactness test sends everything else through `iter()`. a subclass may have
    // overridden `__iter__` and may hand back anything at all, and reading its
    // `ob_item` would silently answer with what the override was written to hide
    agree(
        "iterexact",
        "\
def walked(o) -> object:
    seen = []
    for x in o:
        seen.append(x)
    return seen
",
        &[
            // `__iter__` overridden: the elements the loop sees are not the ones stored
            "m.walked(type('D', (list,), \
             {'__iter__': lambda self: iter([9, 8])})([1, 2, 3]))",
            // and a subclass that overrides nothing still has to agree
            "m.walked(type('P', (list,), {})([1, 2, 3]))",
            // a plain list is the arm the index walk is for
            "m.walked([1, 2, 3])",
        ],
    );
}

#[test]
fn a_list_resized_under_a_for_loop_agrees() {
    // cpython's list iterator holds a position and re-reads the length every step, so
    // a list appended to under a `for` keeps feeding it and one popped from ends it
    // early. reading the length once at the top would be faster and would disagree
    // with the interpreter about both
    agree(
        "iterresize",
        "\
def growing(xs: list[int], limit: int) -> object:
    seen = []
    for x in xs:
        seen.append(x)
        if len(xs) < limit:
            xs.append(x + 10)
    return seen

def shrinking(xs: list[int]) -> object:
    seen = []
    for x in xs:
        seen.append(x)
        if xs:
            xs.pop()
    return seen

def cleared(xs: list[int]) -> object:
    seen = []
    for x in xs:
        seen.append(x)
        xs.clear()
    return seen

def deleted(xs: list[int]) -> object:
    seen = []
    for x in xs:
        seen.append(x)
        if xs:
            del xs[0]
    return seen
",
        &[
            "m.growing([1, 2], 5)",
            "m.growing([], 3)",
            "m.shrinking([1, 2, 3, 4, 5])",
            "m.shrinking([1])",
            "m.cleared([1, 2, 3])",
            "m.deleted([1, 2, 3, 4, 5])",
            "m.deleted([1])",
        ],
    );
}

#[test]
fn rebinding_the_name_a_for_loop_reads_does_not_move_the_loop() {
    // the loop walks the list it opened over, not whatever the name says later. it
    // holds a reference of its own for exactly that reason — and a step that read the
    // *name* each trip would follow the rebinding and walk a list nobody asked for
    agree(
        "iterrebind",
        "\
def rebound(xs: list[int]) -> object:
    seen = []
    for x in xs:
        seen.append(x)
        xs = [99, 98, 97]
    return seen

def dropped(xs: list[int]) -> object:
    seen = []
    for x in xs:
        seen.append(x)
        xs = []
    return seen
",
        &[
            "m.rebound([1, 2, 3])",
            "m.rebound([])",
            "m.dropped([1, 2, 3])",
        ],
    );
}

#[test]
fn a_for_loop_keeps_the_list_it_walks_alive() {
    // the loop holds a reference for exactly as long as it would have held one to a
    // `list_iterator`, which is what keeps a `for` over a temporary reading memory
    // that is still there. the elements are dropped as the only reference to them
    // goes, so a step that borrowed rather than owning would hand back a dead object
    agree(
        "itertemp",
        "\
def made(n: int) -> object:
    seen = []
    for x in [n, n + 1, n + 2]:
        seen.append(x)
    return seen

def doubled(xs: list[int]) -> object:
    seen = []
    for x in list(xs):
        seen.append(x)
    return seen
",
        &[
            "m.made(1)",
            "m.made(-3)",
            "m.doubled([1, 2, 3])",
            "m.doubled([])",
        ],
    );
}

#[test]
fn a_for_loop_starts_over_on_every_trip_through_an_enclosing_one() {
    // one cursor register serves every trip through whatever encloses it, so it is set
    // where the loop opens rather than where the register is declared. left unset on
    // the second trip, an inner loop would carry on from where the first one stopped
    agree(
        "iternest",
        "\
def pairs(xs: list[int], ys: list[int]) -> object:
    seen = []
    for x in xs:
        for y in ys:
            seen.append(x * 10 + y)
    return seen

def restarted(xs: list[int], times: int) -> object:
    seen = []
    n = 0
    while n < times:
        for x in xs:
            seen.append(x)
        n = n + 1
    return seen
",
        &[
            "m.pairs([1, 2], [3, 4])",
            "m.pairs([1, 2], [])",
            "m.pairs([], [3, 4])",
            "m.restarted([1, 2], 3)",
            "m.restarted([], 2)",
        ],
    );
}

#[test]
fn a_for_loop_in_a_generator_resumes_where_it_left_off() {
    // a generator parks its iterator in a field because no register survives a
    // `yield`, and a cursor is a register — so a generator's loop keeps the protocol.
    // given one, a resumed frame would start again from whatever an unset register
    // holds, which is to say from the top
    agree(
        "itergen",
        "\
def each(xs: list[int]) -> object:
    for x in xs:
        yield x
        yield x + 100

def taken(xs: list[int]) -> object:
    return list(each(xs))
",
        &[
            "m.taken([1, 2, 3])",
            "m.taken([])",
            "list(m.each([4, 5]))",
            // stopping part way and resuming is the same question asked once
            "[next(it) for it in [iter(m.each([7, 8]))] for _ in range(3)]",
        ],
    );
}

#[test]
fn iterating_a_wrongly_typed_list_raises() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_iterchk");
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

/// a call site remembers what a method name resolved to, so every way that answer
/// could stop being the right one has to be asked of it
///
/// the one that matters here is a *different class*: an annotation of `str` admits a
/// subclass, and a subclass may have overridden the method. a site is also asked to
/// serve receivers of several types in turn, because the answer it remembers is only
/// ever for one of them — and `split` is asked with and without its argument, which
/// are two different operations rather than one with a default
#[test]
fn a_remembered_method_still_reaches_an_override() {
    agree_python(
        "methodsite",
        "\
def shout(s: str) -> str:
    return s.upper()

def words(line: str, sep: str) -> object:
    return line.split(sep)

# no argument splits on runs of whitespace and drops the empties, where a single
# space keeps every one of them
def loose(line: str) -> object:
    return line.split()

def leads(s: str, prefix) -> object:
    return s.startswith(prefix)

def glued(sep: str, parts: object) -> str:
    return sep.join(parts)

# one site, several receiver types in turn: a str, a list and a tuple all reach
# their own `count` through the same call
def counted(o, x) -> object:
    return o.count(x)

def fetched(o, k) -> object:
    return o.get(k)

# a method call in a loop is what a site is for, and where a receiver that changes
# type part way through would be missed
def shout_all(items: list) -> object:
    out = []
    for i in items:
        out.append(i.upper())
    return out
",
        &[
            "[m.shout(s) for s in ('', 'abc', 'ABC', 'é', '🎉', 'ß')]",
            // a subclass that overrides the method, and one that inherits it
            "m.shout(type('S', (str,), {'upper': lambda self: 'Z'})('ab'))",
            "m.shout(type('P', (str,), {})('ab'))",
            // the exact class first, then the subclass, then the exact class again
            "[m.shout(s) for s in ('ab', type('S', (str,), {'upper': lambda self: 'Z'})('cd'), 'ef')]",
            "[m.words(line, ' ') for line in ('', 'a', 'a b c', ' a  b ', 'a b ')]",
            "[m.words('a-b-c', s) for s in ('-', 'x', 'a')]",
            "[m.loose(line) for line in ('', '   ', 'a b c', ' a  b ', 'a\\tb\\nc')]",
            "m.words(type('S', (str,), {'split': lambda self, sep: ['Z']})('a b'), ' ')",
            "[m.leads(s, 'w') for s in ('', 'w', 'word', 'xw')]",
            // `startswith` also takes a tuple of prefixes, and refuses anything else
            "[m.leads('word', p) for p in ('w', ('x', 'w'), ('x',), ())]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.leads, 'word', 5)]]",
            "[m.glued(s, p) for s in ('', '-', '::') for p in ([], ['a'], ['a', 'b'], ('c', 'd'))]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.glued, '-', [1, 2]), _capture(m.glued, '-', 5)]]",
            "m.glued(type('S', (str,), {'join': lambda self, p: 'Z'})('-'), ['a'])",
            "[m.counted(o, 'a') for o in ('banana', ['a', 'b', 'a'], ('a',))]",
            "[m.counted(o, 1) for o in ([1, 1, 2], (1,))]",
            "[m.fetched(o, 'k') for o in ({'k': 1}, {}, type('D', (dict,), {'get': lambda self, k: 'Z'})())]",
            "m.shout_all(['ab', 'cd'])",
            "m.shout_all([])",
            "m.shout_all(['ab', type('S', (str,), {'upper': lambda self: 'Z'})('cd'), 'ef'])",
        ],
    );
}

/// the other way a remembered answer stops being right: the method is rebound on
/// the type after the site was armed
///
/// the interpreter's version tag is what the site tests, and it is bumped by a write
/// to the class or to any of its bases — so both are written to here, each after a
/// call that has already armed the site against the old body
#[test]
fn a_remembered_method_notices_the_type_being_rebound() {
    agree_python(
        "methodrebind",
        "\
def speak(o) -> object:
    return o.speak()

def speak_twice(o) -> object:
    return (o.speak(), o.speak())
",
        &[
            "[m.speak(o) for o in (type('A', (), {'speak': lambda s: 'a'})(), type('B', (), {'speak': lambda s: 'b'})())]",
            "\
(lambda C: [
    m.speak_twice(C()),
    setattr(C, 'speak', lambda self: 'second'),
    m.speak_twice(C()),
])(type('C', (), {'speak': lambda self: 'first'}))",
            // the same, but the write lands on a base rather than on the class itself
            "\
(lambda B, C: [
    m.speak_twice(C()),
    setattr(B, 'speak', lambda self: 'second'),
    m.speak_twice(C()),
])(*(lambda B: (B, type('C', (B,), {})))(type('B', (), {'speak': lambda self: 'first'})))",
        ],
    );
}

#[test]
fn a_missing_attribute_raises() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_attrerr");
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
    let dir = diff_root().join("by_diff_listbuild");
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
    let dir = diff_root().join("by_diff_rebind");
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

/// a decorator is evaluated once, and what it did on the way happened once
///
/// python evaluates it where the `def` stands. the interpreted twin is what stands there
/// and module init evaluates it again over the compiled definition, so `mark` appended
/// twice while the name it left behind was right either way — the whole of the defect was
/// the second append. `mark` returns what it was handed, so nothing but `marked` can
/// show it
#[test]
fn a_decorator_runs_once() {
    agree(
        "decoratoronce",
        "\
marked: list[int] = []


def mark(f: object) -> object:
    marked.append(1)
    return f


@mark
def counted() -> int:
    return 1


@mark
def counted_twice() -> int:
    return 2
",
        &["m.counted()", "m.counted_twice()", "m.marked"],
    );
}

/// a definition the module *reads* keeps its decorator, and declines
///
/// taking the decorator out of the twin's source leaves the name holding an undecorated
/// definition from the twin's `def` until module init reaches it — a window nothing can
/// see unless the module's own body looks. `AT_IMPORT` looks directly and `alias` keeps
/// what it found, and both of them would otherwise hold what `double` never wrapped
#[test]
fn a_decorated_definition_the_module_reads_declines() {
    agree_with_declines(
        "decoratorread",
        "\
def double(f) -> object:
    def wrapper() -> int:
        return f() * 2
    return wrapper


@double
def one() -> int:
    return 1


AT_IMPORT = one()
alias = one
",
        &["m.one()", "m.AT_IMPORT", "m.alias()"],
    );
}

/// the source both method-decorator tests below compile
///
/// `mark` hands back what it was given, so the binding is right however many times it ran
/// and `seen` is the one thing that can show a second run. `double` is the other half: it
/// wraps, so it says which application ended up installed
const MARKED_METHODS: &str = "\
seen = []


def mark(f):
    seen.append(f.__name__)
    return f


def double(f):
    def wrapper(self) -> int:
        return f(self) * 2
    return wrapper


class C:
    @mark
    def g(self) -> int:
        return 1

    @double
    def doubled(self) -> int:
        return 3

    def calls_doubled(self) -> int:
        return self.doubled()

    def plain(self) -> int:
        return 2
";

/// a method's decorator is evaluated once, and what it did on the way happened once
///
/// a method's decorators run *inside* the class body, so the interpreted twin already
/// applied them before anything of the module was installed. module init then applied them
/// a second time to the native method: the value installed was right — the second
/// application is the one that wins — which is exactly what made it silent. `seen` read
/// `['g', 'g']` where python reads `['g']`, so a decorator that registers registered
/// twice
#[test]
fn a_method_decorator_runs_once() {
    agree_python(
        "methoddecoratoronce",
        MARKED_METHODS,
        &[
            "m.C().g()",
            "m.C().doubled()",
            // through another method too: an internal call must reach the same
            // decorated method an external one does
            "m.C().calls_doubled()",
            "m.seen",
        ],
    );
}

/// and the decorated method is the *interpreted* one, which is the price of the above
///
/// a decorator is handed whatever the class body gave it, and there is no way to hand it
/// the native method without calling it a second time. so a decorated method is no longer
/// native on the type — while an undecorated one is untouched, which is where a compiled
/// class's speed lives. `type(...)` is what can tell the two apart: a compiled type holds
/// a `method_descriptor` where an interpreted class holds a plain function
#[test]
fn a_decorated_method_is_the_interpreted_one_and_its_siblings_are_not() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_methoddecoratoronce_t");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        MARKED_METHODS,
        "by_diff_methoddecoratoronce_t",
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
        "import by_diff_methoddecoratoronce_t as m\n\
         print(type(m.C.__dict__['g']).__name__,\n\
         \x20     type(m.C.__dict__['doubled']).__name__,\n\
         \x20     type(m.C.__dict__['plain']).__name__)\n",
    );
    // the two decorated methods are what their own decorators returned — `mark` hands
    // back the interpreted function it was given, `double` hands back its wrapper —
    // and the untouched sibling is still the compiled type's own
    assert_eq!(out, "function function method_descriptor");
}

/// the source both class-decorator tests below compile
///
/// `mark` hands back what it was given and records only the name, so the binding is right
/// however many times it ran and `seen` is the one thing that can show a second run
const MARKED_CLASSES: &str = "\
seen = []


def mark(o):
    seen.append(o.__name__)
    return o


@mark
class Marked:
    def g(self) -> int:
        return 1


@mark
class Second:
    def h(self) -> int:
        return 2
";

/// a class's decorator is evaluated once, and what it did on the way happened once
///
/// python runs it where the `class` statement stands. the interpreted twin is what stands
/// there and module init ran it again over the namespace entry the compiled type had
/// taken, so `seen` read `['Marked', 'Second', 'Marked', 'Second']` where python reads
/// `['Marked', 'Second']` — and the class each name ended up bound to was right either
/// way, which is what made it silent
#[test]
fn a_class_decorator_runs_once() {
    agree_python(
        "classdecoratoronce",
        MARKED_CLASSES,
        &["m.Marked().g()", "m.Second().h()", "m.seen"],
    );
}

#[test]
fn a_decorated_class_is_the_compiled_type() {
    // the counts above are answered identically by a class that fell back to its
    // interpreted definition, so they cannot say which build answered.
    // `method_descriptor` can: a compiled type holds one where the interpreted class
    // holds a plain function
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_classdecoratoronce_t");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        MARKED_CLASSES,
        "by_diff_classdecoratoronce_t",
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
        "import by_diff_classdecoratoronce_t as m\n\
         print(type(m.Marked.__dict__['g']).__name__,\n\
         \x20     type(m.Second.__dict__['h']).__name__)\n\
         print(m.seen)\n",
    );
    assert_eq!(
        out,
        "method_descriptor method_descriptor\n['Marked', 'Second']"
    );
}

/// a decorated class the module *reads* keeps its decorator, and declines
///
/// taking the decorator out of the twin's source leaves the interpreted definition
/// standing undecorated from its `class` statement until module init reaches it. `TABLE`
/// looks in that window and keeps what it found, so the list would hold a class the
/// module's own name no longer means — `TABLE[0] is Held` would answer `False`
#[test]
fn a_decorated_class_the_module_reads_declines() {
    agree_python_with_declines(
        "classdecoratorread",
        "\
seen = []


def mark(o):
    seen.append(o.__name__)
    return o


@mark
class Held:
    def value(self) -> int:
        return 1


TABLE = [Held]
",
        &["m.Held().value()", "m.seen", "m.TABLE[0] is m.Held"],
    );
}

/// a class named in an *unevaluated* annotation is not read, and keeps compiling
///
/// `from __future__ import annotations` makes `Held` in that signature a string nothing
/// evaluates, so the module never holds the undecorated definition and the decorator can
/// still move to init. without the future import this is `TABLE = [Held]` again
#[test]
fn a_decorated_class_named_in_a_deferred_annotation_still_compiles() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_classdecoratorannotation");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
from __future__ import annotations

seen = []


def mark(o):
    seen.append(o.__name__)
    return o


@mark
class Held:
    def value(self) -> int:
        return 1


def through(h: Held) -> int:
    return h.value()
";
    let built = match build_source(
        source,
        "by_diff_classdecoratorannotation",
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
        "import by_diff_classdecoratorannotation as m\n\
         print(type(m.Held.__dict__['value']).__name__, m.through(m.Held()))\n\
         print(m.seen)\n",
    );
    assert_eq!(out, "method_descriptor 1\n['Held']");
}

/// a decorator written as a chain of attributes, which is what the ir grew an
/// expression to hold
///
/// the ir carried a single `String` and codegen emitted one interned lookup of it, so
/// `functools.cache` was a name to find whole in the module dict — it was declined
/// rather than compiled, which is why it never raised. every step of a path is a *read*,
/// which is what makes evaluating it at module init mean what it meant where the `def`
/// stood
#[test]
fn a_decorator_written_as_a_path_agrees() {
    agree_python(
        "pathdeco",
        "\
import abc
import functools

CALLS = []


class Wrappers:
    @staticmethod
    def tag(cls: type) -> type:
        cls.tag = 'seen'
        return cls


@functools.cache
def cached(n: int) -> int:
    CALLS.append(n)
    return n * 2


class Marks:
    @abc.abstractmethod
    def area(self) -> int:
        return 3


@Wrappers.tag
class Held:
    def __init__(self, n: int) -> None:
        self.n = n

    def read(self) -> int:
        return self.n


def probe() -> int:
    return cached(4) + cached(4)
",
        &[
            // the wrapper the decorator returned is what the name holds, and it is
            // reached from inside the module as well as outside
            "m.probe()",
            "m.cached(9)",
            // and it ran once, so the second call was a cache hit
            "[m.probe(), m.CALLS]",
            "type(m.cached).__name__",
            // a path off something that is not a module resolves the same way
            "m.Held(2).read()",
            "m.Held.tag",
            // a method's decorator is resolved out of the module namespace too
            "m.Marks.area.__isabstractmethod__",
            "m.Marks().area()",
        ],
    );
}

/// the compiled leg answers the path-decorated definitions
///
/// the differential legs agree whichever one answered, so a decorator test passes with
/// the codegen path switched off. this is where it is pinned.
///
/// a *decorated method* is deliberately not one of the things `type` can pin any more: it
/// is the interpreted definition on purpose, because a decorator is handed whatever the
/// class body gave it and applying it again to the native method would run it twice. so
/// the class is pinned by its undecorated sibling, which is still the compiled type's own
/// `method_descriptor`, and the decorator by the effect it had
#[test]
fn a_path_decorated_definition_is_the_compiled_one() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_pathdecolive");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
import abc
import functools


class Wrappers:
    @staticmethod
    def tag(cls: type) -> type:
        cls.tag = 'seen'
        return cls


@functools.cache
def cached(n: int) -> int:
    return n * 2


class Marks:
    @abc.abstractmethod
    def area(self) -> int:
        return 3

    def sized(self) -> int:
        return 4


@Wrappers.tag
class Held:
    def __init__(self, n: int) -> None:
        self.n = n

    def read(self) -> int:
        return self.n
";
    let built = match build_source(
        source,
        "by_diff_pathdecolive",
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
        "import by_diff_pathdecolive as m\n\
         print(type(m.cached).__name__, type(m.cached.__wrapped__).__name__)\n\
         print(type(m.Marks.area).__name__, type(m.Marks.sized).__name__,\n\
         \x20     type(m.Held.read).__name__)\n\
         print(m.cached(4), m.Marks.area.__isabstractmethod__, m.Held.tag)\n",
    );
    assert_eq!(
        out,
        "_lru_cache_wrapper builtin_function_or_method\n\
         function method_descriptor method_descriptor\n\
         8 True seen"
    );
}

/// a decorator that is a call keeps its decline
///
/// python calls `mark('x')` where the `def` stands. module-level code is not compiled,
/// so the only moment init has is the end of the module — by which time the interpreted
/// twin has already made that call. making it again would be a second one, in the wrong
/// place, with whatever it did on the way happening twice
#[test]
fn a_decorator_that_is_a_call_declines() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_calldeco");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
MADE = []


def mark(label: str):
    MADE.append(label)

    def apply(fn):
        fn.label = label
        return fn

    return apply


@mark('x')
def f(n: int) -> int:
    return n + 1
";
    let built = match build_source(
        source,
        "by_diff_calldeco",
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
        built
            .declined
            .iter()
            .any(|declined| declined.reason.contains("run it a second time")),
        "declined: {:?}",
        built.declined
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_calldeco as m\n\
         print(m.f(1), m.f.label, m.MADE, type(m.f).__name__)\n",
    );
    // the factory ran once, where it was written, and the interpreted definition is
    // what the name holds
    assert_eq!(out, "2 x ['x'] function");
}

/// a decorator rooted at a name the class body bound keeps its decline
///
/// a decorator is resolved out of the *module* namespace at init, where a name the class
/// body bound does not exist — the lookup would raise `NameError` and take the whole
/// extension's import with it. `@total.setter` is the one shape that is exempt, because
/// it is not resolved at all: the pair it belongs to is lowered as the one attribute
/// python folds it into, so `Box` stands beside `Rooted` as the case that still compiles
#[test]
fn a_decorator_rooted_in_the_class_body_declines() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_classrooteddeco");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Box:
    def __init__(self, n: int) -> None:
        self._n = n

    @property
    def total(self) -> int:
        return self._n

    @total.setter
    def total(self, value: int) -> None:
        self._n = value


class Rooted:
    def wrap(fn):
        return fn

    @wrap
    def value(self) -> int:
        return 3
";
    let built = match build_source(
        source,
        "by_diff_classrooteddeco",
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
    let reasons = |needle: &str| {
        built
            .declined
            .iter()
            .any(|declined| declined.reason.contains(needle))
    };
    assert!(
        reasons("`wrap` is bound by the class body"),
        "declined: {:?}",
        built.declined
    );
    assert!(
        !reasons("`total` is defined more than once"),
        "declined: {:?}",
        built.declined
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_classrooteddeco as m\n\
         b = m.Box(1)\n\
         b.total = 7\n\
         print(b.total, type(m.Box.total).__name__,\n\
        \x20     type(m.Box.__dict__['total'].fget).__name__, m.Rooted().value())\n",
    );
    // `Box` publishes its pair as the `property` an interpreted class publishes — so the
    // descriptor's own type no longer says which leg answered, and the half inside it is
    // what does: a `method_descriptor` is compiled, and the interpreted definition this
    // could have fallen back to would hold a `function`. `Rooted` fell back and keeps its
    // own
    assert_eq!(out, "7 property method_descriptor 3");
}

/// a class whose type slots publish more than its body wrote keeps its decorator's
/// decline
///
/// python reaches `<=` through `tp_richcompare`, one slot behind all six comparisons —
/// so an emitted type that writes `__lt__` publishes `__le__` as well, answering
/// `NotImplemented`. `functools.total_ordering` reads exactly that: it saw `__le__`
/// already there, filled in nothing, and `a <= b` raised where the interpreted class
/// answered `True`. that was a live wrong answer for the plain-name spelling before the
/// path spelling could reach it at all
#[test]
fn a_class_decorator_over_a_partly_filled_slot_declines() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_partialslot");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
import functools


@functools.total_ordering
class Ranked:
    def __init__(self, n: int) -> None:
        self.n = n

    def __eq__(self, other: object) -> bool:
        return isinstance(other, Ranked) and self.n == other.n

    def __lt__(self, other: object) -> bool:
        return self.n < other.n
";
    let built = match build_source(
        source,
        "by_diff_partialslot",
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
        built
            .declined
            .iter()
            .any(|declined| declined.reason.contains("publishes `__le__`")),
        "declined: {:?}",
        built.declined
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_partialslot as m\n\
         print(m.Ranked(1) <= m.Ranked(2), m.Ranked(3) > m.Ranked(2))\n\
         print(type(m.Ranked.__init__).__name__)\n",
    );
    assert_eq!(out, "True True\nfunction");
}

#[test]
fn a_call_to_a_decorated_function_from_the_same_module_agrees() {
    // the module namespace holds what the decorator returned; the native entry holds
    // what it was handed. a call written inside the module used to reach the entry, so
    // `caller(1)` answered 2 compiled and 4 interpreted with nothing said about it
    agree_python(
        "decoratedcallee",
        "\
def double(fn):
    def inner(x: int) -> int:
        return fn(x) * 2
    return inner


@double
def f(x: int) -> int:
    return x + 1


def caller(x: int) -> int:
    return f(x)


def plain(x: int) -> int:
    return x + 1


def other(x: int) -> int:
    return plain(x)
",
        &[
            "m.caller(1)",
            "m.f(1)",
            "m.other(1)",
            // the decorator ran exactly once on whatever the name holds, rather than
            // once per call site
            "[m.caller(1), m.caller(1), m.f(1)]",
        ],
    );
}

#[test]
fn a_construction_of_a_decorated_class_from_the_same_module_agrees() {
    // a construction is written against the *name*, and a class decorator is what the
    // name then holds. allocating the emitted layout instead skipped the decorator:
    // `probe()` answered 3 compiled and 103 interpreted
    agree_python(
        "decoratedclassctor",
        "\
class Other:
    def __init__(self, x: int) -> None:
        self.x = x + 100


def swap(cls):
    return Other


@swap
class C:
    def __init__(self, x: int) -> None:
        self.x = x


def probe(x: int) -> int:
    return C(x).x
",
        &[
            "m.probe(3)",
            "m.C(3).x",
            "m.Other(1).x",
            // the name is what a construction resolves, from either side of the module
            "m.C is m.Other",
        ],
    );
}

#[test]
fn a_method_modifier_is_not_looked_up_as_a_name() {
    // a modifier reaches the ast as a decorator with no `@`. it was emitted as a name
    // to look up in the module namespace at init, and there is no such name — so the
    // extension built cleanly and then failed to import outright with `NameError: name
    // 'override' is not defined`, taking every function in the module with it.
    //
    // `static` is the sharper case and is not here: it declines, because a method's
    // slot zero is forced to the receiver and `staticmethod` says it is not one
    agree(
        "methodmodifier",
        "\
class Box:
    abstract def area(self) -> int:
        return 7

def probe() -> int:
    return Box().area()
",
        &[
            "m.probe()",
            "m.Box().area()",
            // the modifier became `abstractmethod`, so the method carries the same
            // marker the interpreted twin carries — not a name nobody bound
            "getattr(m.Box.area, '__isabstractmethod__', None)",
        ],
    );
}

#[test]
fn a_static_or_class_method_answers_the_same_through_the_class_and_through_an_instance() {
    // slot zero used to be forced to the receiver for every method, and these two say
    // it is not one — so the compiled `Box.make(3)` bound `3` to a `Box` and raised
    // `unbound method Box.make() needs an argument` at its first call.
    //
    // each is reached four ways: through the class, through an instance, from a method
    // of the same class, and from a module-level function. all four go through the
    // *descriptor* the type publishes, so a wrong convention shows up in every one
    agree_python(
        "staticmethod",
        "\
class Box:
    @staticmethod
    def make(x: int) -> int:
        return x + 7

    @classmethod
    def named(cls, y: int) -> str:
        return cls.__name__ + str(y)

    def n(self) -> int:
        return 1

    def inside(self) -> int:
        return Box.make(1) + len(Box.named(2))


class Alt:
    def __init__(self, v: int) -> None:
        self.v = v

    @classmethod
    def of(cls, v: int) -> \"Alt\":
        return cls(v)


def probe() -> int:
    return Box.make(3)
",
        &[
            "m.probe()",
            "m.Box.make(3)",
            "m.Box().make(3)",
            "m.Box.named(2)",
            "m.Box().named(2)",
            "m.Box().inside()",
            "m.Box.make(x=3)",
            // a class method is what an alternative constructor is written as, and
            // `cls(v)` has to reach the class it was called on
            "m.Alt.of(4).v",
            "type(m.Alt.of(4)).__name__",
            // `__self__` is the class for a class method and nothing at all for a
            // static one, on either build
            "m.Box.named.__self__ is m.Box",
            "getattr(m.Box.make, '__self__', None)",
        ],
    );
}

#[test]
fn a_static_or_class_method_is_the_compiled_one() {
    // neither `agree` can say which build answered — a class that declines answers
    // identically out of its interpreted definition. `type(C.__dict__['m'])` cannot
    // say either: it is `staticmethod` on both legs.
    //
    // the *descriptor* is what differs. a compiled static or class method is reached
    // through a `PyCFunction`, so `type(C.m)` is `builtin_function_or_method` where the
    // interpreted leg has a plain `function` or a bound `method` — and the class
    // method's dict entry is a `classmethod_descriptor` rather than a `classmethod`
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_staticmethod_which");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Box:
    @staticmethod
    def make(x: int) -> int:
        return x + 7

    @classmethod
    def named(cls, y: int) -> str:
        return cls.__name__ + str(y)
";
    let built = match build_source(
        source,
        "by_diff_staticmethod_which",
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
        "import by_diff_staticmethod_which as m\n\
         print(type(m.Box.make).__name__, type(m.Box.__dict__['make']).__name__)\n\
         print(type(m.Box.named).__name__, type(m.Box.__dict__['named']).__name__)\n\
         print(m.Box.make(3), m.Box.named(2))\n",
    );
    assert_eq!(
        out,
        "builtin_function_or_method staticmethod\n\
         builtin_function_or_method classmethod_descriptor\n\
         10 Box2"
    );
}

#[test]
fn a_static_or_class_method_on_a_class_built_through_its_metaclass_answers() {
    // a class on a base out of this module may be built by *calling* its metaclass,
    // and that construction puts the method table into a namespace rather than onto a
    // type — so the descriptor is one the runtime builds. building the plain kind for
    // either of these would hand the function the wrong receiver
    agree_python(
        "staticmethodmeta",
        "\
import collections.abc


class Sized(collections.abc.Sized):
    def __len__(self) -> int:
        return 3

    @staticmethod
    def tag() -> str:
        return \"sized\"

    @classmethod
    def kind(cls) -> str:
        return cls.__name__
",
        &[
            "m.Sized.tag()",
            "m.Sized().tag()",
            "m.Sized.kind()",
            "m.Sized().kind()",
            "len(m.Sized())",
            "type(m.Sized).__name__",
        ],
    );
}

#[test]
fn a_static_method_the_boundary_hands_over_reaches_the_plain_function() {
    // a boundary that cannot establish a parameter hands the whole call to the
    // interpreted twin, and for a *method* that twin is taken off the class with the
    // receiver put back in front of it. a static method has no receiver — `self` holds
    // nothing at all — and the twin taken off the class is already the plain function
    // the `staticmethod` wraps, so prepending anything would have handed it NULL.
    //
    // both reasons to hand over are here: `float` admits an `int`, and a default that
    // is not an immediate is one object every call has to share
    agree_python(
        "staticmethoddefer",
        "\
DEFAULT = [1, 2]


class Box:
    @staticmethod
    def half(x: float) -> float:
        return x / 2

    @staticmethod
    def counted(xs=DEFAULT) -> int:
        return len(xs)

    @staticmethod
    def nests(n: int) -> int:
        def inner(k: int) -> int:
            return k + n
        return inner(1)


def nests(n: int) -> int:
    def inner(k: int) -> int:
        return k + n + 100
    return inner(1)
",
        &[
            "m.Box.half(5.0)",
            // the int arrives where a `double` was compiled, so this is the call that
            // goes back to the interpreted definition
            "m.Box.half(5)",
            "m.Box().half(5)",
            "m.Box.counted()",
            "m.Box.counted([1])",
            "m.Box.counted() and m.Box.counted() is not None",
            // the object the default holds is the module's, shared by every call
            "m.Box.counted.__self__ if hasattr(m.Box.counted, '__self__') else None",
            // a nested function lives on a generated class named after the frame that
            // makes it, and these two frames are both called `nests`
            "m.Box.nests(3)",
            "m.nests(3)",
        ],
    );
}

#[test]
fn a_class_method_the_boundary_would_hand_over_declines() {
    // the twin a method's boundary hands over to is taken off the interpreted class,
    // and for a class method python has already *bound* it — to that class, not to the
    // one in slot zero. handing it the class as well would give the body two of them
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_classmethod_defer");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
DEFAULT = [1, 2]


class Box:
    @classmethod
    def counted(cls, xs=DEFAULT) -> int:
        return len(xs)
";
    let built = match build_source(
        source,
        "by_diff_classmethod_defer",
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
            .contains("already bound to the interpreted class")),
        "declined: {:?}",
        built.declined
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_classmethod_defer as m\n\
         print(m.Box.counted(), m.Box.counted([1]))\n",
    );
    assert_eq!(out, "2 1");
}

#[test]
fn a_global_a_frame_assigns_reaches_the_module_namespace() {
    // the write half of a `global` declaration. there was no op for it, so the
    // assignment bound a local and the module's name kept its old value — a wrong
    // answer rather than a missing one, and the reason this shape was declined.
    //
    // `agree_python` asserts nothing declined, and
    // `a_compiled_frame_is_what_reaches_the_module_namespace` below is what says the
    // native function is the one python calls
    agree_python(
        "globalwrite",
        "\
inited = False
counter = 0


def init() -> None:
    global inited, counter
    inited = True
    counter += 1


def bump(n: int) -> int:
    global counter
    counter = counter + n
    return counter
",
        &[
            // read from *outside* after a write from a compiled frame: a register
            // write is invisible here, which is the whole of the bug
            "(m.inited, m.counter)",
            "(m.init(), m.inited, m.counter)",
            "(m.init(), m.counter)",
            "(m.bump(5), m.counter)",
            // and the interpreted world's own write is what the compiled frame's
            // next read has to see, since both are the one dict
            "(setattr(m, 'counter', 100), m.bump(1), m.counter)",
        ],
    );
}

#[test]
fn a_global_a_frame_assigns_is_read_back_in_that_same_frame() {
    // the other half, and the one a write alone can get wrong in the opposite
    // direction: if the write reaches the namespace while a later read in the same
    // frame still resolves a register, the two halves stop agreeing with each other.
    //
    // this is `mimetypes` in miniature — a constructor that initializes the module the
    // first time it runs, and an `init` whose flag never landed, so the second
    // construction called it again and the compiled leg recursed until the stack ran out
    agree_python(
        "globalselfread",
        "\
inited = False
log: list[str] = []


class C:
    def __init__(self) -> None:
        if not inited:
            init()
        self.x = 1


def init() -> None:
    global inited
    log.append('init')
    inited = True
    # the read the write has to be visible to, in this frame and through `C`
    if inited:
        C()


def flip() -> str:
    global inited
    inited = not inited
    # written, read, written again: three answers out of one place
    first = inited
    inited = not inited
    return f'{first} {inited}'
",
        &[
            "(m.C().x, m.inited, m.log)",
            "m.flip()",
            "(m.flip(), m.inited)",
        ],
    );
}

#[test]
fn a_global_a_frame_deletes_leaves_the_name_unbound() {
    // `del x` under a `global x` unbinds the module's name, and reading it afterwards
    // is a `NameError` — which is not the `KeyError` deleting from a dict raises, nor
    // what a register could ever report
    agree_python(
        "globaldelete",
        "\
value = 1


def drop() -> str:
    global value
    del value
    try:
        return repr(value)
    except NameError as error:
        return f'NameError: {error}'


def again() -> str:
    global value
    try:
        del value
    except NameError as error:
        return f'NameError: {error}'
    return 'deleted'


def restore(n: int) -> int:
    global value
    value = n
    return value
",
        &[
            "m.drop()",
            "m.again()",
            "(m.restore(7), m.value)",
            "(m.drop(), m.again())",
        ],
    );
}

#[test]
fn a_local_a_frame_deletes_leaves_the_name_unbound() {
    // `del x` on a register clears the byte that says whether the local was written,
    // which is the same byte a read-before-write is guarded by. so afterwards a read
    // raises `UnboundLocalError`, a second `del` raises it too rather than doing
    // nothing, and a later write binds the name again
    agree_python(
        "localdelete",
        "\
import sys


def drop(n: int) -> str:
    x = n
    del x
    try:
        return repr(x)
    except UnboundLocalError as error:
        return f'UnboundLocalError: {error}'


def twice(n: int) -> str:
    x = n
    del x
    try:
        del x
    except UnboundLocalError as error:
        return f'UnboundLocalError: {error}'
    return 'deleted'


def rebound(n: int) -> int:
    x = n
    del x
    x = n + 1
    return x


def several(n: int) -> str:
    a = n
    b = n + 1
    del a, b
    try:
        return repr(a)
    except UnboundLocalError as error:
        return f'UnboundLocalError: {error}'


def stops_at_the_first(n: int) -> str:
    # the statement stops where it raises, so the second target is still bound
    a = n
    b = n + 1
    del a
    try:
        del a, b
    except UnboundLocalError:
        pass
    return repr(b)


def parameter(n: int) -> str:
    del n
    try:
        return repr(n)
    except UnboundLocalError as error:
        return f'UnboundLocalError: {error}'


def in_a_loop(n: int) -> str:
    seen = []
    for i in range(n):
        held = i * 2
        seen.append(held)
        del held
        try:
            seen.append(held)
        except UnboundLocalError:
            seen.append(-1)
    return repr(seen)


def unwound(n: int) -> str:
    # the answer to whether the name is bound has to survive the unwind, or the
    # handler reads a slot the deletion emptied
    x = n
    try:
        del x
        raise ValueError('boom')
    except ValueError:
        try:
            return repr(x)
        except UnboundLocalError as error:
            return f'UnboundLocalError: {error}'


def finally_sees_it(n: int) -> str:
    x = n
    try:
        del x
    finally:
        try:
            out = repr(x)
        except UnboundLocalError as error:
            out = f'UnboundLocalError: {error}'
    return out


def an_object(n: int) -> str:
    s = 'v' * n
    del s
    try:
        return s
    except UnboundLocalError as error:
        return f'UnboundLocalError: {error}'


def released(n: int) -> str:
    # a delta rather than an absolute count, so the two legs' own temporaries cancel:
    # the alias adds one reference and the deletion has to give it back
    target = [n]
    before = sys.getrefcount(target)
    alias = target
    during = sys.getrefcount(target)
    del alias
    after = sys.getrefcount(target)
    return f'{during - before}:{after - before}'


def drops_its_argument(target: object) -> int:
    # the frame never took a reference to an argument, so deleting the name must not
    # give one back. calling this repeatedly on the same object is the check: a
    # release too many walks the caller's count down to nothing
    del target
    return 0


def bracketed(n: int) -> str:
    # `del (a, b)` is `del a, b`: python deletes the elements of a display target
    # rather than the display
    a = n
    b = n + 1
    del (a, b)
    try:
        return repr(b)
    except UnboundLocalError as error:
        return f'UnboundLocalError: {error}'


def listed(n: int) -> str:
    a = n
    del [a]
    try:
        return repr(a)
    except UnboundLocalError as error:
        return f'UnboundLocalError: {error}'


def already_conditional(n: int) -> str:
    # the local is maybe-unassigned before the `del` as well as after it, so the two
    # reasons the byte exists meet on the same register
    if n > 0:
        held = n
    try:
        del held
    except UnboundLocalError as error:
        return f'UnboundLocalError: {error}'
    return 'deleted'


def dropped_and_returned(n: int) -> int:
    # the deleted name is never read again, so nothing but the exit path looks at the
    # slot — which has to release what an unbound register holds without reading it
    s = 'v' * n
    del s
    return n
",
        &[
            "m.drop(1)",
            "m.twice(1)",
            "m.rebound(4)",
            "m.several(2)",
            "m.stops_at_the_first(3)",
            "m.parameter(5)",
            "m.in_a_loop(3)",
            "m.unwound(6)",
            "m.finally_sees_it(7)",
            "m.an_object(3)",
            "m.released(9)",
            "m.bracketed(2)",
            "m.listed(2)",
            "m.already_conditional(1)",
            "m.already_conditional(-1)",
            "m.dropped_and_returned(3)",
            "(lambda o: (sys.getrefcount(o), \
             all(m.drops_its_argument(o) == 0 for _ in range(50)), \
             sys.getrefcount(o)))(['kept'])",
        ],
    );
}

#[test]
fn a_deleted_name_no_register_holds_stays_interpreted() {
    // four shapes the register's byte cannot answer for, each of which keeps the
    // function interpreted rather than answering wrongly: a name only ever deleted
    // (python makes it local for the whole function, so the reads elsewhere in the
    // body would have gone to the module namespace), a name a nested frame shares
    // (one cell, whose unbound state is the field being NULL), the same name deleted
    // from the *inner* side under `nonlocal`, and a name deleted after a suspension
    // (the byte is a register, and no register survives one)
    agree_python_with_declines(
        "localdeclinedelete",
        "\
count = 1


def only_deleted() -> str:
    try:
        del count
    except UnboundLocalError as error:
        return f'UnboundLocalError: {error}'
    return 'deleted'


def shared(n: int) -> str:
    held = n

    def read() -> int:
        return held

    out = read()
    del held
    try:
        return f'{out} {read()}'
    except NameError as error:
        return f'{out} NameError'


def declared_nonlocal(n: int) -> str:
    held = n

    def drop() -> None:
        nonlocal held
        del held

    drop()
    try:
        return repr(held)
    except UnboundLocalError as error:
        return f'UnboundLocalError: {error}'


def across_a_suspension(n: int):
    held = n
    yield held
    del held
    try:
        yield held
    except UnboundLocalError as error:
        yield f'UnboundLocalError: {error}'
",
        &[
            "m.only_deleted()",
            "m.shared(2)",
            "m.declared_nonlocal(4)",
            "list(m.across_a_suspension(3))",
        ],
    );
}

#[test]
fn a_global_a_nested_frame_declares_is_not_the_enclosing_local_of_that_name() {
    // the enclosing frame binds a local `seen` and the nested one declares `seen`
    // global, so they are two different places. deciding captures without consulting
    // the declaration makes the closure read and write the enclosing local instead,
    // and both the module's name and the local then answer wrongly
    agree_python(
        "globalnested",
        "\
seen = 0
tally = 0


def outer(n: int) -> str:
    seen = n

    def inner() -> int:
        global seen
        seen = 5
        return seen

    return f'{inner()} {seen}'


def only_reads(n: int) -> str:
    # the nested frame declares the name and never writes it, so nothing about *it*
    # says the enclosing local is the wrong place — only the declaration does
    seen = n

    def peek() -> int:
        global seen
        return seen

    return f'{peek()} {seen}'


def declared_out_here(n: int) -> str:
    # and the mirror: the enclosing frame declares it and writes it, so it has no
    # register for the nested frame to capture even though the name looks local
    global tally
    tally = n

    def peek() -> int:
        return tally

    return f'{peek()} {tally}'


def shadow(n: int) -> int:
    # no declaration: an ordinary local that shadows the module's name
    seen = n
    return seen
",
        &[
            "(m.outer(3), m.seen)",
            "(m.only_reads(9), m.seen)",
            "(m.declared_out_here(4), m.tally)",
            "(m.shadow(9), m.seen)",
        ],
    );
}

#[test]
fn a_global_a_generator_assigns_is_not_one_of_its_state_fields() {
    // a generator's locals become fields of the state object, because the frame has to
    // survive a suspension. a declared `global` is not one of them — the module
    // namespace already outlives every suspension — so it must be kept out of that
    // layout as much as out of a register, and each resumption has to write through
    agree_python(
        "globalgen",
        "\
total = 0
steps: list[int] = []


def counting(n: int):
    global total
    for i in range(n):
        total = total + i
        # the write has to be visible across the suspension, from outside and back
        yield total


def resets():
    global total
    total = 0
    yield total
    total = 100
    yield total
",
        &[
            "(list(m.counting(4)), m.total)",
            "(list(m.counting(3)), m.total)",
            // stepped by hand, reading the module's name between resumptions
            "[(next(g), m.total) for g in [m.counting(5)] for _ in range(3)]",
            "(list(m.resets()), m.total)",
        ],
    );
}

#[test]
fn a_module_level_name_a_global_rebinds_is_found_through_the_namespace() {
    // taking the write opened this: a frame can now rebind a name the module *defined*,
    // and a call written against that name was reaching the definition directly. so the
    // rebound function went on answering with the old body — the same thing a decorator
    // does to a name, and it goes in the same set
    agree_python(
        "globalrebind",
        "\
def base() -> int:
    return 1


def other() -> int:
    return 99


def calls_base() -> int:
    return base()


def rebind() -> str:
    global base
    base = other
    return f'{base()} {calls_base()}'


def replaces_itself() -> int:
    # `pydoc.pager` is this exactly: decide once what to be, rebind the name, then
    # call *through the name*. reaching the native entry for that last call re-enters
    # the body that just rebound it, and the stack runs out
    global replaces_itself
    replaces_itself = other
    return replaces_itself()
",
        &[
            "(m.calls_base(), m.base())",
            "m.rebind()",
            "(m.calls_base(), m.base())",
            "m.replaces_itself()",
            "(m.replaces_itself(), m.replaces_itself())",
        ],
    );
}

#[test]
fn a_compiled_frame_is_what_reaches_the_module_namespace() {
    // the differential tests above compare two legs, and a leg that fell back to its
    // interpreted definition answers exactly as the interpreted leg does — so they
    // cannot say *which* build wrote the global. this one can: a module-level function
    // python calls through `PyModule_AddFunctions` is a `builtin_function_or_method`,
    // and one that fell back is a `function`
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_globalidentity");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
inited = False


class C:
    def __init__(self) -> None:
        if not inited:
            init()
        self.x = 1


def init() -> None:
    global inited
    inited = True
    C()
";
    let built = match build_source(
        source,
        "by_diff_globalidentity",
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
    assert!(built.declined.is_empty(), "{:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_globalidentity as m\n\
         print(type(m.init).__name__, m.C().x, m.inited)\n",
    );
    // and `m.inited` read from out here is the module's own binding, which a register
    // write never touched. before there was an op for it, `C()` inside `init` saw the
    // old `False` and called `init` again until the stack ran out
    assert_eq!(out, "builtin_function_or_method 1 True");
}

#[test]
fn a_declined_function_reads_the_global_a_compiled_one_wrote() {
    // the asymmetry the whole thing turns on. a compiled frame and the interpreted
    // twin of a *declined* one are the same module, and the twin's `__globals__` is
    // the dict the compiled frame binds into — so a write is visible to it at once.
    // a register write is visible to nobody, which is why this had to decline
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_globaltwin");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
flag = 0


def writes(n: int) -> int:
    global flag
    flag = n
    return flag


def declines_and_reads() -> str:
    # `except*` has no lowering, so this whole function stays interpreted. any gate
    # can be lifted — the `del tmp` that used to sit here was — so what keeps this
    # honest is the assertion below rather than the choice of construct. `except*` is
    # picked because it has nothing to do with name binding, which is where the work
    # that moves this test tends to happen
    try:
        pass
    except* ValueError:
        pass
    return f'{flag}'
";
    let built = match build_source(
        source,
        "by_diff_globaltwin",
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
    // the two legs of the same module: one compiled, one not. if the construct above
    // ever gains a lowering this stops being true, and the assertion says so rather
    // than quietly testing two compiled functions
    assert_eq!(
        built
            .declined
            .iter()
            .map(|declined| declined.name.as_str())
            .collect::<Vec<_>>(),
        vec!["declines_and_reads"],
        "declined: {:?}",
        built.declined
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_globaltwin as m\n\
         print(type(m.writes).__name__, type(m.declines_and_reads).__name__,\n\
         \x20     m.declines_and_reads(), m.writes(42), m.declines_and_reads(), m.flag)\n",
    );
    assert_eq!(out, "builtin_function_or_method function 0 42 42 42");
}

#[test]
fn a_second_decorator_over_a_static_method_declines() {
    // the runtime folds the rest of a method's decorators onto the attribute it reads
    // back off the finished type — and reading a static method back hands over the
    // plain function it wraps, which would be written back as an ordinary method. so
    // the pair keeps the decline, and the interpreted definition is what answers
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_staticmethod_stacked");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def mark(fn):
    fn.marked = True
    return fn


class Stacked:
    @mark
    @staticmethod
    def both() -> int:
        return 1
";
    let built = match build_source(
        source,
        "by_diff_staticmethod_stacked",
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
        built
            .declined
            .iter()
            .any(|declined| declined.reason.contains("a second decorator over")),
        "declined: {:?}",
        built.declined
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_staticmethod_stacked as m\n\
         print(m.Stacked.both(), type(m.Stacked.__dict__['both']).__name__)\n",
    );
    assert_eq!(out, "1 staticmethod");
}

#[test]
fn string_literals_do_not_leak() {
    // `gc.get_objects()` cannot see this: `str` is not GC-tracked, so a leaked
    // literal is invisible to an object-count check. the refcount of the returned
    // literal is the measurement that works
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_strleak");
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
    let dir = diff_root().join("by_diff_strhold");
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
    let dir = diff_root().join("by_diff_ctorcheck");
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
    let dir = diff_root().join("by_diff_layout");
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
    // a declared field is the *layout*: a descriptor on the type, read at an offset,
    // never an entry in an instance dict. and `__dict__` itself is refused however the
    // instance is built — a mapping naming only what the layout has no room for would
    // be an empty answer where the interpreted class gives a full one, which is quiet
    // and wrong where the refusal is at least loud
    let out = run(
        &python,
        &dir,
        "import by_diff_layout as m\n\
         p = m.Point(1, 2)\n\
         print(type(vars(m.Point)['x']).__name__)\n\
         print(hasattr(p, '__dict__'))\n",
    );
    assert_eq!(out, "getset_descriptor\nFalse");
}

/// the source both of the instance-dict tests build
///
/// `bump` is here so that the compiled type can be told from the interpreted one: a class
/// that fell back answers `function` where an emitted one answers `method_descriptor`,
/// and without that a test about what an instance accepts passes just as well with the
/// class never compiled at all
const A_CLASS_WITH_A_FIELD: &str = "\
class Counter:
    def __init__(self):
        self.count = 0

    def bump(self):
        self.count = self.count + 1
        return self.count
";

#[test]
fn a_class_takes_an_attribute_its_layout_never_had() {
    // python gives an instance somewhere to put a name its class never mentioned, and an
    // emitted class *is* its layout — so `o.brand_new = 7`, which the interpreted twin
    // stores, raised in the middle of a working program. the declared field stays in the
    // layout: only what the layout has no room for reaches the dict
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_instdict");
    let _ = std::fs::remove_dir_all(&dir);
    let options = Options {
        language: by_irbuild::Language::Python,
        ..Options::default()
    };
    if build_source(
        A_CLASS_WITH_A_FIELD,
        "by_diff_instdict",
        &toolchain,
        &dir,
        &options,
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import by_diff_instdict as m\n\
         print(type(m.Counter.bump).__name__)\n\
         o = m.Counter()\n\
         o.brand_new = 7\n\
         print(o.brand_new, o.bump(), type(vars(m.Counter)['count']).__name__)\n",
    );
    assert_eq!(out, "method_descriptor\n7 1 getset_descriptor");
    agree_python(
        "instdict2",
        A_CLASS_WITH_A_FIELD,
        &[
            "(lambda o: (setattr(o, 'brand_new', 7), o.brand_new, o.bump()))(m.Counter())",
            "(lambda o: (setattr(o, 'count', 5), o.bump()))(m.Counter())",
        ],
    );
}

#[test]
fn a_class_declaring_slots_keeps_the_bare_layout() {
    // `__slots__` is python's own way of saying an instance's attributes are exactly the
    // declared ones, so a class writing it is asking for the layout an emitted class has
    // anyway — and giving it a dict would be the opposite divergence: accepting what the
    // interpreted twin refuses
    agree_python(
        "slotsnodict",
        "\
class Counted:
    __slots__ = ('count',)

    def __init__(self):
        self.count = 0

    def bump(self):
        self.count = self.count + 1
        return self.count


def took(obj, name):
    try:
        setattr(obj, name, 7)
    except AttributeError:
        return 'AttributeError'
    return getattr(obj, name)
",
        &[
            "m.took(m.Counted(), 'brand_new')",
            "m.took(m.Counted(), 'count')",
            "m.Counted().bump()",
        ],
    );
}

#[test]
fn a_slotted_class_over_a_base_that_declares_none_still_takes_a_dict() {
    // python asks the whole chain: a `__slots__` on one class does not take the dict its
    // base already gave the instance away again. so the declaration only settles the
    // question where every class the layout comes from carries it
    agree_python(
        "slotschain",
        "\
class Loose:
    def __init__(self):
        self.count = 0


class Tight(Loose):
    __slots__ = ()

    def bump(self):
        self.count = self.count + 1
        return self.count


def took(obj, name):
    try:
        setattr(obj, name, 7)
    except AttributeError:
        return 'AttributeError'
    return getattr(obj, name)
",
        &[
            "m.took(m.Tight(), 'brand_new')",
            "m.took(m.Loose(), 'brand_new')",
            "m.Tight().bump()",
        ],
    );
}

/// a class whose body binds a name its `__init__` also assigns, which is python's
/// commonest way of writing a field with a fallback
const A_FIELD_WITH_A_CLASS_LEVEL_VALUE: &str = "\
class Held:
    tag = 'none'

    def __init__(self, own):
        if own:
            self.tag = 'own'

    def drop(self):
        del self.tag

    def put(self, value):
        self.tag = value

    def read(self):
        return self.tag
";

#[test]
fn a_field_falls_back_to_the_value_its_class_body_bound() {
    // `tag = None` beside a `self.tag = tag` is two answers under one name, and the whole
    // of what makes it work is that python keeps them apart: the class's value answers
    // until the instance has one of its own, the instance's shadows it once it has, and a
    // `del` puts the instance back to answering the class's. an emitted instance has no
    // dict to hold the second answer in, so all three come out of the layout and the one
    // descriptor over it
    agree_python(
        "clsdefault",
        A_FIELD_WITH_A_CLASS_LEVEL_VALUE,
        &[
            "m.Held.tag",
            "(m.Held(False).tag, m.Held(True).tag)",
            "(hasattr(m.Held(False), 'tag'), getattr(m.Held(False), 'tag', 'absent'))",
            // a delete on an instance with nothing of its own is the class's value being
            // read through, not an attribute to remove
            "repr(_capture(m.Held(False).drop))",
            "(lambda h: (h.drop(), h.tag, repr(_capture(h.drop))))(m.Held(True))",
            // and the field comes back, still shadowing, after it has been away
            "(lambda h: (h.drop(), h.put('again'), h.tag, m.Held.tag))(m.Held(True))",
            "(lambda h: (h.put('set'), h.tag, h.drop(), h.tag))(m.Held(False))",
        ],
    );
}

#[test]
fn a_method_reading_a_defaulted_field_gets_the_class_level_value_too() {
    // a compiled method reads the layout straight, without the descriptor over it — so it
    // has to ask the same question the descriptor asks or the two disagree about the very
    // same attribute. `asyncio`'s `_LoopBoundMixin` is the shape: `self._loop` is read
    // before anything has written one, and `_loop = None` is the whole answer
    agree_python(
        "clsdefaultread",
        A_FIELD_WITH_A_CLASS_LEVEL_VALUE,
        &[
            "(m.Held(False).read(), m.Held(True).read())",
            "(m.Held(False).read(), m.Held(False).tag)",
            "(lambda h: (h.drop(), h.read()))(m.Held(True))",
            "(lambda h: (h.put('set'), h.read(), h.drop(), h.read()))(m.Held(False))",
        ],
    );
}

#[test]
fn a_field_its_constructor_always_assigns_still_keeps_a_class_level_value() {
    // `xml.etree.ElementTree.Element` is written this way, and it is the commonest shape
    // of the two: nothing reads the class's value on a constructed instance, so a layout
    // could have left out the byte that says whether the instance has one. it cannot —
    // the read off the class needs a value, and `del` puts an instance back to answering
    // it, which is a state such a field would otherwise have no way to be in
    agree_python(
        "clsdefaultalways",
        "\
class Node:
    tag = 'unset'

    def __init__(self, tag):
        self.tag = tag
",
        &[
            "m.Node.tag",
            "m.Node('own').tag",
            "(lambda h: (delattr(h, 'tag'), h.tag, repr(_capture(delattr, h, 'tag'))))(m.Node('own'))",
            "(lambda h: (delattr(h, 'tag'), setattr(h, 'tag', 'again'), h.tag))(m.Node('own'))",
            "(hasattr(m.Node('own'), 'tag'), m.Node.tag)",
        ],
    );
}

#[test]
fn a_class_level_value_of_another_type_than_the_field_holds_still_answers() {
    // `xml.etree.ElementTree.Element` writes `tag = None` over a field every assignment
    // gives a `str`, and `http.client.HTTPConnection` writes `debuglevel = 0` over one an
    // `int` fits. the layout is sized from what the *assignments* write, so a class-level
    // value of another type has nowhere to go — the field has to be widened to hold either,
    // exactly as it is for two methods that write it differently
    agree_python(
        "clsdefaultwiden",
        "\
class Node:
    tag = None
    level = 0

    def __init__(self, own):
        if own:
            self.tag = 'own'
            self.level = 3

    def read(self):
        return (self.tag, self.level)
",
        &[
            "(m.Node.tag, m.Node.level)",
            "(m.Node(False).tag, m.Node(False).level)",
            "(m.Node(True).tag, m.Node(True).level)",
            "(m.Node(False).read(), m.Node(True).read())",
            "(lambda h: (delattr(h, 'tag'), h.read()))(m.Node(True))",
        ],
    );
}

#[test]
fn the_class_level_value_a_field_falls_back_to_is_the_one_shared_object() {
    // python evaluates a class body once, so a mutable value it binds is a single object
    // every instance without one of its own is reading — mutating it through one shows up
    // in the class and in every other. the compiled class holds that same object rather
    // than a copy per instance, which is what keeps the sharing observable
    agree_python(
        "clsdefaultmut",
        "\
class Bag:
    items = []

    def __init__(self, own):
        if own:
            self.items = ['own']
",
        &[
            "(lambda a, b: (a.items is b.items, a.items is m.Bag.items))(m.Bag(False), m.Bag(False))",
            "(lambda a, b: (a.items.append(7), b.items, m.Bag.items))(m.Bag(False), m.Bag(False))",
            "(lambda a, b: (a.items.append(7), b.items))(m.Bag(False), m.Bag(True))",
        ],
    );
}

#[test]
fn a_subclass_reads_the_class_level_value_of_whichever_class_bound_it() {
    // the value belongs to the class whose body wrote it, so a subclass that binds nothing
    // reads its base's and one that binds its own reads that instead — while an instance
    // of either that assigns still shadows both
    agree_python(
        "clsdefaultsub",
        "\
class Base:
    tag = 'base'

    def __init__(self, own):
        if own:
            self.tag = 'own'


class Rebound(Base):
    tag = 'rebound'


class Plain(Base):
    pass
",
        &[
            "(m.Base.tag, m.Rebound.tag, m.Plain.tag)",
            "(m.Base(False).tag, m.Rebound(False).tag, m.Plain(False).tag)",
            "(m.Base(True).tag, m.Rebound(True).tag, m.Plain(True).tag)",
            "(lambda h: (delattr(h, 'tag'), h.tag))(m.Rebound(True))",
            "repr(_capture(delattr, m.Plain(False), 'tag'))",
        ],
    );
}

#[test]
fn a_property_bound_beside_an_assignment_of_its_name_is_not_a_defaulted_field() {
    // `calendar.Calendar` is written this way, and it is the shape a field with a
    // class-level fallback is indistinguishable from without knowing what the value is.
    // it is not a field at all: `self.first = ...` runs the property's setter, which puts
    // the value somewhere else entirely, and the instance keeps nothing of its own. taking
    // it for a fallback would answer every read with the class's own value instead
    agree_python_with_declines(
        "clsdefaultprop",
        "\
class Cal:
    def _get(self):
        return self._held % 7

    def _set(self, value):
        self._held = value

    first = property(_get, _set)

    def __init__(self, first):
        self.first = first
",
        &[
            "m.Cal(3).first",
            "m.Cal(9).first",
            "(lambda c: (setattr(c, 'first', 8), c.first, c._held))(m.Cal(0))",
            "type(vars(m.Cal)['first']).__name__",
        ],
    );
}

#[test]
fn a_subclass_that_adds_storage_still_shares_its_base_s_class_level_value() {
    // a subclass adding a field of its own lays out and publishes the whole set, its
    // base's included — so it puts up its own descriptor for the inherited name while the
    // value stays where the base's body wrote it. one object, two descriptors reading it,
    // and only the base is allowed to fill it
    agree_python(
        "clsdefaultshared",
        "\
class Base:
    tag = 'base'

    def __init__(self, own):
        if own:
            self.tag = 'own'


class Wider(Base):
    def __init__(self, own):
        Base.__init__(self, own)
        self.extra = 'extra'
",
        &[
            "(m.Base.tag, m.Wider.tag)",
            "(m.Wider(False).tag, m.Wider(True).tag, m.Wider(False).extra)",
            "m.Base.tag is m.Wider.tag",
            "(lambda h: (delattr(h, 'tag'), h.tag, h.extra))(m.Wider(True))",
            "repr(_capture(delattr, m.Wider(False), 'tag'))",
        ],
    );
}

#[test]
fn a_class_level_value_over_a_field_a_base_left_no_room_to_be_absent_in_declines() {
    // the base's constructor assigns on every path, so its layout carries no byte saying
    // whether an instance has a value — and the subclass cannot add one to a struct the
    // base already settled. the answer is the interpreted definition, which still gets
    // both readings right
    agree_python_with_declines(
        "clsdefaultnoroom",
        "\
class Base:
    def __init__(self, tag):
        self.tag = tag


class Rebound(Base):
    tag = 'rebound'
",
        &[
            "m.Rebound.tag",
            "(m.Base('own').tag, m.Rebound('own').tag)",
            "(lambda h: (delattr(h, 'tag'), h.tag))(m.Rebound('own'))",
        ],
    );
}

#[test]
fn a_defaulted_field_in_storage_appended_past_an_outside_base_still_answers() {
    // a class extending something this module does not write keeps its own fields in a
    // region *past* the base's instance, so the struct holding them does not start where
    // the object does and its members are at no fixed offset from it. reading the presence
    // byte as an offset from the instance read the exception's own fields instead, and
    // every freshly built instance came back as having a value of its own — the one thing
    // the class-level value exists to answer instead
    agree_python(
        "clsdefaultappend",
        "\
class Failed(Exception):
    tag = 'base'

    def __init__(self, own):
        if own:
            self.tag = 'own'


class Rebound(Failed):
    tag = 'rebound'
",
        &[
            "(m.Failed.tag, m.Rebound.tag)",
            "(m.Failed(False).tag, m.Failed(True).tag)",
            "(m.Rebound(False).tag, m.Rebound(True).tag)",
            "(lambda h: (delattr(h, 'tag'), h.tag))(m.Failed(True))",
            "repr(_capture(delattr, m.Failed(False), 'tag'))",
        ],
    );
}

#[test]
fn a_defaulted_field_answers_from_under_a_slotted_base() {
    // a class keeping a dict keeps it in a word at the *front* of its struct, so every
    // field after it sits eight bytes further along — and the word is reserved for a
    // whole chain wherever any rung of it keeps one, because a base and a subclass
    // cannot disagree about where the fields start. `Held` declares `__slots__` and so
    // wants no dict of its own; `Tagged` under it wants one, which puts the reserved
    // word into `Held`'s struct as well and moves `Tagged`'s presence byte twice over.
    //
    // the byte is reached by a predicate emitted beside the getter rather than by an
    // offset carried on the descriptor, which is what makes both shapes the same
    // question. a defaulted field cannot itself be slotted — python refuses a `__slots__`
    // entry that a class variable also names — so a slotted base is the only way the two
    // layouts meet
    agree_python(
        "clsdefaultslotted",
        "\
class Held:
    __slots__ = ('n',)

    def __init__(self):
        self.n = 1


class Tagged(Held):
    tag = 'base'

    def __init__(self, own):
        Held.__init__(self)
        if own:
            self.tag = 'own'
",
        &[
            "m.Tagged.tag",
            "(m.Tagged(False).tag, m.Tagged(True).tag)",
            "(m.Tagged(False).n, m.Tagged(True).n)",
            "(lambda h: (delattr(h, 'tag'), h.tag, h.n))(m.Tagged(True))",
            "repr(_capture(delattr, m.Tagged(False), 'tag'))",
            // the dict the subclass keeps is what takes a name neither class declares,
            // and it must not be the place the defaulted field answered from
            "(lambda h: (setattr(h, 'extra', 9), h.extra, h.tag))(m.Tagged(False))",
        ],
    );
}

#[test]
fn a_defaulted_field_is_answered_by_a_descriptor_of_ours() {
    // a class that fell back to its interpreted definition answers every one of the
    // comparisons above identically, so nothing they say proves the compiled leg was the
    // one asked. the entry in the type's dict is what does: a plain field's is python's
    // own `getset_descriptor`, and a defaulted one's has to be ours or the read off the
    // class would be answering with the descriptor rather than the value
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_clsdefaultdesc");
    let _ = std::fs::remove_dir_all(&dir);
    let options = Options {
        language: by_irbuild::Language::Python,
        ..Options::default()
    };
    if build_source(
        A_FIELD_WITH_A_CLASS_LEVEL_VALUE,
        "by_diff_clsdefaultdesc",
        &toolchain,
        &dir,
        &options,
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import by_diff_clsdefaultdesc as m\n\
         print(type(vars(m.Held)['tag']).__name__, type(m.Held.drop).__name__)\n",
    );
    assert_eq!(out, "field_default method_descriptor");
}

#[test]
fn a_class_over_a_slotted_base_takes_a_dict_the_base_does_not() {
    // the other direction of the same rule, and the one the *layout* has to be arranged
    // for: `Tight` keeps the bare layout and `Loose` under it keeps a dict, so the two
    // disagree about whether the word holding one is there. they cannot disagree about
    // where `kept` sits — `reach` is handed a `Tight` and reads that field out of a
    // `Loose` — so the word is reserved on both and only `Loose` names it to its type
    agree_python(
        "slotsbase",
        "\
class Tight:
    __slots__ = ('kept',)

    def __init__(self):
        self.kept = 1

    def read(self):
        return self.kept


class Loose(Tight):
    def bump(self):
        self.kept = self.kept + 1
        return self.kept


def reach(t: Tight) -> object:
    return t.read()


def took(obj, name):
    try:
        setattr(obj, name, 7)
    except AttributeError:
        return 'AttributeError'
    return getattr(obj, name)
",
        &[
            "[m.reach(m.Tight()), m.reach(m.Loose())]",
            "m.took(m.Tight(), 'brand_new')",
            "m.took(m.Loose(), 'brand_new')",
            "m.Loose().bump()",
            // the field the base declared, read through a base-typed name out of an
            // instance of the subclass — which is where a word the two disagreed about
            // would move the offset
            "(lambda o: [setattr(o, 'brand_new', 3), m.reach(o), o.bump(), o.brand_new])(m.Loose())",
            // and the shadow the direct call has to keep asking about, on the subclass
            // whose instances have somewhere to hold one
            "(lambda o: [m.reach(o), setattr(o, 'read', lambda: 99), m.reach(o)])(m.Loose())",
        ],
    );
}

#[test]
fn a_native_class_instance_does_not_leak() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_classleak");
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
    let dir = diff_root().join("by_diff_floatimport");
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
    let dir = diff_root().join("by_diff_cfunc");
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
    let dir = diff_root().join("by_diff_noany");
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
    let dir = diff_root().join("by_diff_reqnative");
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
    let dir = diff_root().join("by_diff_noany_ok");
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
    let dir: PathBuf = diff_root().join("by_diff_declined");
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
    let dir = diff_root().join("by_diff_setcheck");
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
    let dir = diff_root().join("by_diff_argcheck");
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
    let dir = diff_root().join("by_diff_fieldleak");
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
fn a_literal_used_in_a_loop_neither_leaks_nor_is_released_twice() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_constborrow");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def kept(line: str) -> int:
    total = 0
    for part in line.split(\" \"):
        if part.startswith(\"w\"):
            total = total + 1
    return total

def marked(line: str) -> str:
    return \"-\".join(line.split(\" \"))
";
    if build_source(
        source,
        "by_diff_constborrow",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    // the literals are module statics the emitter builds once, so a register that
    // only ever holds one borrows it. what that has to not do is give back a
    // reference it never took: a release per trip would have taken the count to
    // zero long before this loop ended, and a retain per trip would show as a leak.
    // a one-character latin-1 string is a singleton the interpreter shares, so
    // `"w"` here *is* the module's own
    let out = run(
        &python,
        &dir,
        "import sys, by_diff_constborrow as m\n\
         line = 'word zero ' * 50\n\
         wanted = line.replace(' ', '-')\n\
         before = (sys.getrefcount(' '), sys.getrefcount('w'), sys.getrefcount('-'))\n\
         answers = set()\n\
         for _ in range(20000):\n\
        \x20   answers.add((m.kept(line), m.marked(line) == wanted))\n\
         after = (sys.getrefcount(' '), sys.getrefcount('w'), sys.getrefcount('-'))\n\
         print(sorted(answers), 'stable' if after == before else f'moved {before}->{after}')\n",
    );
    assert_eq!(out, "[(50, True)] stable");
}

/// a narrowing check whose source is a temporary, and a chain of copies off one
///
/// `key = keys[i]` narrows the subscript's result to a `str`, and that narrowing is a
/// type test and a retain of the very object it was given — so it borrows, resting on
/// the temporary the subscript filled. `table[key]` three times a trip then copies the
/// key three more times, and each of those rests on the same temporary rather than on
/// the key: cutting the chain at the key instead would turn three free copies into
/// three retained ones
const BORROWED_NARROWINGS: &str = "\
def picked(holder: list[str], passes: int) -> int:
    total = 0
    i = 0
    while i < passes:
        got = holder[0]
        total = total + len(got)
        i = i + 1
    return total

def counted(table: dict[str, int], keys: list[str], passes: int) -> int:
    running = 0
    i = 0
    while i < passes:
        key = keys[0]
        running = running + table[key] + table[key] + table[key]
        i = i + 1
    return running
";

#[test]
fn a_borrowed_narrowing_does_not_over_release_what_it_narrowed() {
    // the narrowing no longer retains, so the register is reading through something it
    // does not own. the subscript's temporary holds the only reference the borrow
    // rests on, and it is released every trip — so a window that is wrong by one
    // operation frees the string out of the list, which is fatal rather than merely
    // wrong. a stray release shows first as a falling reference count
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_narrowborrow");
    let _ = std::fs::remove_dir_all(&dir);
    if build_source(
        BORROWED_NARROWINGS,
        "by_diff_narrowborrow",
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
        "import sys, by_diff_narrowborrow as m\n\
         held = 'abcdefghij' * 4\n\
         holder = [held]\n\
         key = 'k' * 12\n\
         table = {key: 7}\n\
         before = (sys.getrefcount(held), sys.getrefcount(key))\n\
         answers = set()\n\
         for _ in range(2000):\n\
        \x20   answers.add((m.picked(holder, 5), m.counted(table, [key], 5)))\n\
         after = (sys.getrefcount(held), sys.getrefcount(key))\n\
         print(sorted(answers), 'stable' if after == before else f'moved {before}->{after}')\n",
    );
    assert_eq!(out, "[(200, 105)] stable");
}

#[test]
fn a_constructor_result_is_used_natively() {
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

/// what a caller can do to an emitted class between two calls on it, and still be
/// answered the way the interpreter would answer
///
/// every compiled call on a method here goes through a remembered answer of some kind —
/// a licence for a method the class declares, a call-site memo for one it inherits — and
/// each is a memo of an *attribute lookup*, so anything the caller can do to change what
/// that lookup finds has to be caught. `Tally` has a subclass, so its instances carry a
/// dict of their own: writing a value into one is the route that reaches neither the
/// class nor its version tag
#[test]
fn a_remembered_method_notices_the_class_being_changed_under_it() {
    agree_python(
        "methodstale",
        "\
class Counter:
    def __init__(self, n: int) -> None:
        self.n = n

    def step(self, d: int) -> int:
        return self.n + d


class Tally(Counter):
    def double(self) -> int:
        return self.n + self.n


def declared(t: Tally) -> object:
    return t.double()


def inherited(t: Tally) -> object:
    return t.step(1)
",
        &[
            "[m.declared(m.Tally(5)), m.inherited(m.Tally(5))]",
            // a value on the instance shadows the type's entry, for the method the
            // class declares and for the one it inherits alike
            "\
(lambda t: [
    m.declared(t),
    setattr(t, 'double', lambda: 111),
    m.declared(t),
])(m.Tally(5))",
            "\
(lambda t: [
    m.inherited(t),
    setattr(t, 'step', lambda d: 222),
    m.inherited(t),
])(m.Tally(5))",
            // the same shadow written past the attribute machinery, which is the route
            // a `tp_setattro` of our own would not have seen
            "\
(lambda t: [
    m.declared(t),
    object.__setattr__(t, 'double', lambda: 333),
    m.declared(t),
])(m.Tally(5))",
            // an instance written to under some *other* name still answers from the type
            "\
(lambda t: [
    setattr(t, 'unrelated', 1),
    m.declared(t),
    m.inherited(t),
])(m.Tally(5))",
            // rebound on the class, after a call that has already settled the answer
            "\
(lambda t: [
    m.declared(t),
    setattr(m.Tally, 'double', lambda self: 444),
    m.declared(t),
])(m.Tally(5))",
            // rebound on the *base*, which the class inherits the method from
            "\
(lambda t: [
    m.inherited(t),
    setattr(m.Counter, 'step', lambda self, d: 555),
    m.inherited(t),
])(m.Tally(5))",
            // a subclass written in the interpreter overrides both
            "\
(lambda S: [m.declared(S(5)), m.inherited(S(5))])(
    type('S', (m.Tally,), {
        'double': lambda self: 666,
        'step': lambda self, d: 777,
    }))",
        ],
    );
}

/// the source the shadowed-method tests build
///
/// `Alone` has no base and no subclass, which is the shape a call site names one body
/// for outright — no test, no lookup, nothing to invalidate. `Slotted` is the same class
/// with python's own declaration that its instances hold exactly the declared
/// attributes, which is what leaves the direct call with nothing to ask
const A_CLASS_NOTHING_DERIVES_FROM: &str = "\
class Alone:
    def __init__(self, n):
        self.n = n

    def double(self):
        return self.n + self.n

    def twice_over(self):
        return self.double() + self.double()


class Slotted:
    __slots__ = ('n',)

    def __init__(self, n):
        self.n = n

    def double(self):
        return self.n + self.n


def alone(a):
    return a.double()


def nested(a):
    return a.twice_over()


def slotted(s):
    return s.double()
";

/// a value written on the instance of a class nothing derives from
///
/// a method is a non-data descriptor, so `a.double = f` stored in the instance's own dict
/// wins over the class's entry. this class is the one shape a call site does not have to
/// test anything for — nothing can subclass it and nothing can rebind a method on it —
/// and the instance is the one route left. every call below reaches the same compiled
/// body, so a guard that stops asking shows up on all of them at once
#[test]
fn a_value_on_the_instance_shadows_the_method_of_a_class_nothing_derives_from() {
    agree_python(
        "aloneshadow",
        A_CLASS_NOTHING_DERIVES_FROM,
        &[
            "[m.alone(m.Alone(5)), m.nested(m.Alone(5)), m.slotted(m.Slotted(5))]",
            // the shadow, and the same call answered before and after it
            "\
(lambda a: [
    m.alone(a),
    setattr(a, 'double', lambda: 222),
    m.alone(a),
])(m.Alone(5))",
            // written past the attribute machinery, which is the route a `tp_setattro`
            // of our own would not have seen — and the reason a per-class flag saying
            // no instance has been written to cannot be kept honest
            "\
(lambda a: [
    m.alone(a),
    object.__setattr__(a, 'double', lambda: 333),
    m.alone(a),
])(m.Alone(5))",
            // taken away again, and the class's own body answers once more
            "\
(lambda a: [
    setattr(a, 'double', lambda: 444),
    m.alone(a),
    delattr(a, 'double'),
    m.alone(a),
])(m.Alone(5))",
            // some *other* name written on the instance leaves the method alone
            "\
(lambda a: [
    setattr(a, 'unrelated', 1),
    m.alone(a),
])(m.Alone(5))",
            // the shadow reached from inside another compiled method, where the receiver
            // is `self` rather than an argument
            "\
(lambda a: [
    m.nested(a),
    setattr(a, 'double', lambda: 555),
    m.nested(a),
])(m.Alone(5))",
            // an instance with no dict has nowhere to put one, and python says so
            "\
(lambda s: [
    m.slotted(s),
    type(_capture(setattr, s, 'double', lambda: 666)).__name__,
    m.slotted(s),
])(m.Slotted(5))",
        ],
    );
}

/// a `final` receiver, which says no subclass exists and nothing else
///
/// the class still stands in an inheritance chain, so it is emitted as a mutable heap
/// type: `Fixed.tripled = f` rebinds the method and a value written on an instance
/// shadows it. `final` rules out neither, and a call site that read it as licensing a
/// direct call answered from the compiled body through both
#[test]
fn a_final_receiver_notices_the_class_being_changed_under_it() {
    agree_python(
        "finalstale",
        "\
from typing import final


class Open:
    def __init__(self, n: int) -> None:
        self.n = n

    def doubled(self) -> int:
        return self.n * 2


@final
class Fixed(Open):
    def tripled(self) -> int:
        return self.n * 3


def on_final(f: Fixed) -> object:
    return f.tripled()
",
        &[
            "m.on_final(m.Fixed(5))",
            "\
(lambda f: [
    m.on_final(f),
    setattr(f, 'tripled', lambda: 111),
    m.on_final(f),
])(m.Fixed(5))",
            "\
(lambda f: [
    m.on_final(f),
    object.__setattr__(f, 'tripled', lambda: 222),
    m.on_final(f),
])(m.Fixed(5))",
            "\
(lambda f: [
    m.on_final(f),
    setattr(m.Fixed, 'tripled', lambda self: 333),
    m.on_final(f),
])(m.Fixed(5))",
        ],
    );
}

/// that the calls above were answered by compiled bodies rather than by the fallback
///
/// a class left to its interpreted definition answers every one of them the way python
/// does, so the test above passes just as well with nothing compiled at all. an emitted
/// method is a `method_descriptor` where the interpreted one is a `function`
#[test]
fn the_shadowed_calls_are_answered_by_compiled_bodies() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_aloneshadowkind");
    let _ = std::fs::remove_dir_all(&dir);
    if build_source(
        A_CLASS_NOTHING_DERIVES_FROM,
        "by_diff_aloneshadowkind",
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
        "import by_diff_aloneshadowkind as m\n\
         print(type(m.Alone.double).__name__)\n\
         print(type(m.Slotted.double).__name__)\n\
         print(type(m.alone).__name__)\n",
    );
    assert_eq!(
        out,
        "method_descriptor\nmethod_descriptor\nbuiltin_function_or_method"
    );
}

/// a builtin whose entry point wants the defining class, which no call site can supply
///
/// `re.Pattern.search` is `METH_FASTCALL | METH_KEYWORDS | METH_METHOD`, and the extra
/// flag moves the arguments along one: the array would land where the class belongs.
/// only a heap type can carry it, so it is the convention a call-site memo meets as
/// soon as it is willing to answer for one
#[test]
fn a_method_wanting_its_defining_class_is_not_called_as_a_plain_one() {
    agree_python(
        "methmethod",
        "\
import re


def first(pattern: re.Pattern[str], text: str) -> object:
    found = pattern.search(text)
    if found is None:
        return None
    return found.group(0)


def each(pattern: re.Pattern[str], texts: list[str]) -> list[object]:
    out: list[object] = []
    for text in texts:
        out.append(first(pattern, text))
    return out
",
        &[
            "m.each(__import__('re').compile('a+'), ['baaad', 'none', 'aa', ''])",
            // the same site over and over, which is what settles a memo on the answer
            "m.each(__import__('re').compile('[0-9]+'), [str(n) for n in range(20)])",
        ],
    );
}

#[test]
fn a_direct_method_call_does_not_leak() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_methleak");
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
    let dir = diff_root().join("by_diff_borrow");
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
    let dir = diff_root().join("by_diff_finalizer");
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
fn a_nested_functions_computed_default_is_evaluated_once_where_the_def_stands() {
    // python evaluates a default once, where the `def` runs, and hands that one object
    // to every call that omits the parameter. a nested function has no interpreted twin
    // to keep the object for it, so the environment the closure carries keeps it.
    //
    // `accumulates` is the sharp one: the list has to be *the same list* across two
    // calls, which a default rebuilt per call would not be. `reads` is the sharp one in
    // the other direction — the value is taken where the `def` stands, so a later write
    // to the name it came from must not be visible through it
    agree_python(
        "nesteddefault",
        "\
def tally():
    def accumulates(v, seen=[]):
        seen.append(v)
        return seen
    return accumulates

def snapshot(n):
    held = n * 2
    def reads(extra, base=held):
        return base + extra
    held = 0
    return reads

def two_of_them(a, b):
    def joins(times, sep=b, offset=a + 1):
        return sep.join([str(offset)] * times)
    return joins

def by_keyword(n):
    def scaled(x, *, factor=n + 1):
        return x * factor
    return scaled
",
        &[
            // one list, appended to twice through two separate calls
            "[(f := m.tally(), f(1), f(2))[-1]]",
            "[(f := m.tally(), f(1), f(2), f(1))[-1]]",
            // and a second closure gets a list of its own
            "[m.tally()(9), m.tally()(9)]",
            // supplying the parameter leaves the default untouched
            "[(f := m.tally(), f(1, [7]), f(2))[-1]]",
            "m.snapshot(5)(1)",
            "m.snapshot(10 ** 20)(1)",
            "m.two_of_them(3, '-')(3)",
            "m.by_keyword(4)(5)",
            "m.by_keyword(4)(5, factor=2)",
        ],
    );
}

#[test]
fn a_def_in_a_loop_whose_default_is_computed_is_declined() {
    // a frame allocates its environment once, so two passes of the loop would put their
    // defaults in one field and the first closure would find the second's object. python
    // gives each `def` its own, so this is turned down rather than shared
    agree_python_with_declines(
        "loopdefault",
        "\
def each(xs):
    out = []
    for x in xs:
        def holds(v, base=x):
            return base + v
        out.append(holds)
    return out
",
        &["[f(1) for f in m.each([10, 20])]"],
    );
}

#[test]
fn a_class_that_takes_a_weak_reference_to_itself_is_declined() {
    // an emitted instance is its layout and a type spec adds no `__weakref__`, so
    // `ref(self)` raises `TypeError` where python hands back a reference. the class is
    // turned down and runs interpreted instead, which is what makes both legs agree.
    //
    // this is `_weakrefset.WeakSet`'s shape, snapshotting the reference into a nested
    // function's default — the two constructions are together because the gate is about
    // the weak reference and not about where it stands
    agree_python_with_declines(
        "weakself",
        "\
from weakref import ref


class Holder:
    def __init__(self):
        self.me = ref(self)

    def alive(self):
        return self.me() is self


class Snapshots:
    def __init__(self):
        def report(prefix, selfref=ref(self)):
            return prefix + type(selfref()).__name__
        self.report = report
",
        &["m.Holder().alive()", "m.Snapshots().report('is ')"],
    );
}

#[test]
fn a_basedpython_default_that_is_not_an_immediate_is_re_evaluated_at_each_call() {
    // basedpython has no mutable-default gotcha: `mutable_defaults` rewrites such a
    // default to a sentinel and a guard that rebuilds it in the body, so each call gets
    // its own list. snapshotting it at the `def` the way python does would be a wrong
    // answer here, so a nested function declines rather than taking python's lowering
    agree_with_declines(
        "bydefault",
        "\
def tally() -> ((int) -> list[int]):
    def accumulates(v: int, seen: list[int] = []) -> list[int]:
        seen.append(v)
        return seen
    return accumulates
",
        &[
            // a fresh list each call, which is what makes this different from python
            "[(f := m.tally(), f(1), f(2))[-1]]",
        ],
    );
}

#[test]
fn a_nested_function_that_reads_its_own_name_agrees() {
    // a nested function that calls itself reads its own name out of the frame around
    // it, and so does one that calls a sibling. that makes the name a *cell* rather
    // than a plain local, and the `def` has to bind the cell — the same one the
    // closure reads, or the call finds nothing there.
    //
    // this is the shape `_pylong` uses five times over: a divide-and-conquer `inner`
    // defined beside the values it recurs against
    agree(
        "recursivedef",
        "\
def descend(n: int) -> int:
    limit = 2
    def inner(x: int) -> int:
        if x <= limit:
            return x
        return inner(x - 1) + inner(x - 2)
    return inner(n)

def siblings(n: int) -> str:
    def even(x: int) -> str:
        if x == 0:
            return \"even\"
        return odd(x - 1)
    def odd(x: int) -> str:
        if x == 0:
            return \"odd\"
        return even(x - 1)
    return even(n)

def two_deep(base: int, n: int) -> int:
    def middle(x: int) -> int:
        def inner(y: int) -> int:
            if y <= 0:
                return base
            return inner(y - 1) + 1
        return inner(x)
    return middle(n)

def escapes(n: int) -> ((int) -> int):
    def inner(x: int) -> int:
        if x <= 0:
            return n
        return inner(x - 1)
    return inner
",
        &[
            "m.descend(8)",
            "m.descend(0)",
            "m.siblings(7)",
            "m.siblings(0)",
            "m.two_deep(0, 4)",
            "m.two_deep(10 ** 20, 4)",
            // the closure outlives the frame, so its own name has to have been read
            // out of a cell rather than a register that went with the frame
            "m.escapes(3)(5)",
        ],
    );
}

#[test]
fn a_nested_function_rebound_after_its_def_is_called_through_the_name() {
    // a closure the frame made itself is normally called at its native entry, because
    // the name is known to hold it. binding the name to something else takes that
    // licence away — the call has to read whatever the name holds now
    agree(
        "rebounddef",
        "\
def twice(f: (int) -> int) -> ((int) -> int):
    def wrapped(x: int) -> int:
        return f(f(x))
    return wrapped

def rewrapped(n: int) -> int:
    def step(x: int) -> int:
        return x + 1
    step = twice(step)
    return step(n)

def rewrapped_in_a_loop(n: int) -> list[int]:
    def step(x: int) -> int:
        return x + 1
    out = []
    for _ in range(3):
        out.append(step(n))
        step = twice(step)
    return out
",
        &[
            "m.rewrapped(0)",
            "m.rewrapped(10 ** 20)",
            // the first pass reads the plain `def`, every later one a wrapped it
            "m.rewrapped_in_a_loop(0)",
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
    let dir = diff_root().join("by_diff_closureleak");
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
    let dir = diff_root().join("by_diff_envhidden");
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
    let dir = diff_root().join("by_diff_handlerleak");
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
fn the_frame_that_owns_a_cell_reads_an_unset_one_as_its_own_local() {
    // the two sides of a cell raise *different* errors for the same unset field. the
    // frame that owns the name has an unwritten local of its own, which is
    // `UnboundLocalError`; a frame that closes over it has a free variable, which is
    // the plainer `NameError`. only the exact class tells them apart, so that is what
    // this asks for
    agree(
        "cellunsetowner",
        "\
def owner(a: int) -> str:
    def step() -> None:
        nonlocal held
        held = a
    if a < 0:
        held = 0
    try:
        return str(held)
    except NameError as e:
        return type(e).__name__

def closes_over(a: int) -> str:
    def read() -> str:
        try:
            return str(held)
        except NameError as e:
            return type(e).__name__
    def step() -> None:
        nonlocal held
        held = a
    out = read()
    held = 0
    return out
",
        &["m.owner(1)", "m.owner(-1)", "m.closes_over(1)"],
    );
}

#[test]
fn a_shared_cell_does_not_leak() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_cellleak");
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
    let dir = diff_root().join("by_diff_geniter");
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
    let dir = diff_root().join(tag);
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

def raising(label: str) -> object:
    yield label
    raise StopIteration(label)
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

/// converting a raised `StopIteration` releases both exceptions
///
/// pep 479 replaces the exception leaving the frame and hangs the original off the
/// replacement, so for the length of the conversion two exceptions, a traceback and
/// a type are all live at once and all counted by hand. that is exactly the shape a
/// missed release hides in — and the original is deliberately *kept*, as the new
/// exception's `__cause__`, so a leak here looks like the chaining working
#[test]
fn converting_a_raised_stop_iteration_releases_both_exceptions() {
    let Some((python, dir)) = leak_module("by_diff_genconvert") else {
        return;
    };
    let out = run(
        &python,
        &dir,
        &format!(
            "{LEAK_INSTRUMENTS}\
             import by_diff_genconvert as m\n\
             label = 'x' * 40\n\
             def convert():\n\
            \x20   g = m.raising(label)\n\
            \x20   next(g)\n\
            \x20   try:\n\
            \x20       next(g)\n\
            \x20   except RuntimeError:\n\
            \x20       pass\n\
             print('runtime errors', leaked(RuntimeError, convert))\n\
             print('stop iterations', leaked(StopIteration, convert))\n\
             before = sys.getrefcount(label)\n\
             for _ in range(20000):\n\
            \x20   convert()\n\
             gc.collect()\n\
             print('label', 'stable' if sys.getrefcount(label) == before else 'leaked')\n"
        ),
    );
    assert_eq!(out, "runtime errors 0\nstop iterations 0\nlabel stable");
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
    let dir = diff_root().join("by_diff_reraiseleak");
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

/// `await f(...)` of a coroutine of ours that never suspends is compiled to the call
///
/// the object such an await would build is made by the expression, awaited once and
/// dropped, so nothing can observe that it was never there. everything that *could*
/// observe one is here: the coroutine reached any other way, the exception a body
/// raises on the way out, and pep 479's replacement of a `StopIteration` — which the
/// frame used to perform as it left and which the call now performs instead
#[test]
fn an_await_of_a_coroutine_that_never_suspends_agrees() {
    agree(
        "directawait",
        "\
async def doubled(n: int) -> int:
    return n * 2

async def nothing(n: int) -> None:
    return

async def defaulted(n: int, step: int = 3) -> int:
    return n + step

async def failing(n: int) -> int:
    return 10 // n

async def ending(kind: object) -> int:
    raise kind('boom')

async def chained(n: int) -> int:
    total = 0
    i = 0
    while i < n:
        total = total + await doubled(i)
        i = i + 1
    return total

async def with_none(n: int) -> object:
    got = await nothing(n)
    return (got, await doubled(n))

async def keyworded(n: int) -> int:
    return await defaulted(n, step=10) + await defaulted(n)

async def divided(n: int) -> int:
    return await failing(n)

async def guarding_stop(kind: object) -> object:
    try:
        return await ending(kind)
    except StopIteration as e:
        return 'stop ' + str(e)
    except RuntimeError as e:
        return 'runtime ' + str(e)
    except BaseException as e:
        return type(e).__name__ + ' ' + str(e)

async def unpacked(args: object) -> int:
    return await doubled(*args)

async def shadowing(doubled: object, n: int) -> object:
    return await doubled(n)

async def streaming(n: int) -> object:
    i = 0
    while i < n:
        yield await doubled(i)
        i = i + 1

async def awaited_by_name(n: int) -> int:
    held = doubled(n)
    return await held

async def awaited_as_value(x: object) -> object:
    return await x
",
        &[
            "_run(m.chained(6))",
            "_run(m.with_none(4))",
            "_run(m.keyworded(2))",
            "_run(m.divided(5))",
            "_run(m.unpacked((7,)))",
            "_run(m.shadowing(m.doubled, 9))",
            "_run(_drain(m.streaming(4)))",
            // the awaited body's own failure is the awaiting frame's failure, with the
            // same exception and the same words
            "_capture_async(m.divided, 0)",
            "_run(m.guarding_stop(ValueError))",
            "_run(m.guarding_stop(KeyError))",
            // pep 479: a coroutine raising `StopIteration` is forging the exhaustion
            // the await protocol reports with one, so python replaces it. an
            // `except StopIteration` around the await must go on not catching it
            "_run(m.guarding_stop(StopIteration))",
            "_capture_async(m.ending, StopIteration)",
            "_chain(_capture(_run, m.ending(StopIteration)))",
            "_chain(_capture(_run, m.ending(ValueError)))",
            // and the coroutine reached any other way is the object it always was:
            // awaited out of a name, awaited as somebody else's argument, driven by
            // hand, run by `asyncio` on its own, and asked what it is
            "_run(m.doubled(21))",
            "_run(m.awaited_by_name(8))",
            "_run(m.awaited_as_value(m.doubled(4)))",
            "_sent_once(m.doubled(6))",
            "_is_coroutine(m.doubled(1))",
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

/// what a resumable frame's `return` is worth, read through both faces of it
///
/// a compiled frame reports finishing by writing the value into its state object and
/// handing back nothing, because the slot python prefers to ask with — `am_send` —
/// can then say what the frame returned without an exception ever being built. the
/// iterator protocol still owes python a `StopIteration`, so the other face builds
/// one, and the two have to agree about every shape a return can take.
///
/// the shapes that break a careless implementation are the ones a `StopIteration`
/// would read as an *argument list*: a tuple spreads across it, an empty one leaves
/// nothing, a one-tuple collapses, and an exception instance is raised in place of
/// the error asked for. a subclass carrying its own `value` is the other half, since
/// what a delegation collects is the field rather than the attribute
#[test]
fn a_return_agrees_between_the_raise_and_the_send_slot() {
    agree(
        "sendslot",
        "\
def returning(v: object) -> object:
    yield 1
    return v

def straight(v: object) -> object:
    return v
    yield

def relayed(v: object) -> object:
    got = yield from returning(v)
    yield got

async def finishing(v: object) -> object:
    return v

async def relaying(v: object) -> object:
    got = await finishing(v)
    return got

def counting(n: int) -> object:
    i = 0
    while i < n:
        yield i
        i = i + 1

def silent() -> object:
    yield 1
    return

def relayed_silent(again: int) -> object:
    inner = silent()
    got = yield from inner
    yield got
    i = 0
    while i < again:
        yield (yield from inner)
        i = i + 1

async def nothing() -> None:
    return

async def relaying_nothing() -> object:
    got = await nothing()
    return got

def echoing(n: int) -> object:
    i = 0
    while i < n:
        got = yield i
        yield got
        i = i + 1
    return 'end'

def relaying_sends(n: int) -> object:
    got = yield from echoing(n)
    yield got

def failing() -> object:
    yield 1
    raise ValueError('inner')

def relayed_failing() -> object:
    got = yield from failing()
    yield got

def guarding(log: list[str]) -> object:
    try:
        yield 1
        yield 2
    except ValueError:
        log.append('caught')
        yield 3
    finally:
        log.append('left')
",
        &[
            // the raising face, which is the only one a python caller can reach
            "[_value(m.returning(v)) for v in \
              ((1, 2), (), (1,), 5, None, [1, 2], 'ab', StopIteration(9), _Sub(3), _Shadowed(7))]",
            // and the exception it builds is shaped the way python shapes one
            "[(lambda e: (repr(e), e.args, e.value))(_capture(next, m.straight(v))) \
              for v in ((1, 2), (), (1,), 5, None, StopIteration(9), _Sub(3), _Shadowed(7))]",
            "[_value(m.straight(v)) for v in ((1, 2), (), (1,), 5, None, _Shadowed(7))]",
            // the send slot, reached three ways: a compiled `yield from` over a
            // compiled generator, a compiled `await` of a compiled coroutine, and
            // asyncio driving one from the outside
            "[list(m.relayed(v)) for v in \
              ((1, 2), (), (1,), 5, None, [1, 2], 'ab', StopIteration(9), _Sub(3), _Shadowed(7))]",
            "[_run(m.relaying(v)) for v in \
              ((1, 2), (), (1,), 5, None, [1, 2], StopIteration(9), _Sub(3), _Shadowed(7))]",
            "[_run(m.finishing(v)) for v in \
              ((1, 2), (), (1,), 5, None, [1, 2], StopIteration(9), _Sub(3), _Shadowed(7))]",
            // a frame that finishes without naming a value still finishes: the slot
            // does not carry that one structurally, so this is the path that asks
            // cpython what a raised `StopIteration` was worth — including the second
            // time round, when the frame is already exhausted
            "list(m.relayed_silent(0))",
            "list(m.relayed_silent(2))",
            "_run(m.relaying_nothing())",
            // a value sent into a delegation reaches the inner frame through the same
            // slot, and has to arrive there rather than at the frame that delegated
            "_sent(m.relaying_sends(2), (7, 8, 9, 10, 11))",
            "_sent(m.relaying_sends(2), ('a', None, 'b', None))",
            // and an exception that is not a finish is not a return value
            "_capture(list, m.relayed_failing())",
            "[(lambda e: (type(e).__name__, str(e)))(_capture(list, m.relayed_failing()))]",
            // a frame that finished stays finished, whichever face asked
            "[(g := m.returning(4), _value(g), type(_capture(next, g)).__name__)[1:]]",
            "[(g := m.returning(4), list(g), list(m.relayed(4)))[1:]]",
            // `close` on a suspended frame and on a finished one are both clean, and
            // both leave it exhausted
            "[(g := m.counting(3), next(g), g.close(), type(_capture(next, g)).__name__)[3:]]",
            "[(g := m.counting(1), list(g), g.close(), type(_capture(next, g)).__name__)[2:]]",
            "[(g := m.returning(9), _value(g), g.close(), type(_capture(next, g)).__name__)[2:]]",
            // `throw` resumes at the suspension: one the body catches carries on, one
            // it does not comes out — and either way the cleanup runs exactly once
            "[(log := [], g := m.guarding(log), next(g), g.throw(ValueError('x')), \
               type(_capture(next, g)).__name__, log)[3:]]",
            "[(log := [], g := m.guarding(log), next(g), \
               type(_capture(g.throw, KeyError('k'))).__name__, \
               type(_capture(next, g)).__name__, log)[3:]]",
        ],
    );
}

/// the send slot is answered by the compiled state object, and its return arrives
/// without an exception
///
/// `agree` cannot see this on its own: the slot is deliberately invisible from
/// python — the same values come back whether it is there or not, which is what
/// makes it safe to add — so a run of the differential tests above would pass with
/// every part of it switched off. what pins it is the emitted C, plus a build that
/// refuses to decline, so the answers really are the compiled frame's
#[test]
fn a_compiled_state_object_answers_the_send_slot() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_sendslot_pin");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def counting(n: int) -> object:
    i = 0
    while i < n:
        yield i
        i = i + 1
    return 'end'

async def finishing(v: object) -> object:
    return v
";
    let options = Options {
        require_native: true,
        ..Options::default()
    };
    if build_source(source, "by_diff_sendslot_pin", &toolchain, &dir, &options).is_err() {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let emitted = std::fs::read_to_string(dir.join("by_diff_sendslot_pin.c"))
        .expect("the generated C is written beside the extension");
    // both surfaces publish the slot, and both report their `return` into the state
    // object rather than by raising. each frame finishes twice over: once where its
    // `return` stands, and once in the block the state dispatch falls through to,
    // which is both running off the end of the body and being resumed after the end
    assert_eq!(emitted.matches(".am_send =").count(), 2, "{emitted}");
    assert_eq!(emitted.matches("By_SendGenerator(").count(), 2, "{emitted}");
    assert_eq!(
        emitted.matches("->by_returned = by_t;").count(),
        4,
        "{emitted}"
    );
    // and no frame is left reporting its own end by raising, in either shape: with a
    // value, or the bare one a frame that names none would have used
    assert!(
        !emitted.contains("By_RaiseWith(PyExc_StopIteration"),
        "{emitted}"
    );
    assert!(
        !emitted.contains("PyErr_SetNone(PyExc_StopIteration)"),
        "{emitted}"
    );

    let out = run(
        &python,
        &dir,
        "import asyncio\n\
         import by_diff_sendslot_pin as m\n\
         print(type(m.counting).__name__)\n\
         print(type(m.counting(0)).__name__)\n\
         print(asyncio.run(m.finishing((1, 2))))\n",
    );
    // a declined function would be a plain `function` and its state a `generator`
    assert_eq!(
        out, "builtin_function_or_method\ncounting$gen\n(1, 2)",
        "{out}"
    );
}

/// a `StopIteration` the body *raised* leaves the frame as a `RuntimeError`, and one
/// the frame's own end produced does not
///
/// pep 479. the two are the same exception class and only the operation that made it
/// tells them apart, which is why a finish is [`Op::FinishFrame`] and not a raise: a
/// finish writes its value into the state object and builds nothing, so an exception
/// standing at the frame's exit can only be one the body raised. converting on the
/// class alone would turn every ordinary `return` into a `RuntimeError`
#[test]
fn a_raised_stop_iteration_leaves_a_generator_as_a_runtime_error() {
    agree_python(
        "pep479",
        "\
from typing import Any


def raised() -> Any:
    yield 1
    raise StopIteration(7)


# the accident pep 479 was written for: nothing here says `StopIteration`, and the
# one an exhausted iterator raises inside the body would have read as this frame's
# own end
def passed_through(source: Any) -> Any:
    yield 1
    next(source)


def relayed() -> Any:
    got = yield from raised()
    yield got


async def awaited() -> Any:
    raise StopIteration(7)


async def async_raised() -> Any:
    yield 1
    raise StopIteration(7)


# only an *async* generator converts this one, because `StopAsyncIteration` is what
# its own protocol uses to mean `ended`
async def async_stopped() -> Any:
    yield 1
    raise StopAsyncIteration


# a plain generator raising it means nothing in particular, and is left alone
def stopped_async() -> Any:
    yield 1
    raise StopAsyncIteration


def raised_on_exit() -> Any:
    try:
        yield 1
    except GeneratorExit:
        raise StopIteration(9)


def raised_in_handler() -> Any:
    try:
        yield 1
    except ValueError:
        raise StopIteration(4)


def raised_subclass(error: Any) -> Any:
    yield 1
    raise error


# the other half of the pin: every one of these *ends*, and an end is still a plain
# `StopIteration` carrying the value the `return` named
def ended(v: Any) -> Any:
    yield 1
    return v


def ended_bare() -> Any:
    yield 1
    return


def ended_falling_off() -> Any:
    yield 1
",
        &[
            // the raise, and the chaining that goes with the conversion
            "_escaped(list, m.raised())",
            "_escaped(list, m.passed_through(iter([])))",
            "_escaped(list, m.relayed())",
            "_escaped(_run, m.awaited())",
            "_escaped(_run, _drain(m.async_raised()))",
            "_escaped(_run, _drain(m.async_stopped()))",
            // and the one surface that does not convert it
            "_escaped(list, m.stopped_async())",
            // a subclass carries its own class into the cause, and one that shadows
            // `.value` still does — the conversion reads the exception, not a field
            "_escaped(list, m.raised_subclass(_Sub(3)))",
            "_escaped(list, m.raised_subclass(_Shadowed(7)))",
            "_escaped(list, m.raised_subclass(StopIteration((1, 2))))",
            // `close` and `throw` reach the frame the same way a resumption does, so
            // the conversion has to happen for them too — and `close` must not go on
            // reading the converted error as the clean exhaustion it accepts
            "[(g := m.raised_on_exit(), next(g), _escaped(g.close))[2:]]",
            "[(g := m.raised_in_handler(), next(g), _escaped(g.throw, ValueError()))[2:]]",
            // an end is not a raise, on either face: the value comes back whole, the
            // exception is a plain `StopIteration`, and nothing is chained onto it
            "[_value(m.ended(v)) for v in ((1, 2), (), (1,), 5, None, 'ab', _Shadowed(7))]",
            "[_escaped(list, m.ended(v)) for v in ((1, 2), (), (1,), 5, None)]",
            "_escaped(list, m.ended_bare())",
            "_escaped(list, m.ended_falling_off())",
            // including the second time round, when the frame is already exhausted
            "[(g := m.ended((1, 2)), list(g), _escaped(next, g))[2:]]",
            "[(g := m.raised(), _escaped(list, g), _escaped(next, g))[1:]]",
        ],
    );
}

/// the two resumptions python refuses, at either end of a machine's life
///
/// a frame that has never run is suspended at no `yield`, so a sent value has no
/// expression to become and python refuses a non-`None` one outright. all three
/// surfaces refuse it and only the noun differs, which is why one rule serves.
///
/// the other end is the coroutine's alone: one that has finished is spent rather than
/// exhausted, so awaiting it again raises where a generator hands back the
/// `StopIteration` that means "ended". the exemptions are the interesting half —
/// `close()` on a spent coroutine passes, because that is how a caller says it is
/// done with it, and a `throw` into an *unstarted* one just propagates.
#[test]
fn the_resumptions_python_refuses_agree() {
    agree_python(
        "resumerefusals",
        "\
from typing import Any


def counting() -> Any:
    yield 1
    yield 2


async def once() -> int:
    return 7


async def streaming() -> Any:
    yield 1
",
        &[
            // a value sent into a frame that has not started, on each of the three
            // surfaces — one rule, and only the noun in the message differs
            "_escaped(m.counting().send, 5)",
            "_refused(m.once(), 'send', 5)",
            "_escaped(_run, _awaits(m.streaming().asend(5)))",
            // `None` into an unstarted frame is how `next` itself resumes, so it stands
            "m.counting().send(None)",
            "[next(g := m.counting()), g.send(None)]",
            // a spent coroutine is not an exhausted iterator, and says so to both
            // the resumption a second await makes and to a throw
            "_escaped(_spend(m.once()).send, None)",
            "_escaped(_spend(m.once()).throw, ValueError('x'))",
            // closing one is how a caller says it is done with it, and passes
            "_spend(m.once()).close()",
            // and a throw into one that never started carries its own exception out
            "_refused(m.once(), 'throw', ValueError('x'))",
            // the surfaces that really do end must be left exactly as they were: a
            // generator answers `StopIteration`, an async generator
            // `StopAsyncIteration`, and a throw into a finished generator propagates
            "_escaped(_drained(m.counting()).send, None)",
            "_escaped(_drained(m.counting()).throw, ValueError('x'))",
            "_escaped(_run, _asend_spent(m.streaming()))",
        ],
    );
}

/// a resumption that carries no value leaves the `yield` evaluating to `None`
///
/// `next(g)` *is* `g.send(None)`. the value a `send` carries is parked in a field the
/// resumed `yield` reads, and a resumption that skipped the store left the previous
/// `send`'s value standing there to be read a second time. every way into the frame
/// has to park it: `tp_iternext`, the send slot a `yield from` reaches, `throw`, and
/// an async generator's `__anext__`
#[test]
fn a_resumption_carrying_nothing_sends_none() {
    agree_python(
        "sentnone",
        "\
from typing import Any


def echoing(n: int) -> Any:
    i = 0
    while i < n:
        got = yield i
        yield got
        i = i + 1


# a `yield` whose value is discarded reads nothing, so the field it did not consume
# is still there for the next one that does
def discarding() -> Any:
    yield 1
    got = yield 2
    yield got


def relaying(n: int) -> Any:
    yield from echoing(n)


def recovering() -> Any:
    got = yield 1
    try:
        yield ('a', got)
        yield 'unreached'
    except ValueError:
        after = yield 'caught'
        yield ('b', after)


async def async_echoing(n: int) -> Any:
    i = 0
    while i < n:
        got = yield i
        yield got
        i = i + 1
",
        &[
            // the shape the divergence was found in: a `send`, then two plain
            // resumptions. the second is the one that reads the field again
            "_renext(m.echoing(4), 7, 2)",
            "_renext(m.echoing(4), None, 2)",
            "_renext(m.discarding(), 7, 1)",
            // the send slot, which a `yield from` reaches instead of `tp_iternext`
            "_renext(m.relaying(4), 7, 2)",
            // `throw` carries no value in either, so a `yield` after a handled one
            // must not read what the last `send` left
            "[(g := m.recovering(), next(g), g.send(9), g.throw(ValueError()), next(g))[3:]]",
            // and the async surface, whose `asend(None)` is its `__anext__`
            "_run(_reasend(m.async_echoing(4), 7, 2))",
            "_run(_reasend(m.async_echoing(4), None, 2))",
            // a value that *is* sent still arrives, which is the half that already
            // worked and the half a fix could break
            "_sent(m.echoing(4), (7, 8, 9))",
            "_sent(m.relaying(4), ('a', 'b'))",
        ],
    );
}

/// the frame's surface reaches the runtime, and every resumption parks a value
///
/// `agree` sees the conversion's *message*, which names the surface, so a generator
/// answering as a coroutine is already caught. what it cannot see is the third
/// constant: an async generator and a coroutine differ only in whether
/// `StopAsyncIteration` converts, and a module with no async generator raising one
/// would agree either way. this pins the mapping at the point it is emitted
#[test]
fn a_state_object_tells_the_runtime_which_surface_it_is() {
    let Some((_python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_framekind_pin");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
from typing import Any


def plain() -> Any:
    yield 1


async def awaited() -> Any:
    return 1


async def streamed() -> Any:
    yield 1
";
    let options = Options {
        require_native: true,
        language: by_irbuild::Language::Python,
        ..Options::default()
    };
    if build_source(source, "by_diff_framekind_pin", &toolchain, &dir, &options).is_err() {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let emitted = std::fs::read_to_string(dir.join("by_diff_framekind_pin.c"))
        .expect("the generated C is written beside the extension");
    // each surface names itself, and none of them names another
    for (function, kind) in [
        ("plain", "BY_FRAME_GENERATOR"),
        ("awaited", "BY_FRAME_COROUTINE"),
        ("streamed", "BY_FRAME_ASYNC_GENERATOR"),
    ] {
        let symbol = format!("By_by_diff_framekind_pin_{function}_gen_Type_iternext");
        let iternext = emitted_function(&emitted, &symbol);
        assert!(iternext.contains(kind), "{function}: {iternext}");
        for other in [
            "BY_FRAME_GENERATOR",
            "BY_FRAME_COROUTINE",
            "BY_FRAME_ASYNC_GENERATOR",
        ] {
            assert_eq!(
                iternext.contains(other),
                other == kind,
                "{function} named {other}: {iternext}"
            );
        }
        // and it carries `None` in, because a resumption through this slot carries
        // nothing — the store cannot be skipped or the last `send` survives it
        assert!(iternext.contains("Py_None"), "{function}: {iternext}");
    }
}

/// one emitted C function's body, from its definition to the closing brace
///
/// a whole-module `contains` is not an assertion about the function it was written
/// for: every emitted function's text sits in the same file, so a claim about one
/// passes on another's
fn emitted_function<'a>(emitted: &'a str, symbol: &str) -> &'a str {
    let start = emitted
        .match_indices(&format!("{symbol}("))
        // a longer symbol ending in this one is a different function, and the
        // forward declaration is not the definition — only the definition's
        // parameter list is followed by a body.
        //
        // `*` counts as a boundary as much as a space does: a function returning a
        // pointer is emitted both ways, `PyObject * name` for a lowered body and
        // `PyObject *name` for a slot, and only the second would be missed
        .find(|(at, _)| {
            emitted[..*at].ends_with([' ', '\n', '*'])
                && emitted[*at..]
                    .split_once(')')
                    .is_some_and(|(_, rest)| rest.starts_with(" {"))
        })
        .map(|(at, _)| at)
        .unwrap_or_else(|| panic!("{symbol} was not emitted:\n{emitted}"));
    let end = emitted[start..]
        .find("\n}\n")
        .unwrap_or_else(|| panic!("{symbol} has no end:\n{emitted}"));
    &emitted[start..start + end]
}

/// a frame *finishing* and a body raising `StopIteration` are emitted differently
///
/// they arrive at codegen as different operations, which is the point: a bare
/// `return`, running off the end and being resumed after the end all mean the frame
/// is done, while `raise StopIteration` is an exception the body chose to raise. only
/// the first three may be reported through the state object.
///
/// the difference is now visible from python too, in
/// [`a_raised_stop_iteration_leaves_a_generator_as_a_runtime_error`]: the raised one
/// comes back as a `RuntimeError` and the three finishes do not. this test stays
/// because that conversion *rests* on the split — it reads "an exception is standing
/// at the frame's exit" as "the body raised", which is only true while a finish
/// builds no exception at all. the emitted C is where that stops being true first
#[test]
fn a_finish_is_emitted_apart_from_a_written_stop_iteration() {
    let Some((_, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_finish_pin");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def bare(n: int) -> object:
    yield n
    return

def falling(n: int) -> object:
    yield n

def written(n: int) -> object:
    yield n
    raise StopIteration
";
    let options = Options {
        require_native: true,
        ..Options::default()
    };
    if build_source(source, "by_diff_finish_pin", &toolchain, &dir, &options).is_err() {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let emitted = std::fs::read_to_string(dir.join("by_diff_finish_pin.c"))
        .expect("the generated C is written beside the extension");
    let resume = |name: &str| {
        emitted_function(
            &emitted,
            &format!("by_by_diff_finish_pin_{name}_gen__resume"),
        )
    };
    let finishes = |body: &str| body.matches("->by_returned = by_t;").count();
    let raises = |body: &str| body.matches("PyErr_SetNone(PyExc_StopIteration)").count();

    // a bare `return` finishes, and so does the block the state dispatch falls
    // through to — which is both the end of the body and a resume past the end
    assert_eq!(finishes(resume("bare")), 2, "{}", resume("bare"));
    assert_eq!(raises(resume("bare")), 0, "{}", resume("bare"));
    // a body with no `return` at all has only the implicit end
    assert_eq!(finishes(resume("falling")), 1, "{}", resume("falling"));
    assert_eq!(raises(resume("falling")), 0, "{}", resume("falling"));
    // and a written raise is a raise: the frame's own end beside it is still a
    // finish, so this one function holds exactly one of each
    assert_eq!(finishes(resume("written")), 1, "{}", resume("written"));
    assert_eq!(raises(resume("written")), 1, "{}", resume("written"));
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

/// a `throw` needs a suspension point to raise *at*, and two machines have none: one
/// that never started has not reached its first `yield`, and one that has finished has
/// left its frame for good. python raises the exception at the call site for both and
/// runs no body at all — so the `try` around the first `yield` below does not catch a
/// throw that arrives before the generator was ever stepped, and the `finally` does
/// not run.
///
/// resuming instead gets both wrong, and wrong about *which exception the caller is
/// holding*: a machine that never started runs its body from the top and answers with
/// the first yielded value, and a finished one reports exhaustion — so `throw`
/// answered `StopIteration` where python answers with the thing that was thrown.
///
/// `close()` asks the same question and answers `None`: there is nothing to unwind
#[test]
fn a_throw_into_a_frame_with_no_suspension_raises_at_the_call_site() {
    agree_python(
        "throwfinished",
        "\
from typing import Any


def guarded(log: list[str]) -> Any:
    try:
        yield 1
        yield 2
    except ValueError:
        log.append('caught')
        yield 99
    finally:
        log.append('closed')
",
        &[
            // never started
            "[(log := [], g := m.guarded(log), (e := _capture(g.throw, ValueError('boom'))), (type(e).__name__, str(e)), log)[3:]]",
            // and one thrown into that way is finished afterwards
            "[(log := [], g := m.guarded(log), _capture(g.throw, ValueError('boom')), type(_capture(next, g)).__name__, log)[3:]]",
            // finished
            "[(log := [], g := m.guarded(log), list(g), (e := _capture(g.throw, ValueError('boom'))), (type(e).__name__, str(e)), log)[4:]]",
            // `close()` on either runs nothing, and leaves it finished
            "[(log := [], g := m.guarded(log), g.close(), log)[2:]]",
            "[(log := [], g := m.guarded(log), g.close(), type(_capture(next, g)).__name__, log)[3:]]",
            "[(log := [], g := m.guarded(log), list(g), g.close(), log)[3:]]",
            // the argument is still checked, by the rule `throw` uses wherever it lands
            "[(g := m.guarded([]), list(g), str(_capture(g.throw, 42)))[2:]]",
            "[(g := m.guarded([]), str(_capture(g.throw, 'no')))[1:]]",
            "[(g := m.guarded([]), list(g), repr(_capture(g.throw, ValueError)))[2:]]",
            // and the case that *does* have a suspension still resumes into the body's
            // own handler, which is what the whole `$thrown` field exists for
            "[(log := [], g := m.guarded(log), next(g), g.throw(ValueError('boom')), log)[3:]]",
        ],
    );
}

/// the async half of the same question, and the two surfaces do *not* answer alike:
/// an `athrow` into a finished async generator ends the await with `None` rather than
/// re-raising, where one into a machine that never started raises at the call site
#[test]
fn an_athrow_into_a_frame_with_no_suspension_agrees() {
    agree_python(
        "athrowfinished",
        "\
from typing import Any


async def guarded(log: list[str]) -> Any:
    try:
        yield 1
    except ValueError:
        log.append('caught')
    finally:
        log.append('closed')
",
        &[
            "[(log := [], _run(_athrown(m.guarded(log), ValueError, 0)), log)[1:]]",
            "[(log := [], _run(_athrown(m.guarded(log), ValueError, 3)), log)[1:]]",
            // `aclose` runs no body for either, exactly as `close` does not
            "[(log := [], _run(_aclosed(m.guarded(log), 0)), log)[1:]]",
            "[(log := [], _run(_aclosed(m.guarded(log), 3)), log)[1:]]",
            // and the suspended case still reaches the body's handler
            "[(log := [], _run(_athrown(m.guarded(log), ValueError, 1)), log)[1:]]",
        ],
    );
}

#[test]
fn a_parked_value_does_not_leak() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_parkleak");
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
    let dir = diff_root().join("by_diff_coro");
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
    let dir = diff_root().join("by_diff_coroleak");
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
    let dir = diff_root().join("by_diff_defaultleak");
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
    let dir = diff_root().join("by_diff_varleak");
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

/// a `@property` and the `@name.setter` under it are one attribute
///
/// python folds the two `def`s into a single `property` bound once, and an emitted type
/// builds the same object out of the same two halves at module init. so the write runs
/// the setter's body, the read runs the getter's, and neither half is reachable under its
/// own name
#[test]
fn a_property_pair_agrees() {
    agree_python(
        "proppair",
        "\
class Box:
    def __init__(self, n: int) -> None:
        self._n = n

    @property
    def value(self) -> int:
        return self._n * 10

    @value.setter
    def value(self, given: int) -> None:
        self._n = given + 1


def raised(fn: object) -> str:
    try:
        fn()
    except AttributeError as error:
        return str(error)
    return 'nothing raised'
",
        &[
            "m.Box(3).value",
            // the setter's body ran, not a store beside it
            "(lambda b: (setattr(b, 'value', 4), b.value, b._n))(m.Box(0))",
            // a pair with no deleter refuses `del` in python's own wording, which names
            // the *object's* type rather than the class the property was written in
            "m.raised(lambda: delattr(m.Box(1), 'value'))",
            // and neither half answers under its own name
            "hasattr(m.Box(1), 'value$get')",
        ],
    );
}

/// what the type publishes under a property's name is a `property`
///
/// this is the half no behavioural comparison can reach. an attribute published through
/// `tp_getset` reads and writes exactly as a property does, so every test that only
/// *uses* the attribute passes either way — while `C.value.fget` raises, and so do
/// `.fset`, `.getter(...)`, `.setter(...)` and `isinstance(C.value, property)`. so this
/// asks the type what it holds rather than asking an instance what it answers.
///
/// the half inside it is what says which leg ran: a `method_descriptor` is the compiled
/// one, and an interpreted fallback would hold a `function` there
#[test]
fn a_property_is_published_as_a_property_object() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_propobject");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Box:
    def __init__(self, n: int) -> None:
        self._n = n

    @property
    def value(self) -> int:
        return self._n

    @value.setter
    def value(self, given: int) -> None:
        self._n = given
";
    let built = match build_source(
        source,
        "by_diff_propobject",
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
        "import by_diff_propobject as m\n\
         p = m.Box.__dict__['value']\n\
         print(type(p).__name__, isinstance(p, property), p.fdel)\n\
         print(type(p.fget).__name__, p.fget.__name__, p.fget.__qualname__)\n\
         print(type(p.fset).__name__, p.fset.__qualname__)\n\
         # the halves are reached through the property and under no name of their own\n\
         print(p.fget(m.Box(4)), p.fset(m.Box(0), 1), hasattr(m.Box, 'value$get'))\n\
         # each verb builds a *new* property and leaves the class holding the old one\n\
         fresh = p.deleter(lambda self: None)\n\
         print(type(fresh).__name__, fresh is p, fresh.fget is p.fget,\n\
        \x20     m.Box.__dict__['value'] is p)\n",
    );
    assert_eq!(
        out,
        "property True None\n\
         method_descriptor value Box.value\n\
         method_descriptor Box.value\n\
         4 None False\n\
         property False True True"
    );
}

/// a class whose type is a static struct is published to the same way
///
/// nearly every emitted class is built from a type spec, and a property could have been
/// put on one of those with `setattr`. a class carrying a field spelled as a dunder is
/// the exception: it stays the static `PyTypeObject` it always was, and python refuses
/// `setattr` on one outright — "cannot set 'x' attribute of immutable type". so the
/// property is written into `tp_dict`, which is the one route open to both kinds, and
/// this is the kind that would have caught a change back to the other
#[test]
fn a_property_reaches_a_static_type_as_well() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_propstatic");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Holder:
    def __init__(self) -> None:
        self.__wrapped__ = 1

    @property
    def value(self) -> int:
        return self.__wrapped__ * 2

    @value.setter
    def value(self, given: int) -> None:
        self.__wrapped__ = given
";
    let built = match build_source(
        source,
        "by_diff_propstatic",
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
        "import by_diff_propstatic as m\n\
         # without this the class is a spec type and the test is the one above again\n\
         print(bool(m.Holder.__flags__ & (1 << 9)))\n\
         p = m.Holder.__dict__['value']\n\
         print(type(p).__name__, type(p.fget).__name__)\n\
         h = m.Holder()\n\
         h.value = 5\n\
         print(h.value)\n",
    );
    assert_eq!(out, "False\nproperty method_descriptor\n10");
}

/// a property answers to its own name, which is what a missing half is reported under
///
/// nothing calls `__set_name__` on a property built at module init, and `type.__new__`
/// is what calls it on one a class body left behind. below 3.13 that call is the *only*
/// thing that gives a property a name: one that never got it reports a missing half as
/// "property of 'Box' object has no setter", with the name dropped out of the middle.
/// 3.13 grew a fallback to the getter's own name, so the two are indistinguishable there
/// — this bites when it is run under 3.11 or 3.12.
///
/// the name is asked for through the wording rather than through `property.__name__`,
/// which is itself 3.13 and later
#[test]
fn a_property_carries_the_name_it_was_published_under() {
    agree_python(
        "propname",
        "\
class Box:
    def __init__(self, n: int) -> None:
        self._n = n

    @property
    def value(self) -> int:
        return self._n

    @value.deleter
    def value(self) -> None:
        self._n = 0


def raised(fn: object) -> str:
    try:
        fn()
    except AttributeError as error:
        return str(error)
    return 'nothing raised'
",
        &[
            "type(m.Box.__dict__['value']).__name__",
            // written with no setter, so the write is what raises — and the wording names
            // the property, which is the name `__set_name__` settled
            "m.raised(lambda: setattr(m.Box(1), 'value', 2))",
        ],
    );
}

/// a property with a deleter, and one without a setter
///
/// `del` reaches the deleter the `property` holds, and a half the body never wrote raises
/// there instead — in python's own wording for a missing one, naming which half it wanted
#[test]
fn a_property_deleter_agrees() {
    agree_python(
        "propdelete",
        "\
class Slot:
    def __init__(self) -> None:
        self._held = 1

    @property
    def held(self) -> int:
        return self._held

    @held.deleter
    def held(self) -> None:
        self._held = 0


def raised(fn: object) -> str:
    try:
        fn()
    except AttributeError as error:
        return str(error)
    return 'nothing raised'
",
        &[
            "m.Slot().held",
            "(lambda s: (delattr(s, 'held'), s.held))(m.Slot())",
            // written with a deleter and no setter, so assigning is what raises
            "m.raised(lambda: setattr(m.Slot(), 'held', 5))",
        ],
    );
}

/// a subclass that writes one half of a property writes a whole new one
///
/// python re-derives the property from what the subclass body bound, so a subclass with
/// only a getter has no setter at all — the base's is not inherited into it. the emitted
/// type has to shadow the base's entry the same way, or a write the interpreted class
/// refuses would quietly reach the base's setter
#[test]
fn a_subclass_overriding_one_half_of_a_property_agrees() {
    agree_python(
        "propoverride",
        "\
class Base:
    def __init__(self, n: int) -> None:
        self._n = n

    @property
    def value(self) -> int:
        return self._n

    @value.setter
    def value(self, given: int) -> None:
        self._n = given


class Narrow(Base):
    @property
    def value(self) -> int:
        return self._n * 2


def raised(fn: object) -> str:
    try:
        fn()
    except AttributeError as error:
        return str(error)
    return 'nothing raised'
",
        &[
            "m.Base(3).value",
            "m.Narrow(3).value",
            "(lambda b: (setattr(b, 'value', 8), b.value))(m.Base(0))",
            // the subclass wrote no setter, so it has none — the base's does not carry
            "m.raised(lambda: setattr(m.Narrow(1), 'value', 8))",
        ],
    );
}

/// a property whose halves are not the shape a getset stands for
///
/// each of these is *nearly* the construct and is turned down for what it actually is,
/// rather than falling through to a message about a name written twice
#[test]
fn a_property_the_backend_cannot_fold_declines() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_propdecline");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def marking(fn: object) -> object:
    return fn


class Restated:
    @property
    def value(self) -> int:
        return 1

    @value.getter
    def value(self) -> int:
        return 2


class Stacked:
    @marking
    @property
    def value(self) -> int:
        return 3

    @value.setter
    def value(self, given: int) -> None:
        pass


class Wide:
    @property
    def value(self) -> int:
        return 5

    @value.setter
    def value(self, given: int, extra: int = 1) -> None:
        pass
";
    let built = match build_source(
        source,
        "by_diff_propdecline",
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
    let reasons = |needle: &str| {
        built
            .declined
            .iter()
            .any(|declined| declined.reason.contains(needle))
    };
    assert!(
        reasons("writes a second `getter`"),
        "declined: {:?}",
        built.declined
    );
    assert!(
        reasons("what stands above them is not a plain `@property`"),
        "declined: {:?}",
        built.declined
    );
    assert!(
        reasons("is called with exactly 2 argument(s)"),
        "declined: {:?}",
        built.declined
    );
    // every one of them keeps its interpreted definition, so the `property` the body
    // built is still what stands under the name
    let out = run(
        &python,
        &dir,
        "import by_diff_propdecline as m\n\
         print(m.Restated().value, m.Stacked().value, m.Wide().value,\n\
        \x20     type(m.Restated.value).__name__)\n",
    );
    assert_eq!(out, "2 3 5 property");
}

#[test]
fn a_decorated_method_agrees() {
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
    let dir = diff_root().join("by_diff_arrayleak");
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
    let dir = diff_root().join("by_diff_pyloop");
    let interpreted = diff_root().join("by_diff_pyloop_i");
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
fn a_parameter_its_own_body_rebinds_covers_both_representations() {
    // an unannotated parameter is declared by its default, so `safe='/'` makes the
    // register a `str` — and then the body puts bytes there. the store narrowed the
    // object back to a `str` with a check, and the check raised on a call the
    // interpreter answers: `urllib.parse.quote_from_bytes(b'a b')` was `'a%20b'`
    // interpreted and `TypeError: expected str, got bytes` compiled
    agree_python(
        "reboundparam",
        "\
def quoted(bs, safe='/'):
    if isinstance(safe, str):
        safe = safe.encode('ascii', 'ignore')
    return repr(bs) + repr(safe)


def stepped(n, step=1):
    step = str(step)
    return step * n


def flagged(x, on=False):
    if x:
        on = 'yes'
    return repr(on)
",
        &[
            "m.quoted(b'a b')",
            // the boundary unboxed to the default's representation too, so a caller
            // supplying the *other* one was refused before the body was reached
            "m.quoted(b'a b', b'/')",
            "m.stepped(2)",
            "m.stepped(2, 3)",
            "m.flagged(1)",
            "m.flagged(0)",
            "m.flagged(0, 'no')",
        ],
    );
}

#[test]
fn a_walrus_and_an_exception_handler_are_writes_a_register_has_to_cover_too() {
    // the two binding forms an assignment statement does not cover. a walrus hides
    // inside an expression and a handler's name is on the `try` rather than in its
    // body, so neither was counted when a register's representation was decided —
    // and both then stored through a check that refused the value they had just bound
    agree_python(
        "walrushandler",
        "\
def walrused(safe='/'):
    if (safe := safe.encode('ascii')):
        return repr(safe)
    return 'empty'


def caught(tag='t'):
    try:
        raise ValueError('boom')
    except ValueError as tag:
        return repr(tag)
",
        &["m.walrused()", "m.walrused('ab')", "m.caught()"],
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

/// the family a class appending storage grows: `TokenList` keeps a field past a `list`
/// instance, and every class under it keeps that one and adds none
///
/// this is the stdlib's own shape — `email._header_value_parser` writes thirty-seven of
/// them — and the whole point is that a subclass adding no field of its own appends
/// nothing: what it stores is what `TokenList` stores, at the offset `TokenList` laid it
/// out and through the descriptor `TokenList` published. `Restating` is the same class
/// written the other way round, assigning an attribute the base already keeps
const APPENDED_FAMILY: &str = "\
class TokenList(list):

    token_type = None

    def __init__(self, *args):
        super().__init__(*args)
        self.defects = []

    def kind(self):
        return self.token_type

    def defect_count(self):
        return len(self.defects)


class Plain(TokenList):
    pass


class Named(TokenList):
    token_type = 'named'


class Deeper(Named):
    token_type = 'deeper'

    def kind(self):
        return 'deep:' + str(self.token_type)


class Restating(TokenList):
    def __init__(self, *args):
        super().__init__(*args)
        self.defects = ['restated']
";

#[test]
fn a_subclass_that_appends_nothing_past_a_base_agrees() {
    agree_python(
        "appendnothing",
        APPENDED_FAMILY,
        &[
            "[(type(t).__name__, list(t), t.defects, t.kind(), t.defect_count())\n\
             \x20 for t in (m.TokenList([0]), m.Plain([1]), m.Named([2]), m.Deeper([3]),\n\
             \x20           m.Restating([4]))]",
            // the base's field, written and read through a subclass instance
            "[(p.defects.append('one'), p.defects, p.defect_count(),\n\
             \x20  (p.append(9), list(p))[1]) for p in [m.Plain([1, 2])]]",
            "[c.__name__ for c in m.Deeper.__mro__]",
            "(isinstance(m.Deeper([1]), m.TokenList), isinstance(m.Restating([1]), list),\n\
             \x20issubclass(m.Plain, m.TokenList))",
            "(m.Plain.token_type, m.Named.token_type, m.Deeper.token_type)",
            "(sorted(m.Plain([3, 1, 2])), m.Plain([1, 2]) == [1, 2], m.Plain([1]) + [2])",
            // python's own subclass of one, built by the class statement rather than here
            "[(list(s([5])), s([5]).defects, s([5]).kind())\n\
             \x20 for s in [type('Py', (m.Plain,), {'token_type': 'py'})]]",
            // every instance holds a cycle through the appended field, so the collector
            // has to be able to see it and the deallocation has to release it
            "(len([t for t in [m.Deeper([i]) for i in range(200)]\n\
             \x20     if t.defects.append(t) is None]), __import__('gc').collect() >= 0)",
        ],
    );
}

#[test]
fn a_subclass_that_appends_nothing_is_the_compiled_type() {
    // the behaviour above is answered identically by a class that fell back to its
    // interpreted definition, so it cannot say which build answered.
    // `method_descriptor` can: a real type of ours holds one where the interpreted
    // class holds a plain function
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_appendnothing_t");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        APPENDED_FAMILY,
        "by_diff_appendnothing_t",
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
        "import by_diff_appendnothing_t as m\n\
         print(type(m.TokenList.__dict__['kind']).__name__,\n\
         \x20     type(m.Deeper.__dict__['kind']).__name__)\n\
         print(m.TokenList.__basicsize__ == m.Plain.__basicsize__\n\
         \x20     == m.Named.__basicsize__ == m.Deeper.__basicsize__)\n\
         print(m.Deeper([1]).defects, m.Restating([1]).defects)\n",
    );
    assert_eq!(
        out,
        "method_descriptor method_descriptor\n\
         True\n\
         [] ['restated']"
    );
}

/// a class whose storage would sit past a **heap** base, beside classes whose base is one
/// python allocates itself
///
/// `SubprocessError` is written in python, so it is a heap type, and no spec can build
/// `Held` on it: python's own `tp_dealloc` picks the deallocator to chain to out of
/// `Py_TYPE(self)`, finds the one this class supplies, and calls it straight back until
/// the stack runs out. leaving `Held` as its interpreted definition is the only answer,
/// and always was.
///
/// what that used to cost was the whole module. the refusal has to be a wide one wherever
/// compiled code would read one of these instances as its own struct — the offset is one
/// only the emitted type lays out, and the interpreted definition's instance stops where
/// the base's does. nothing here does: `Held` is named by nothing, stood on by nothing,
/// and built by nothing. so it alone falls back, and `Kept`, `Deeper` and `add` — every
/// one of which used to be given up with it — go on standing.
///
/// this is `asyncio.unix_events`'s shape, where `_UnixSelectorEventLoop` stands on a heap
/// base from another module and took `PidfdChildWatcher` and `_UnixSubprocessTransport`
/// down with it
const REFUSED_BESIDE_KEPT: &str = "\
from subprocess import SubprocessError


class Held(SubprocessError):
    def __init__(self, tag):
        super().__init__(tag)
        self.tag = tag

    def read(self):
        return self.tag


class Kept(Exception):
    def __init__(self, tag):
        super().__init__(tag)
        self.tag = tag
        self.seen = []

    def note(self, value):
        self.seen.append(value)
        return len(self.seen)


class Deeper(Kept):
    def __init__(self, tag):
        super().__init__(tag)
        self.depth = 1

    def down(self):
        return self.depth + len(self.seen)


def add(a, b):
    return a + b
";

#[test]
fn a_class_no_spec_can_build_agrees_beside_the_ones_that_stand() {
    agree_python(
        "perclassheld",
        REFUSED_BESIDE_KEPT,
        &[
            "(m.Held('h').read(), m.Kept('k').note(1), m.Deeper('d').down(), m.add(2, 3))",
            "[(type(e).__name__, e.args, str(e)) for e in\n\
             \x20 (m.Held('h'), m.Kept('k'), m.Deeper('d'))]",
            "[c.__name__ for c in m.Deeper.__mro__]",
            "(isinstance(m.Deeper('d'), m.Kept), isinstance(m.Held('h'), Exception),\n\
             \x20 issubclass(m.Deeper, m.Kept))",
            // the classes that used to be lost with it, doing the thing their own
            // appended storage is for
            "[(k.note('a'), k.note('b'), k.seen) for k in [m.Kept('k')]]",
            "[(d.note(d.depth), d.down(), d.seen) for d in [m.Deeper('d')]]",
            // raised and caught, which is what an exception class is for. the refused one
            // included: it is a working class either way, just not a compiled one
            "[(type(e).__name__, e.args, str(e)) for e in\n\
             \x20 (_raised_and_caught(m.Held, 'boom'), _raised_and_caught(m.Kept, 'bang'),\n\
             \x20  _raised_and_caught(m.Deeper, 'crash'))]",
            // every instance holds a cycle through its appended field, so the collector
            // has to see it and the deallocation has to release what it finds
            "(len([k for k in [m.Kept(str(i)) for i in range(200)]\n\
             \x20     if k.note(k) == 1]), __import__('gc').collect() >= 0)",
        ],
    );
}

/// which build answered, which no comparison of the two legs can say: a class that fell
/// back to its interpreted definition answers exactly what a compiled one does
///
/// `function` against `method_descriptor` is what pins it. `Held` has to be the one and
/// only `function` — a change that refused nothing would make it `method_descriptor`, and
/// one that went back to refusing the module would make `Kept` and `Deeper` `function`
/// too and `add` an ordinary python function
#[test]
fn only_the_class_no_spec_can_build_is_left_interpreted() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_perclassheld_t");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        REFUSED_BESIDE_KEPT,
        "by_diff_perclassheld_t",
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
        "import by_diff_perclassheld_t as m\n\
         print(type(m.Held.__dict__['read']).__name__,\n\
         \x20     type(m.Kept.__dict__['note']).__name__,\n\
         \x20     type(m.Deeper.__dict__['down']).__name__,\n\
         \x20     type(m.add).__name__)\n\
         # a spec has no code object to write one from, so its absence is the emitted type\n\
         print('__firstlineno__' in vars(m.Held), '__firstlineno__' in vars(m.Kept))\n",
    );
    assert_eq!(
        out,
        "function method_descriptor method_descriptor builtin_function_or_method\n\
         True False"
    );
}

/// and the classes that stand beside the refused one still lay out, collect and free
/// correctly
///
/// each rung of appended storage supplies a `tp_dealloc`, `tp_traverse` and `tp_clear` of
/// its own that chains to the one below, and the sizes say the storage really is appended
/// rather than shared. every instance here holds a cycle through its own appended field,
/// so the collector has to be able to walk into it and the deallocation has to let go of
/// what it finds — which is the failure that segfaulted 24 of the `encodings` modules
#[test]
fn the_classes_beside_a_refused_one_lay_out_and_deallocate() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_perclassheld_d");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        REFUSED_BESIDE_KEPT,
        "by_diff_perclassheld_d",
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
        "import gc\n\
         import by_diff_perclassheld_d as m\n\
         # each rung's own storage is past the one below it, so the sizes strictly grow\n\
         print(Exception.__basicsize__ < m.Kept.__basicsize__ < m.Deeper.__basicsize__)\n\
         def live():\n\
         \x20   gc.collect()\n\
         \x20   return tuple(sum(1 for o in gc.get_objects() if type(o) is c)\n\
         \x20                for c in (m.Held, m.Kept, m.Deeper))\n\
         start = live()\n\
         for _ in range(3000):\n\
         \x20   d = m.Deeper('t')\n\
         \x20   d.note(d)\n\
         \x20   k = m.Kept('k')\n\
         \x20   k.note(k)\n\
         \x20   h = m.Held('h')\n\
         del d, k, h\n\
         print(start, live())\n",
    );
    assert_eq!(out, "True\n(0, 0, 0) (0, 0, 0)");
}

/// a chain where every rung keeps fields of its own past the one below, which is the
/// stdlib's commonest exception family — `configparser` writes ten of them
///
/// each rung's storage is a region of its own past the base's instance, and reaching it
/// takes a `tp_dealloc`, `tp_traverse` and `tp_clear` of that rung's own. those three
/// read the base to chain to from the type that *declared* them, so the chain walks down
/// to `Exception` and stops. `Silent` is the boundary in the other direction: it adds no
/// field, so it appends nothing and is built the way any class with no storage is
const APPENDED_CHAIN: &str = "\
class Error(Exception):
    def __init__(self, message):
        Exception.__init__(self, message)
        self.message = message

    def label(self):
        return 'error:' + self.message


class SectionError(Error):
    def __init__(self, message, section):
        Error.__init__(self, message)
        self.section = section

    def label(self):
        return 'section:' + self.section + ':' + self.message


class OptionError(SectionError):
    def __init__(self, message, section, option):
        SectionError.__init__(self, message, section)
        self.option = option


class Silent(SectionError):
    pass
";

#[test]
fn a_chain_of_appended_storage_agrees() {
    agree_python(
        "appendchain",
        APPENDED_CHAIN,
        &[
            "[(e.message, e.label(), e.args) for e in [m.Error('m')]]",
            "[(e.message, e.section, e.label()) for e in [m.SectionError('m', 's')]]",
            "[(e.message, e.section, e.option, e.label())\n\
             \x20 for e in [m.OptionError('m', 's', 'o')]]",
            "[(e.message, e.section, e.label()) for e in [m.Silent('m', 's')]]",
            "[c.__name__ for c in m.OptionError.__mro__]",
            // a field a base declared is written through the base's own storage, so a
            // write on one rung has to be what the other rungs read back
            "[(e.message, (setattr(e, 'message', 'w'), e.message)[1], e.label())\n\
             \x20 for e in [m.OptionError('m', 's', 'o')]]",
            "(isinstance(m.OptionError('m', 's', 'o'), m.Error),\n\
             \x20isinstance(m.SectionError('m', 's'), Exception),\n\
             \x20issubclass(m.OptionError, m.SectionError))",
            // raising and catching one, which is what the family exists for
            "[(type(e).__name__, e.section, e.option)\n\
             \x20 for e in [_raised_and_caught(m.OptionError, 'm', 's', 'o')]]",
            // python's own subclass of a rung, built by the class statement rather than
            // here — its deallocation chains through every compiled rung below it
            "[(s('m', 's', 'o').option, s('m', 's', 'o').label())\n\
             \x20 for s in [type('Py', (m.OptionError,), {})]]",
            // every instance holds a cycle through a field of the *innermost* rung, so
            // the collector has to reach past two appended regions to see it
            "(len([e for e in [m.OptionError('m', 's', str(i)) for i in range(200)]\n\
             \x20     if setattr(e, 'option', e) is None]), __import__('gc').collect() >= 0)",
        ],
    );
}

/// the same chain, built and dropped in bulk. this is the shape whose deallocation goes
/// wrong loudly: a rung that resolved the base to chain to from `Py_TYPE(self)` would
/// find its own deallocator and call it back until the stack ran out, and one that
/// released the wrong region would free somebody else's object
#[test]
fn a_chain_of_appended_storage_deallocates_without_growing() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_appendchain_t");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        APPENDED_CHAIN,
        "by_diff_appendchain_t",
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
        "import gc, by_diff_appendchain_t as m\n\
         # `method_descriptor` against `function` is what says the compiled type\n\
         # answered — the interpreted definition answers every question below the same\n\
         print(type(m.SectionError.__dict__['label']).__name__)\n\
         # each rung keeps a region past the one below, so each instance is larger\n\
         print(m.Error.__basicsize__ < m.SectionError.__basicsize__\n\
         \x20     < m.OptionError.__basicsize__)\n\
         # a method of one rung reading a field another rung's constructor wrote: two\n\
         # copies of a field would leave one of them at whatever `tp_alloc` zeroed\n\
         print(m.OptionError('m', 's', 'o').label())\n\
         for _ in range(200): m.OptionError('m', 's', 'o')\n\
         gc.collect(); before = len(gc.get_objects())\n\
         for _ in range(20000):\n\
         \x20   e = m.OptionError('m', 's', ['o'])\n\
         \x20   e.option = e\n\
         \x20   del e\n\
         gc.collect(); after = len(gc.get_objects())\n\
         print('stable' if after <= before + 200 else f'grew {before}->{after}')\n\
         # and a deep chain of them raised and caught, which is the deallocation the\n\
         # recursion would show up in\n\
         for _ in range(20000):\n\
         \x20   try: raise m.OptionError('m', 's', 'o')\n\
         \x20   except m.Error: pass\n\
         print('caught')\n",
    );
    assert_eq!(out, "method_descriptor\nTrue\nsection:s:m\nstable\ncaught");
}

/// a chain whose lower rungs hold nothing at all, which is the stdlib's commonest
/// exception family: `tarfile` writes `TarError`, `FilterError` and then five classes
/// with a field each, and `contextlib`, `selectors` and `_pyio` all repeat the shape
///
/// a rung holding nothing is still a rung the deallocation passes through, and a `class`
/// statement's type there carries `subtype_dealloc` — which reads the deallocator to
/// chain to out of `Py_TYPE(self)`, finds the appending class's own, and calls it back
/// until the stack runs out. so the hollow rungs are built from specs of their own too,
/// and given the three slots with nothing in them.
///
/// `Held` is the harder half: it appends its storage past a hollow rung, but the field
/// its constructor writes through `Base.__init__` lives two rungs further down, in
/// `Base`'s own region. two copies of that field would leave whichever one the method
/// reads at whatever `tp_alloc` zeroed
const HOLLOW_CHAIN: &str = "\
class TarError(Exception):
    pass


class FilterError(TarError):
    pass


class AbsolutePathError(FilterError):
    def __init__(self, name):
        FilterError.__init__(self, 'absolute')
        self.name = name

    def label(self):
        return 'absolute:' + self.name


class LinkOutsideError(AbsolutePathError):
    def __init__(self, name, link):
        AbsolutePathError.__init__(self, name)
        self.link = link

    def label(self):
        return 'outside:' + self.name + ':' + self.link


class Base(Exception):
    def __init__(self, message):
        Exception.__init__(self, message)
        self.message = message

    def label(self):
        return 'base:' + self.message


class Hollow(Base):
    pass


class Held(Hollow):
    def __init__(self, message, code):
        Base.__init__(self, message)
        self.code = code

    def label(self):
        return 'held:' + self.message + ':' + str(self.code)
";

#[test]
fn a_chain_over_a_base_that_holds_nothing_agrees() {
    agree_python(
        "hollowchain",
        HOLLOW_CHAIN,
        &[
            "[(e.name, e.label(), e.args) for e in [m.AbsolutePathError('n')]]",
            "[(e.name, e.link, e.label()) for e in [m.LinkOutsideError('n', 'l')]]",
            "[(e.message, e.code, e.label()) for e in [m.Held('m', 7)]]",
            "[(e.message, e.label()) for e in [m.Base('m')]]",
            "[c.__name__ for c in m.LinkOutsideError.__mro__]",
            "(m.TarError('t').args, m.FilterError('f').args, m.Hollow('h').args)",
            // a field a rung two below declared, written and read back through this one
            "[(e.message, (setattr(e, 'message', 'w'), e.message)[1], e.label())\n\
             \x20 for e in [m.Held('m', 7)]]",
            "(isinstance(m.LinkOutsideError('n', 'l'), m.TarError),\n\
             \x20isinstance(m.AbsolutePathError('n'), Exception),\n\
             \x20isinstance(m.Held('m', 1), m.Base), issubclass(m.Hollow, m.Base))",
            "[(type(e).__name__, e.name, e.link)\n\
             \x20 for e in [_raised_and_caught(m.LinkOutsideError, 'n', 'l')]]",
            "[(type(e).__name__, e.args) for e in [_raised_and_caught(m.FilterError, 'f')]]",
            // python's own subclass of a hollow rung, whose deallocation goes through
            // every compiled rung below it
            "[(s('f').args, type(s('f')).__name__) for s in [type('Py', (m.FilterError,), {})]]",
            // every instance holds a cycle through the appended field, so the collector
            // has to reach past the hollow rungs to see it
            "(len([e for e in [m.LinkOutsideError([i], 'l') for i in range(200)]\n\
             \x20     if setattr(e, 'name', e) is None]), __import__('gc').collect() >= 0)",
        ],
    );
}

/// the same chain, built and dropped in bulk. a hollow rung left to its interpreted
/// definition is where this goes wrong loudly: the appending class's deallocator would
/// call `subtype_dealloc`, which would find that same deallocator through `Py_TYPE(self)`
/// and call it straight back until the stack ran out
#[test]
fn a_chain_over_a_base_that_holds_nothing_deallocates_without_growing() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_hollowchain_t");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        HOLLOW_CHAIN,
        "by_diff_hollowchain_t",
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
        "import gc, by_diff_hollowchain_t as m\n\
         # `method_descriptor` against `function` is what says the compiled type\n\
         # answered — the interpreted definition answers every question below the same\n\
         print(type(m.AbsolutePathError.__dict__['label']).__name__,\n\
         \x20     type(m.Held.__dict__['label']).__name__)\n\
         # a hollow rung asks for no room of its own, and the rungs that do append grow\n\
         print(Exception.__basicsize__ == m.TarError.__basicsize__\n\
         \x20     == m.FilterError.__basicsize__\n\
         \x20     < m.AbsolutePathError.__basicsize__ < m.LinkOutsideError.__basicsize__)\n\
         print(m.Base.__basicsize__ == m.Hollow.__basicsize__ < m.Held.__basicsize__)\n\
         # a method reading a field a rung two below it wrote\n\
         print(m.Held('m', 7).label())\n\
         gc.collect(); before = len(gc.get_objects())\n\
         for _ in range(20000):\n\
         \x20   e = m.LinkOutsideError(['n'], 'l')\n\
         \x20   e.name = e\n\
         \x20   h = m.Held(['m'], 7)\n\
         \x20   h.code = h\n\
         \x20   q = m.FilterError('f')\n\
         \x20   del e, h, q\n\
         gc.collect(); after = len(gc.get_objects())\n\
         print('stable' if after <= before + 200 else f'grew {before}->{after}')\n\
         # and a deep chain of them raised and caught, which is the deallocation the\n\
         # recursion would show up in\n\
         for _ in range(20000):\n\
         \x20   try: raise m.LinkOutsideError('n', 'l')\n\
         \x20   except m.TarError: pass\n\
         print('caught')\n",
    );
    assert_eq!(
        out,
        "method_descriptor method_descriptor\n\
         True\nTrue\nheld:m:7\nstable\ncaught"
    );
}

/// storage appended past an `OSError`, which is `smtplib`'s whole exception family
///
/// the release takes the instance off the collector's list, and it has to go straight back
/// on before the base is handed it: `OSError`'s deallocator takes it off again without
/// checking, and unlinking an object that is already unlinked corrupts the list. that was
/// a segfault at the very first deallocation, and `Exception` hid it — its deallocator
/// checks, so every test written over one passed
const OSERROR_FAMILY: &str = "\
class Failure(OSError):
    pass


class Refused(Failure):
    def __init__(self, code, note):
        Failure.__init__(self, note)
        self.code = code
        self.note = note

    def label(self):
        return 'refused:' + str(self.code) + ':' + self.note


class Direct(OSError):
    def __init__(self, code):
        OSError.__init__(self, code)
        self.code = code
";

#[test]
fn storage_appended_past_an_oserror_agrees() {
    agree_python(
        "oserrfamily",
        OSERROR_FAMILY,
        &[
            "[(e.code, e.note, e.label(), e.args) for e in [m.Refused(4, 'n')]]",
            "[(e.code, e.args) for e in [m.Direct(7)]]",
            "(m.Failure('f').args, [c.__name__ for c in m.Refused.__mro__])",
            "(isinstance(m.Refused(1, 'n'), OSError), issubclass(m.Direct, OSError))",
            "[(type(e).__name__, e.code) for e in [_raised_and_caught(m.Refused, 2, 'n')]]",
            // the deallocation that corrupted the collector's list, in bulk
            "(len([e for e in [m.Refused(i, 'n') for i in range(2000)]]),\n\
             \x20len([e for e in [m.Direct(i) for i in range(2000)]]),\n\
             \x20len([e for e in [m.Failure('f') for i in range(2000)]]),\n\
             \x20__import__('gc').collect() >= 0)",
        ],
    );
}

/// a `__del__` anywhere in such a chain turns the class that writes it down, because the
/// finalizer is reached from the deallocator of whichever class owns the instance layout
/// — and here that is the outside base, whose deallocator does not call one. so the
/// hollow rung keeps its interpreted definition, and the class appending storage past it
/// has no base of ours to chain to and keeps its own
#[test]
fn a_finalizer_on_a_hollow_rung_declines_the_chain() {
    let source = "\
class Root(Exception):
    pass


class Hollow(Root):
    def __del__(self):
        _seen.append('gone')


class Held(Hollow):
    def __init__(self, code):
        Hollow.__init__(self, 'held')
        self.code = code


_seen = []


def seen():
    return list(_seen)
";
    agree_python_with_declines(
        "hollowfinal",
        source,
        &[
            "[(m.Held(1).code, m.seen()) for _ in [0]]",
            "[(len([m.Held(i) for i in range(50)]), __import__('gc').collect() >= 0,\n\
             \x20  len(m.seen())) for _ in [0]]",
        ],
    );
}

#[test]
fn a_subclass_with_no_storage_stands_on_a_base_that_declined_later() {
    // a base is settled as one of ours while the layouts settle, and only the body being
    // lowered can turn it down after that — here a `__new__`, which fills a type slot
    // with no adapter. a class with no storage of its own does not need the base to have
    // stayed one: what stands under the name at import is a class either way, so it is
    // built on the *name*, which is the construction every class over an outside base
    // already takes.
    //
    // `method_descriptor` against `function` is what says which type answered: `Plain`
    // and `Deeper` are compiled types standing on the interpreted `TokenList`
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_lostbase");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class TokenList(list):

    token_type = None

    def __new__(cls, *args):
        return super().__new__(cls, *args)

    def __init__(self, *args):
        super().__init__(*args)
        self.defects = []

    def kind(self):
        return self.token_type


class Plain(TokenList):
    token_type = 'plain'

    def side(self):
        return 'side:' + str(self.token_type)


class Deeper(Plain):
    token_type = 'deeper'
";
    let built = match build_source(
        source,
        "by_diff_lostbase",
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
            "TokenList",
            "a `__new__` allocates, and this class takes its instance layout from a base outside this module — only that base knows how big one is"
        )]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_lostbase as m\n\
         print([c.__name__ for c in m.Deeper.__mro__])\n\
         print(type(m.Plain.__dict__['side']).__name__,\n\
         \x20     type(m.TokenList.__dict__['kind']).__name__)\n\
         print(list(m.Plain([1, 2])), m.Plain([1, 2]).defects, m.Plain([1]).side())\n\
         print(m.Deeper([3]).kind(), m.Deeper([3]).side(), isinstance(m.Deeper([3]), m.TokenList))\n",
    );
    assert_eq!(
        out,
        "['Deeper', 'Plain', 'TokenList', 'list', 'object']\n\
         method_descriptor function\n\
         [1, 2] [] side:plain\n\
         deeper side:deeper True"
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
    let dir = diff_root().join("by_diff_mixedmeta");
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

/// a class inside a package reports the whole dotted module it was written in
///
/// the test above asks this of a top-level module, where the module's own name and
/// its name inside its package are the same string — so it cannot see the
/// difference. cpython reads a type's `__module__` off the front of its `tp_name`
/// and its `__name__` off the back, and a compiled class that carried only its
/// file's stem named a module `sys.modules` has nothing under. `dataclasses` looks
/// exactly that up (`sys.modules.get(cls.__module__).__dict__`), so a package
/// member with a dataclass in it failed to import at all
#[test]
fn a_class_in_a_package_reports_the_package_it_came_from() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let source = "\
class Point:
    def __init__(self, x: int, y: int) -> None:
        self.x = x
        self.y = y

    def total(self) -> int:
        return self.x + self.y
";
    let base = diff_root().join("by_diff_pkgmodule");
    let _ = std::fs::remove_dir_all(&base);
    let compiled_root = base.join("c");
    let interpreted_root = base.join("i");
    let compiled = compiled_root.join("by_diff_pkg");
    let interpreted = interpreted_root.join("by_diff_pkg");
    for dir in [&compiled, &interpreted] {
        std::fs::create_dir_all(dir).expect("the package directory is created");
        std::fs::write(dir.join("__init__.py"), "").expect("the package marker is written");
    }
    std::fs::write(interpreted.join("member.py"), source)
        .expect("the interpreted module is written");

    // the *root* of the output tree, not the package directory: the build writes the
    // artefact at the module's own place within the tree, so handing it the package
    // directory would nest a second `by_diff_pkg` inside the first
    let built = match build_source(
        source,
        "by_diff_pkg.member",
        &toolchain,
        &compiled_root,
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

    let body = "\
import sys\n\
import by_diff_pkg.member as m\n\
print(m.Point.__module__, m.Point.__qualname__, m.Point.__name__)\n\
print(sys.modules[m.Point.__module__] is m)\n\
print(m.Point(2, 3).total())\n";
    let compiled_out = run(&python, &compiled_root, body);
    let interpreted_out = run(&python, &interpreted_root, body);
    assert_eq!(
        compiled_out, interpreted_out,
        "compiled {compiled_out}, interpreted {interpreted_out}"
    );
    assert_eq!(
        compiled_out, "by_diff_pkg.member Point Point\nTrue\n5",
        "the module python imported is the module the class names"
    );

    // …and it really was the emitted type that answered. a class that fell back to
    // its interpreted definition answers all of the above identically, and holds a
    // plain function where a type of ours holds a descriptor
    let descriptor = run(
        &python,
        &compiled_root,
        "import by_diff_pkg.member as m\n\
         print(type(m.Point.__dict__['total']).__name__)\n",
    );
    assert_eq!(descriptor, "method_descriptor");
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
    let dir = diff_root().join("by_diff_metafields");
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
            "a class with fields of its own needs `type` for every base's metaclass, and `ABC` has `ABCMeta`"
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

    // and the abstract-base machinery the metaclass carries is all still there, on the
    // class that declined and on the two beside it that compiled. a class built from a
    // type spec would have `type` for its metaclass and none of this: `register` is the
    // metaclass's method, `__abstractmethods__` is what it fills in, and `isinstance`
    // against a registered class goes through the `__subclasshook__` it installs
    let abc_surface = run(
        &python,
        &dir,
        "import by_diff_metafields as m\n\
         from abc import abstractmethod\n\
         class Unrelated:\n\
         \x20   pass\n\
         m.Fieldless.register(Unrelated)\n\
         m.Keyed.register(Unrelated)\n\
         print(isinstance(Unrelated(), m.Fieldless), isinstance(Unrelated(), m.Keyed))\n\
         print(m.Fielded.__abstractmethods__, m.Fieldless.__abstractmethods__)\n\
         class Abstract(m.Fieldless):\n\
         \x20   @abstractmethod\n\
         \x20   def missing(self) -> int: ...\n\
         print(sorted(Abstract.__abstractmethods__))\n\
         try:\n\
         \x20   Abstract()\n\
         except TypeError as error:\n\
         \x20   print('refused', 'missing' in str(error))\n",
    );
    assert_eq!(
        abc_surface,
        "True True\n\
         frozenset() frozenset()\n\
         ['missing']\n\
         refused True"
    );
}

#[test]
fn a_metaclass_that_remakes_a_class_level_constant_is_turned_down_after_the_call() {
    // the constants go into the namespace the metaclass is handed, which is enough for a
    // metaclass that only *reads* one. an `EnumType` does not read them: it builds a
    // *member* out of `STRICT = auto()`, and the member is not the value the module body
    // already took a reference to. `FIRST is Boundary.STRICT` is what that costs, and no
    // amount of writing the namespace fixes it.
    //
    // so the class is asked afterwards whether it kept what it was handed, and where it
    // did not the interpreted definition stands. the two classes are the boundary: same
    // base, same metaclass, and only `Boundary` has a constant. `function` against
    // `method_descriptor` is what says the refusal is exactly that narrow — a guard that
    // never fired would make `Boundary` say `method_descriptor` and lose `FIRST`
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_metaconstant");
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
fn a_dunder_the_module_body_hangs_on_a_class_keeps_it_off_the_compiled_surface() {
    // `ctypes` writes `c_byte.__ctype_le__ = c_byte.__ctype_be__ = c_byte` under the class
    // statement, and the adoption that carries a twin's attributes across leaves every
    // dunder behind — a dunder is what a type slot answers, and a second answer sitting in
    // the dict would disagree with it. so the attribute has nowhere to land and the class
    // has to decline.
    //
    // the two classes are the two constructions: `Meta` can only be built by calling its
    // metaclass and `Spec` comes from a type spec, and the adoption is the same for both.
    // `plain` is the boundary in the other direction — a name that is not a dunder *is*
    // carried, and is not a reason to turn anything down
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_hungdunder");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
from abc import ABCMeta


class Meta(metaclass=ABCMeta):
    TAG = 1

    def label(self) -> str:
        return \"meta\"


class Spec:
    TAG = 2

    def label(self) -> str:
        return \"spec\"


class Untouched:
    TAG = 3

    def label(self) -> str:
        return \"untouched\"


Meta.__marker__ = Meta
Spec.__marker__ = Spec
Untouched.plain = 4
";
    let built = match build_source(
        source,
        "by_diff_hungdunder",
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
                "Meta",
                "the module body writes `__marker__` onto `Meta`, which the emitted type does not carry"
            ),
            (
                "Spec",
                "the module body writes `__marker__` onto `Spec`, which the emitted type does not carry"
            )
        ]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_hungdunder as m\n\
         print(m.Meta.__marker__ is m.Meta, m.Spec.__marker__ is m.Spec, m.Untouched.plain)\n\
         print(m.Meta().label(), m.Spec().label(), m.Untouched().label())\n\
         print(type(m.Meta.label).__name__, type(m.Spec.label).__name__,\n\
         \x20     type(m.Untouched.label).__name__)\n",
    );
    assert_eq!(
        out,
        "True True 4\n\
         meta spec untouched\n\
         function function method_descriptor"
    );
}

#[test]
fn a_constant_that_reads_back_differently_every_time_is_not_turned_down_for_it() {
    // a class-level constant is read out of the *mapping* the class body wrote rather
    // than through a lookup on the class, and `__class_getitem__ = classmethod(f)` is
    // where the two differ: a lookup runs the descriptor and answers a freshly bound
    // method, and one bound to the interpreted definition at that. copying that is what
    // used to make `Holder[int]` answer the twin instead of `Holder`, and it made the
    // check after the metaclass call — which compares the finished class against the
    // values it was handed — turn down every class with such a constant, since no two
    // reads are the same object. almost every container type in the stdlib has one.
    //
    // `Spec` is the boundary: both constructions carry the classmethod itself now, so
    // both answer the class the interpreter answers
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_metaunstable");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
from abc import ABCMeta


def _get(cls, item):
    return cls


class Holder(metaclass=ABCMeta):
    __class_getitem__ = classmethod(_get)

    def label(self) -> str:
        return \"holder\"


class Spec:
    __class_getitem__ = classmethod(_get)

    def label(self) -> str:
        return \"spec\"
";
    let built = match build_source(
        source,
        "by_diff_metaunstable",
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
        "import by_diff_metaunstable as m\n\
         print(m.Holder().label(), m.Spec().label())\n\
         print(type(m.Holder.label).__name__, type(m.Spec.label).__name__)\n\
         print(m.Holder[int] is m.Holder, m.Spec[int] is m.Spec)\n\
         print(m.Holder[int] is m.Spec[int])\n",
    );
    assert_eq!(
        out,
        "holder spec\n\
         method_descriptor method_descriptor\n\
         True True\n\
         False"
    );
}

#[test]
fn a_metaclass_that_raises_on_the_namespace_it_is_handed_leaves_the_import_standing() {
    // `ssl.Purpose`'s shape, and the one thing worse than a wrong answer: `EnumType` is
    // handed a namespace whose members are the twin's *finished* ones and tries to build a
    // member out of a member — `Obj.__new__(cls, a, b)` against a `__new__` that takes one
    // argument. that raises before the check after the call could turn the class down, and
    // propagating it fails `by_exec` and takes the whole import with it.
    //
    // the interpreted definition already built this class — the fallback source ran first —
    // so the raise says the reconstruction is wrong, not that the class is unbuildable. it
    // is the same refusal the check makes, reached earlier. `Plain` is the boundary: the
    // same metaclass with a namespace it can work with still reaches the compiled type
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_metaraise");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
from collections import namedtuple
from enum import Enum


class Obj(namedtuple(\"Obj\", \"a b\")):
    __slots__ = ()

    def __new__(cls, text):
        return super().__new__(cls, text, text.upper())


class Kind(Obj, Enum):
    ONE = \"one\"
    TWO = \"two\"


class Plain(Enum):
    def shout(self) -> str:
        return \"plain\"


FIRST = Kind.ONE
";
    let built = match build_source(
        source,
        "by_diff_metaraise",
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
    // `Obj` names a call as its base, which is a decline of its own and not this one
    assert_eq!(
        built
            .declined
            .iter()
            .map(|declined| declined.name.as_str())
            .collect::<Vec<_>>(),
        ["Obj"]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_metaraise as m\n\
         print(m.Kind.ONE.a, m.Kind.ONE.b, m.FIRST is m.Kind.ONE)\n\
         print(isinstance(m.FIRST, m.Kind), m.Kind(m.Kind.ONE.value) is m.FIRST)\n\
         print(m.Kind._member_names_, type(m.Kind).__name__)\n\
         print(type(m.Plain.shout).__name__)\n",
    );
    assert_eq!(
        out,
        "one ONE True\n\
         True True\n\
         ['ONE', 'TWO'] EnumType\n\
         method_descriptor"
    );
}

#[test]
fn a_class_level_constant_beside_a_base_of_ours_reaches_the_metaclass_namespace() {
    // the shape 69 of the stdlib's 84 instances of the old constant decline had: no class
    // keyword anywhere, a base this module emits standing beside one from outside, and a
    // constant. a spec cannot work that base list out, so the metaclass builds it — and
    // the constant goes into the namespace it is handed rather than onto the type
    // afterwards, which is what makes that construction answer for such a class at all.
    //
    // `issubclass` is what the decline used to be protecting: with `Reader` interpreted
    // and `Codec` emitted it answers False where python answers True, so the two had to
    // decline together. both are compiled here, and `method_descriptor` against
    // `function` is what says so — `Alone` is the boundary that never needed the base
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_metaconstant_base");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
import codecs


class Codec(codecs.Codec):
    def label(self) -> str:
        return \"codec\"


class Reader(Codec, codecs.StreamReader):
    tag = 1

    def kind(self) -> str:
        return \"reader\"


class Alone(codecs.Codec):
    tag = 2

    def kind(self) -> str:
        return \"alone\"
";
    let built = match build_source(
        source,
        "by_diff_metaconstant_base",
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
    assert_eq!(declined, Vec::new());
    let out = run(
        &python,
        &dir,
        "import by_diff_metaconstant_base as m\n\
         print(issubclass(m.Reader, m.Codec), m.Reader.tag, m.Alone.tag)\n\
         print([c.__name__ for c in m.Reader.__mro__])\n\
         print(type(m.Codec.label).__name__, type(m.Reader.kind).__name__,\n\
         \x20     type(m.Alone.kind).__name__)\n",
    );
    assert_eq!(
        out,
        "True 1 2\n\
         ['Reader', 'Codec', 'StreamReader', 'Codec', 'object']\n\
         method_descriptor method_descriptor method_descriptor"
    );
}

#[test]
fn a_conditional_in_a_class_body_carries_across_whichever_leg_ran() {
    // `pickle._Pickler` is the shape: a `def` and a table write behind a module flag.
    // nothing in the compiler evaluates the flag — the interpreted definition ran the
    // block, and every name it could have bound is copied off the namespace it left.
    //
    // so this asserts the things that copy has to get right. the condition runs *once*,
    // in body order, and its effect stands: `seen` is the record. a name a false leg
    // never wrote is absent, which `hasattr` is the observable form of. an `if`/`else`
    // where both legs write one name carries whichever leg python took, rather than
    // declining as the rebinding it is not. and a write the block makes into something
    // that is not the class namespace — `table[1]`, `box.n` — leaves no name behind and
    // is already done by the time init reads that namespace.
    //
    // `kept` is the prize and `method_descriptor` is what says it was won: a class that
    // used to decline whole now compiles every method the block does not hold, and only
    // the block's own stay interpreted
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_class_conditional");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Box:
    n: int = 0


seen = []
box = Box()


def flag(name: str, value: bool) -> bool:
    seen.append(name)
    return value


class Guarded:
    table = {}

    def kept(self, n: int) -> int:
        return n + 1
    if flag(\"on\", True):
        def on_leg(self) -> int:
            return 2
        alias = 3
        table[1] = alias
        box.n = 7
    if flag(\"off\", False):
        def off_leg(self) -> int:
            return 4
        missing = 5
    if flag(\"pick\", False):
        def picked(self) -> str:
            return \"then\"
    else:
        def picked(self) -> str:
            return \"else\"
";
    let built = match build_source(
        source,
        "by_diff_class_conditional",
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
    assert_eq!(declined, Vec::new());
    let out = run(
        &python,
        &dir,
        "import by_diff_class_conditional as m\n\
         print(m.seen)\n\
         print(hasattr(m.Guarded, 'on_leg'), hasattr(m.Guarded, 'off_leg'))\n\
         print(hasattr(m.Guarded, 'alias'), hasattr(m.Guarded, 'missing'))\n\
         g = m.Guarded()\n\
         print(g.kept(1), g.on_leg(), g.picked(), m.Guarded.alias, m.Guarded.table)\n\
         print(m.box.n, hasattr(m.Guarded, 'box'))\n\
         print(type(m.Guarded.kept).__name__, type(m.Guarded.on_leg).__name__)\n",
    );
    assert_eq!(
        out,
        "['on', 'off', 'pick']\n\
         True False\n\
         True False\n\
         2 2 else 3 {1: 3}\n\
         7 False\n\
         method_descriptor function"
    );
}

#[test]
fn the_shapes_a_class_body_block_is_not_lowered_for_decline() {
    // three the copy cannot answer for, and the module still runs from the interpreted
    // definitions the decline leaves standing.
    //
    // a `def` beside a block that binds the same name is two definitions of one
    // attribute, and only the running interpreter knows which python kept. a dunder is
    // settled from the body text — `__slots__` decides an instance layout before any
    // block has run. and a `try` binds names this does not model, so it declines by
    // name rather than being walked past with its bindings missed
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_class_conditional_declines");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
on = True


class Twice:
    def load(self, n: int) -> int:
        return n
    if on:
        def load(self, n: int) -> int:
            return n + 1


class Dunder:
    n = 0
    if on:
        def __repr__(self) -> str:
            return \"dunder\"


class Caught:
    try:
        n = 1
    except ValueError:
        n = 2
";
    let built = match build_source(
        source,
        "by_diff_class_conditional_declines",
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
        .filter(|(name, _)| matches!(*name, "Twice" | "Dunder" | "Caught"))
        .collect();
    declined.sort_unstable();
    assert_eq!(
        declined,
        vec![
            ("Caught", "only fields and methods are lowered yet"),
            (
                "Dunder",
                "`__repr__` is bound by a block nested in the class body, and a dunder is settled before one runs",
            ),
            (
                "Twice",
                "`load` is both defined by this class body and bound by a block nested in it",
            ),
        ]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_class_conditional_declines as m\n\
         print(m.Twice().load(1), repr(m.Dunder()), m.Caught.n)\n\
         print(type(m.Twice.load).__name__)\n",
    );
    assert_eq!(
        out,
        "2 dunder 1\n\
         function"
    );
}

#[test]
fn a_slots_declaration_reaches_the_metaclass_rather_than_the_finished_type() {
    // `__slots__` is the constant that proves the namespace is where these have to go.
    // `type.__new__` reads it *out of the namespace* to decide whether the instances get
    // a dict at all, so one copied onto the finished type afterwards is not a `__slots__`
    // — the class already has the dict, and the entry sits there saying otherwise.
    //
    // 29 stdlib classes are this shape. `Open` is the boundary: the same base and the
    // same construction with no `__slots__`, and python gives *its* instances a dict, so
    // this is not a rule about emitted classes but about what the body wrote
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_metaslots");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
from abc import ABC


class Slotted(ABC):
    __slots__ = ()

    def label(self) -> str:
        return \"slotted\"


class Open(ABC):
    def label(self) -> str:
        return \"open\"
";
    let built = match build_source(
        source,
        "by_diff_metaslots",
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
        "import by_diff_metaslots as m\n\
         print(m.Slotted.__slots__, hasattr(m.Slotted(), '__dict__'), hasattr(m.Open(), '__dict__'))\n\
         print(m.Slotted().label(), m.Open().label())\n\
         print(type(m.Slotted.label).__name__, type(m.Open.label).__name__)\n",
    );
    assert_eq!(
        out,
        "() False True\n\
         slotted open\n\
         method_descriptor method_descriptor"
    );
}

#[test]
fn a_class_constant_naming_another_class_reaches_the_metaclass_namespace_remapped() {
    // the value a constant carries comes off the twin, so `pair = Other` in a class body
    // hands over the *interpreted* `Other` — a class nothing else in the module can
    // reach, and one `isinstance` denies against `m.Other`. the substitution that fixes
    // that is the copy's, and the namespace has to make the same one or the two
    // constructions disagree about what a constant is.
    //
    // `Below` is what forces the metaclass here: `ABCMeta` closes the spec, so the
    // constant goes in before the call rather than onto the type after it
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_metaconstant_remap");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
from abc import ABCMeta


class Other:
    def kind(self) -> str:
        return \"other\"


class Below(metaclass=ABCMeta):
    pair = Other

    def label(self) -> str:
        return \"below\"
";
    let built = match build_source(
        source,
        "by_diff_metaconstant_remap",
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
        "import by_diff_metaconstant_remap as m\n\
         print(m.Below.pair is m.Other, isinstance(m.Below.pair(), m.Other))\n\
         print(m.Below.pair().kind(), m.Below().label())\n\
         print(type(m.Below.label).__name__, type(m.Other.kind).__name__)\n",
    );
    assert_eq!(
        out,
        "True True\n\
         other below\n\
         method_descriptor method_descriptor"
    );
}

#[test]
fn a_class_the_module_pops_out_of_its_own_globals_stays_off_the_compiled_surface() {
    // `ast` builds `Num` and then pops the name straight out of its own globals. that is
    // a `del` whose target this cannot read — the name comes off a comprehension there —
    // so every definition the module writes is treated as one the pop could have taken.
    // installing a compiled `Gone` over a name the body removed would put a class on the
    // surface python does not have there, and the interpreted definition the construction
    // would otherwise fall back to is not there to be found either.
    //
    // the class-level-constant gate used to carry this, and this is what stayed behind
    // when it went. `Kept` is no longer a boundary — the rule reaches the whole module,
    // which is what its second decline says
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_poppedclass");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
from abc import ABCMeta


class Gone(metaclass=ABCMeta):
    TAG = 1

    def label(self) -> str:
        return \"gone\"


class Kept:
    def label(self) -> str:
        return \"kept\"


HIDDEN = {name: globals().pop(name) for name in (\"Gone\",)}
";
    let built = match build_source(
        source,
        "by_diff_poppedclass",
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
                "Gone",
                "`Gone` is rebound at module level, so installing this over it would replace what the rebind produced"
            ),
            (
                "Kept",
                "`Kept` is rebound at module level, so installing this over it would replace what the rebind produced"
            )
        ]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_poppedclass as m\n\
         print('Gone' in m.__dict__, 'Kept' in m.__dict__)\n\
         print(m.HIDDEN['Gone'].TAG, m.HIDDEN['Gone']().label(), m.Kept().label())\n\
         print(type(m.HIDDEN['Gone'].label).__name__, type(m.Kept.label).__name__)\n",
    );
    assert_eq!(
        out,
        "False True\n\
         1 gone kept\n\
         function function"
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
    let dir = diff_root().join("by_diff_annotatedconstant");
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
fn a_late_gift_that_could_hand_the_interpreted_class_back_is_moved_onto_the_type() {
    // carrying an attribute across is only sound where the value cannot answer with the
    // interpreted definition, which is about to stop being the class under its name. a
    // value that *is* one is replaced by the type standing in for it, a container the
    // module still owns is settled — every twin inside it moved onto its replacement — and
    // only a shape with no route to its contents stays absent, which is the loud failure
    // the attribute already gave rather than a quiet wrong answer.
    //
    // so `MARKER`, `PAIR`, `shout`, `ITEMS` (a list of nothing that could be a class) and
    // `HIDDEN` (a tuple whose one entry is a twin, rebuilt around the type that replaced
    // it) all come across. `SAMPLE` is an *instance* of the interpreted class, and it comes
    // across as an instance of the type that replaced it: the object cannot be re-typed and
    // cannot be built again, so its state is moved onto one allocated with no constructor
    // running. that is what makes `type(Held.SAMPLE) is Other` hold, where the object the
    // body built answers a class nothing else in the module can reach.
    //
    // a dunder never comes across either — a name in the type's dict does not fill a type
    // slot, so `__ge__` there would answer `a.__ge__(b)` while `a >= b` still went to the
    // slot — and that is why a class the body hangs one on is turned down instead. `Ordered`
    // is that half: dropping the entry is as wrong an answer as carrying it would be.
    //
    // `method_descriptor` is what says the compiled type answered at all: a class that
    // fell back to its interpreted definition would carry every one of these, because it
    // *is* the object the module body wrote to
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_twinshapes");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Other:
    def tag(self) -> str:
        return \"other\"


class Held:
    def tag(self) -> str:
        return \"held\"


class Ordered:
    def tag(self) -> str:
        return \"ordered\"


Held.MARKER = 3
Held.PAIR = Other
Held.SAMPLE = Other()
Held.ITEMS = [1, 2]
Held.HIDDEN = (Other,)
Held.shout = lambda self: self.tag().upper()
Ordered.__ge__ = lambda self, right: True
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
    assert_eq!(
        built
            .declined
            .iter()
            .map(|declined| declined.name.as_str())
            .collect::<Vec<_>>(),
        ["Ordered"]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_twinshapes as m\n\
         print(m.Held.MARKER, m.Held.PAIR is m.Other, m.Held().shout())\n\
         print(type(m.Held.SAMPLE) is m.Other, m.Held.ITEMS, m.Held.HIDDEN[0] is m.Other)\n\
         print(m.Ordered() >= m.Ordered(), m.Ordered().tag())\n\
         print(type(m.Held.tag).__name__, type(m.Other.tag).__name__,\n\
         \x20     type(m.Ordered.tag).__name__)\n",
    );
    assert_eq!(
        out,
        "3 True HELD\n\
         True [1, 2] True\n\
         True ordered\n\
         method_descriptor method_descriptor function"
    );
}

/// an object several names hold is moved once, so they go on holding one object
///
/// this is the shape `logging` writes and the reason a move has to be remembered at all:
///
/// ```python
/// root = RootLogger(WARNING)
/// Logger.root = root
/// Logger.manager = Manager(Logger.root)
/// ```
///
/// the `Manager` captures the very object the class attribute holds. moving each reading
/// on its own would build a second root, leave `Logger.manager.root is Logger.root` False
/// and give the module two roots that drift apart from the first write — a fresh silent
/// wrong answer in place of the loud one the missing attribute gave.
#[test]
fn an_instance_several_names_hold_is_moved_once() {
    // `leaf` is reached four ways: under its own module-level name, under an alias the
    // body bound, as a class attribute, and as a field of another moved instance. all four
    // have to answer the same object, and it has to be an instance of the *emitted* `Leaf`
    // rather than of the definition the body built it with.
    //
    // `wrapper_descriptor` is what says the compiled type answered: a class that fell back
    // to its interpreted definition would hold every one of these already, because it *is*
    // the object the module body wrote to
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_twinshared");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Leaf:
    def __init__(self, tag: str) -> None:
        self.tag = tag


class Holder:
    def __init__(self, leaf: Leaf) -> None:
        self.leaf = leaf


leaf = Leaf(\"one\")
alias = leaf
Holder.shared = leaf
Holder.wrapper = Holder(leaf)
";
    let built = match build_source(
        source,
        "by_diff_twinshared",
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
        "import by_diff_twinshared as m\n\
         print(type(m.Holder.shared) is m.Leaf, m.Holder.shared.tag)\n\
         print(m.Holder.wrapper.leaf is m.Holder.shared, m.leaf is m.Holder.shared,\n\
         \x20     m.alias is m.Holder.shared)\n\
         print(type(m.Holder.wrapper) is m.Holder, type(m.Leaf.__init__).__name__)\n",
    );
    assert_eq!(
        out,
        "True one\n\
         True True True\n\
         True wrapper_descriptor"
    );
}

/// an instance the emitted layout cannot hold stays where the module body built it
///
/// the move is decided before it is begun, because a half-filled instance would be the
/// worst answer of the three available. a field the layout treats as always defined has no
/// presence byte and no check at any read, so one left unwritten does not raise — an
/// unwritten tagged integer reads back as `0`. so an instance that cannot be moved
/// *completely* is not moved at all, and the attribute goes on being absent, which is the
/// loud failure it already gave.
#[test]
fn an_instance_the_layout_cannot_hold_is_left_where_the_body_built_it() {
    // three refusals, one for each thing the layout has no room for.
    //
    // `spare` carries an attribute nothing declared, so the emitted instance would answer
    // the layout's fields and quietly lose `extra`. `bare` was built through `__new__` and
    // never ran `__init__`, so the field the layout treats as always defined was never
    // written and there is nothing to move onto it. `raised` is an instance of a class
    // standing on a base python allocates, and whatever `Exception` keeps for it lives in
    // a part of the object nothing here can read back.
    //
    // both classes still compile — the refusal is about one value, not about the class —
    // and `wrapper_descriptor`/`method_descriptor` is what says so
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_twinunmoved");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Loose:
    def __init__(self) -> None:
        self.a = 1


class Tagged(Exception):
    def tag(self) -> str:
        return \"tagged\"


spare = Loose()
spare.extra = 2
Loose.spare = spare

bare = Loose.__new__(Loose)
Loose.bare = bare

raised = Tagged(\"boom\")
Tagged.raised = raised
";
    let built = match build_source(
        source,
        "by_diff_twinunmoved",
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
        "import by_diff_twinunmoved as m\n\
         print(hasattr(m.Loose, 'spare'), hasattr(m.Loose, 'bare'),\n\
         \x20     hasattr(m.Tagged, 'raised'))\n\
         print(type(m.spare) is m.Loose, type(m.bare) is m.Loose,\n\
         \x20     type(m.raised) is m.Tagged)\n\
         print(type(m.Loose.__init__).__name__, type(m.Tagged.tag).__name__)\n",
    );
    assert_eq!(
        out,
        "False False False\n\
         False False False\n\
         wrapper_descriptor method_descriptor"
    );
}

/// a frozen class needs no rule of its own, because its own setter is the rule
///
/// the move writes each field through the type's setter, so that a value takes the same
/// conversion an assignment from python would. a frozen class publishes none, and that is
/// the whole answer for it: one with fields refuses at the first of them, and one with no
/// fields has nothing to lose and moves. the alternative — turning every immutable class
/// down up front — would cost the second case for nothing.
#[test]
fn a_frozen_instance_moves_only_where_it_has_no_field_to_fill() {
    // `Fixed.origin` is absent because `n` has no setter to write it through, and absent is
    // the right answer: an emitted instance with `n` unwritten would answer `0`, which is
    // the quiet wrong answer the whole move is arranged to avoid. `Blank.nothing` moves,
    // because a frozen class with no fields is entirely its type
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_twinfrozen");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
frozen data class Fixed:
    n: int


frozen data class Blank:
    pass


origin = Fixed(0)
Fixed.origin = origin

nothing = Blank()
Blank.nothing = nothing
";
    let built = match build_source(
        source,
        "by_diff_twinfrozen",
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
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_twinfrozen as m\n\
         print(hasattr(m.Fixed, 'origin'), type(m.origin) is m.Fixed)\n\
         print(type(m.Blank.nothing) is m.Blank, m.nothing is m.Blank.nothing)\n",
    );
    assert_eq!(
        out,
        "False False\n\
         True True"
    );
}

/// a module-level *function* has an interpreted twin too, and it is deliberately left
/// where it stands
///
/// the same staleness a class has: the module body runs against the interpreted
/// definitions, so everything it captured holds the `def`'s own function object, while
/// `PyModule_AddFunctions` puts the compiled `PyCFunction` under the name at the end of
/// init. `ALIAS is fn` is then False where python says True.
///
/// the class fix does not transfer, and this pins that it has not been made to. a class
/// twin is *incompatible* with the type that replaced it — `isinstance` denies it and a
/// compiled method refuses its instances — so a reference still holding one is already
/// broken, and moving it repairs damage. a function twin is **interchangeable** with the
/// compiled function for every use except identity: it computes the same answer, and
/// nothing rejects it. so moving one repairs nothing that was broken and breaks two
/// things that were not:
///
/// * a `function` in a class dict binds `self` and a `PyCFunction` does not, so the
///   moved reference stops being a method. `optparse` writes `class Option: __repr__ =
///   _repr` and `multiprocessing.reduction` writes `class AbstractReducer: dump = dump`
/// * `inspect.signature` works on a `function` and raises `ValueError` on a
///   `PyCFunction`, so a captured callback stops being introspectable
///
/// both turn a *right* answer into a raise, and a wrong answer is better than a crash.
/// over the stdlib corpus the captured references are overwhelmingly dispatch tables —
/// `copy._deepcopy_dispatch`, `shutil._ARCHIVE_FORMATS`, `xml.etree.ElementTree._serialize`
/// — whose entries are only ever *called*, so the divergence is unobservable there while
/// the repair would be plainly observable.
///
/// the two questions a remap would also have had to answer turn out to be answered
/// already, and neither needs runtime machinery: a function whose module-level name the
/// body rebinds is not `exported`, so an accelerator import (`asyncio.events` keeping
/// `_py_get_event_loop`, `operator`'s trailing `from _operator import *`) never produces
/// a twin at all; and a *decorated* module-level definition the module reads already
/// declines, because its decorator cannot run where the `def` stands and again over the
/// compiled one
#[test]
fn a_class_attribute_naming_a_module_function_keeps_the_definition_that_binds() {
    // `multiprocessing.reduction` writes `class AbstractReducer: dump = dump`, and this is
    // that: a class body binding a name to a module-level function that goes on to
    // compile. the value the emitted type carries is the *twin*, and it has to be — a
    // `PyCFunction` in a class dict is not a descriptor, so `Reducer().dump()` would call
    // `_dump` with no `self` at all.
    //
    // `function` against `builtin_function_or_method` is the whole assertion. the class
    // answers the same *value* either way, so nothing but the type of what sits in the
    // slot says which definition is standing there
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_fntwinbind");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def _dump(this: object) -> str:
    return \"dumped\"


class Reducer:
    dump = _dump

    def kind(self) -> str:
        return \"reducer\"
";
    let built = match build_source(
        source,
        "by_diff_fntwinbind",
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
        "import by_diff_fntwinbind as m\n\
         print(m.Reducer().dump(), m.Reducer().kind())\n\
         print(type(m._dump).__name__, type(m.Reducer.__dict__['dump']).__name__)\n\
         print(type(m.Reducer.kind).__name__)\n",
    );
    // `method_descriptor` says the emitted type answered rather than a class that fell
    // back to its interpreted definition — which would carry the slot for the other reason
    assert_eq!(
        out,
        "dumped reducer\n\
         builtin_function_or_method function\n\
         method_descriptor"
    );
}

/// the same, for the slot a *declined* class keeps and for a dunder
///
/// `optparse` writes `class Option: __repr__ = _repr`, and over the corpus that is a
/// captured twin — the compiled `optparse` keeps `Option` interpreted and its `__repr__`
/// holds the definition the module's own `_repr` no longer names. the alias remap walks
/// exactly that dict, so it is the second route a function substitution would take into a
/// descriptor position, and `repr()` is where it would show: python looks a dunder up on
/// the type and calls what it finds, and what it finds has to bind
#[test]
fn a_declined_class_keeps_the_dunder_slot_a_module_function_filled() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_fntwindunder");
    let _ = std::fs::remove_dir_all(&dir);
    // the late gift is the decline lever, and a *dunder* one is what turns the class down
    // rather than having its attributes adopted — see the twin-shapes test above. it is
    // here to put `Option` on the interpreted leg, which is where `optparse` has it
    let source = "\
def _repr(this: object) -> str:
    return \"<option>\"


class Option:
    __repr__ = _repr

    def kind(self) -> str:
        return \"option\"


Option.__ge__ = lambda self, other: True
";
    let built = match build_source(
        source,
        "by_diff_fntwindunder",
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
    assert_eq!(
        built
            .declined
            .iter()
            .map(|declined| declined.name.as_str())
            .collect::<Vec<_>>(),
        ["Option"]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_fntwindunder as m\n\
         print(repr(m.Option()), m.Option().kind())\n\
         print(type(m._repr).__name__, type(m.Option.__dict__['__repr__']).__name__)\n\
         print(type(m.Option.kind).__name__)\n",
    );
    // `function` on the last line is the class itself confirming it stayed interpreted,
    // which is the only state in which this slot exists to be got wrong
    assert_eq!(
        out,
        "<option> option\n\
         builtin_function_or_method function\n\
         function"
    );
}

/// a module-level container's function entries reach a class dict one step later, so
/// they are left where they stand too
///
/// `functools` is the case: `_convert` is a module-level dict of module-level functions
/// and `total_ordering` `setattr`s them onto the class it decorates. over the corpus all
/// twelve of its entries are captured twins. a remap that moved a container's entries
/// would put a `PyCFunction` in `__gt__` on every `@total_ordering` class in the process,
/// and the comparison would call it without the operands it binds
#[test]
fn a_container_entry_a_decorator_later_installs_on_a_class_keeps_binding() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_fntwinconvert");
    let _ = std::fs::remove_dir_all(&dir);
    // the late dunder gift turns `Ordered` down, which is the only state `total_ordering`
    // can act on at all: an emitted type is a *static* type and refuses `setattr`
    // outright with `cannot set '__gt__' attribute of immutable type`. so this is also
    // the shape of the real thing — a `@total_ordering` class that reaches a compiled
    // module is an interpreted one
    let source = "\
def _gt_from_lt(this: object, other: object) -> str:
    return \"gt\"


_convert = {\"__gt__\": _gt_from_lt}


def total_ordering(cls: type) -> type:
    setattr(cls, \"__gt__\", _convert[\"__gt__\"])
    return cls


class Ordered:
    def kind(self) -> str:
        return \"ordered\"


Ordered.__le__ = lambda self, other: True
";
    let built = match build_source(
        source,
        "by_diff_fntwinconvert",
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
    assert_eq!(
        built
            .declined
            .iter()
            .map(|declined| declined.name.as_str())
            .collect::<Vec<_>>(),
        ["Ordered"]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_fntwinconvert as m\n\
         m.total_ordering(m.Ordered)\n\
         print(m.Ordered() > m.Ordered())\n\
         print(type(m._gt_from_lt).__name__, type(m._convert['__gt__']).__name__)\n\
         print(type(m.Ordered.__dict__['__gt__']).__name__)\n",
    );
    // the module's name answers the compiled function while the table keeps the twin,
    // and it is the twin's being a descriptor that makes the installed comparison work
    assert_eq!(
        out,
        "gt\n\
         builtin_function_or_method function\n\
         function"
    );
}

/// and this is what leaving it costs, written down rather than left to be rediscovered
///
/// every module-level name the body bound to a compiled function keeps the interpreted
/// definition, so an identity test against the function's own name answers False where
/// python answers True. it is the whole observable surface of the decision above — a
/// captured function is otherwise interchangeable with the one that replaced it, because
/// calling either gives the same answer.
///
/// the test asserts the compiled answers directly rather than through `agree`, because
/// the point is precisely that the two legs differ here. it fails if the divergence is
/// ever closed, which is the reminder to come back and read why it was not
#[test]
fn identity_against_a_module_function_is_the_one_thing_a_captured_twin_gets_wrong() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_fntwinalias");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def proxy() -> int:
    return 7


ALIAS = proxy
TABLE = {\"held\": proxy}


def alias_is_proxy() -> bool:
    return ALIAS is proxy


def table_is_proxy() -> bool:
    return TABLE[\"held\"] is proxy


def alias_answers() -> int:
    return ALIAS()
";
    let built = match build_source(
        source,
        "by_diff_fntwinalias",
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
        "import by_diff_fntwinalias as m\n\
         print(m.alias_is_proxy(), m.table_is_proxy())\n\
         print(m.alias_answers(), m.ALIAS())\n\
         print(type(m.proxy).__name__, type(m.ALIAS).__name__)\n",
    );
    // python answers `True True` on the first line. the second is why that is tolerable:
    // the captured definition computes exactly what the compiled one computes, so the
    // divergence never reaches an answer — only an `is`
    assert_eq!(
        out,
        "False False\n\
         7 7\n\
         builtin_function_or_method function"
    );
}

#[test]
fn a_class_keeps_what_a_factory_installed_on_it_after_the_class_statement() {
    // `multiprocessing.managers` is the case this is drawn from. it defines `SyncManager`
    // and then the module body makes sixteen `SyncManager.register(...)` calls, each of
    // which closes over the proxy type it was handed, `setattr`s the closure onto the
    // class, and records the same in a `_registry` dict on it. all sixteen landed on the
    // interpreted definition and none of them on the emitted type, which carried neither
    // the methods nor the registry:
    //
    //     interpreted  Queue-on-class <function ...temp>   registry ['Array', ...]
    //     compiled     Queue-on-class MISSING              registry []
    //
    // the mechanism is not the ordering it looks like. what the body installs *is* seen —
    // a plain function with an empty closure comes across, which is why five earlier
    // attempts to shrink this to a small module all came back green. the two shapes that
    // did not come across are a function with a **non-empty closure** and a **dict**, and
    // both were refused wholesale by a rule that could only ask "might this reach a twin?"
    // and never move what it found. `installed` and `table` are those two shapes;
    // `direct` is the third, which always worked and is here to keep the distinction.
    //
    // `pinned` is the same staleness one level down: the closure holds the interpreted
    // `Other`, so carrying it verbatim would have handed back a class nothing else in the
    // module can name. it has to answer the type standing under `Other` instead.
    //
    // `registry` is the `_registry` dict's own shape, and it is here for its *depth*: a
    // dict of tuples, one entry of which is a function whose `incref=True` default sits
    // five levels below where the walk starts. the bound on that walk is there to stop the
    // recursion, and a value with nothing inside it is not a step into anything — so an
    // atom has to be answered before the bound rather than after it, or a `True` that far
    // down takes the whole dict with it. `len` is the other shape the entry needs: a
    // function written in C reaches nothing python chose but its own module
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_late_factory");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Other:
    def tag(self) -> str:
        return \"other\"


class Held:
    def tag(self) -> str:
        return \"held\"


def install(cls, name, held):
    def method(self):
        return held

    setattr(cls, name, method)
    cls.table = {name: held}


def direct(self):
    return \"direct\"


def proxy(token, incref=True):
    return token


install(Held, \"installed\", \"gift\")
install(Held, \"pinned\", Other)
Held.direct = direct
Held.registry = {\"held\": (Other, (\"get\", \"put\"), None, proxy, len)}
";
    let built = match build_source(
        source,
        "by_diff_late_factory",
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
    assert!(built.declined.is_empty(), "{:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_late_factory as m\n\
         print(m.Held().installed(), m.Held().direct())\n\
         print(m.Held().pinned() is m.Other, m.Held.table['pinned'] is m.Other)\n\
         entry = m.Held.registry['held']\n\
         print(entry[0] is m.Other, entry[1], entry[3](7), entry[4] is len)\n\
         print(type(m.Held.tag).__name__, type(m.Other.tag).__name__)\n",
    );
    assert_eq!(
        out,
        "gift direct\n\
         True True\n\
         True ('get', 'put') 7 True\n\
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
    let dir = diff_root().join("by_diff_annreach");
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
    let dir = diff_root().join("by_diff_annlost");
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
    let dir = diff_root().join("by_diff_anndunder");
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
fn a_subclass_of_a_class_the_metaclass_gate_turns_down_is_built_here() {
    // the metaclass gate is asked while the layouts settle, so a class it turns down
    // leaves the layout set — and its subclass is then laid out on the interpreted
    // definition the way every other declining class's subclass is. asked while the body
    // was lowered instead, the base stayed in the set and the subclass cascaded behind
    // it, which is what this decline used to be two of.
    //
    // `Constant` is the other half, and it is the boundary: a class-level constant no
    // longer turns a class down, so both it and its subclass are built here. the bases
    // carry `ABCMeta`, so `PyType_FromSpecWithBases` is closed to every class in this
    // module and the metaclass builds them — `method_descriptor` is what says that
    // construction happened at all, since a class that fell back would answer from a
    // `function`
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_gatedbase");
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
    // exactly the one base, and nothing behind it: a subclass in this list is the
    // cascade this move exists to stop
    assert_eq!(
        built
            .declined
            .iter()
            .map(|declined| declined.name.as_str())
            .collect::<Vec<_>>(),
        ["Decorated"]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_gatedbase as m\n\
         print(m.BelowDecorated().size(), m.BelowDecorated.label())\n\
         print(m.BelowConstant().size(), m.BelowConstant.TAG, m.BelowConstant().label())\n\
         print([b.__name__ for b in m.BelowConstant.__mro__])\n\
         print(isinstance(m.BelowConstant(), m.Constant), isinstance(m.BelowDecorated(), m.Decorated))\n\
         print(type(m.BelowDecorated.size).__name__, type(m.BelowConstant.size).__name__,\n\
         \x20     type(m.Constant.label).__name__)\n",
    );
    assert_eq!(
        out,
        "1 decorated\n\
         2 1 constant\n\
         ['BelowConstant', 'Constant', 'object']\n\
         True True\n\
         method_descriptor method_descriptor method_descriptor"
    );
}

#[test]
fn a_pair_the_body_cross_links_agrees_when_the_link_is_made_after_the_class_statement() {
    // `urllib.parse`'s shape, and the one that reverted this move the first time: `_pair`
    // runs against the twins, because the whole module body runs before module init
    // installs anything, and it hangs each result class off the other under a name no
    // class body wrote.
    //
    // what makes it agree is that an emitted type carries what the body gave its twin
    // *and* remaps a carried twin to the type standing in for it — without the remap
    // `Text._encoded_counterpart()` builds something `isinstance` says is not a
    // `m.Bytes`. the base used to decline at the class-level-constant gate and take the
    // whole chain with it; it is emitted now, which is what the empty decline list says
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_gatedpair");
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
    // `Mixin` used to decline and take `Root` and `Extra` down with it, because an
    // emitted class cannot have an interpreted subclass. none of the five is here now
    assert_eq!(
        built
            .declined
            .iter()
            .map(|declined| declined.name.as_str())
            .collect::<Vec<_>>(),
        Vec::<&str>::new()
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
    let dir = diff_root().join("by_diff_privatemangle");
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
fn an_annotated_class_level_value_beside_a_field_is_the_same_fallback() {
    // an *annotated* class-level value is the same fallback a bare one is: the annotation
    // only adds an entry to `__annotations__`, and the value lands in the class namespace
    // either way. so it reaches the field the same way, and the class that used to decline
    // over the pair now answers both readings from the one descriptor
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_constantfieldclash");
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
    assert!(built.declined.is_empty(), "declined: {:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_constantfieldclash as m\n\
         print(m.Tagged('mine').KIND, m.Tagged('mine').read(), m.Tagged.KIND)\n\
         print(type(m.Tagged.read).__name__, type(vars(m.Tagged)['KIND']).__name__)\n",
    );
    assert_eq!(
        out,
        "mine mine class-level\nmethod_descriptor field_default"
    );
}

#[test]
fn a_decorated_class_carries_the_body_its_own_decorator_was_handed() {
    // a class-level constant is copied off the body the interpreted `class` statement
    // wrote, taken while that statement runs and before any of the class's decorators is
    // handed it. reading the finished definition instead is what this used to do, and by
    // then the decorator has been over it.
    //
    // `@dataclass` shows both halves of what that cost. `later` has no default, so
    // `_process_class` *deletes* the `field(init=False)` and left nothing to copy — the
    // emitted type's annotation then read as a required argument after a defaulted one,
    // and the module raised `TypeError` at import. `hidden` has one, so the `Field` was
    // replaced by the bare `2` and `repr=False` went with it — the emitted `repr` showed
    // a field the interpreted one hides, which no sweep can see.
    //
    // the descriptors are what say the compiled types answered: an interpreted leg has a
    // plain `function` in both places
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_decoratedconstant");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
from dataclasses import dataclass, field


@dataclass
class Sub:
    tag: str = \"b\"
    later: list[str] = field(init=False)

    def name(self) -> str:
        return self.tag


@dataclass
class Holder:
    shown: int = 1
    hidden: int = field(default=2, repr=False)

    def total(self) -> int:
        return self.shown + self.hidden
";
    let built = match build_source(
        source,
        "by_diff_decoratedconstant",
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
        "from dataclasses import fields\n\
         import by_diff_decoratedconstant as m\n\
         print(m.Sub('x').name(), repr(m.Holder()), m.Holder().total())\n\
         print([(f.name, f.init, f.repr) for f in fields(m.Sub)])\n\
         print([(f.name, f.init, f.repr) for f in fields(m.Holder)])\n\
         print(type(m.Sub.name).__name__, type(m.Holder.total).__name__)\n",
    );
    assert_eq!(
        out,
        "x Holder(shown=1) 3\n\
         [('tag', True, True), ('later', False, True)]\n\
         [('shown', True, True), ('hidden', True, False)]\n\
         method_descriptor method_descriptor"
    );
}

#[test]
fn the_class_body_capture_reaches_only_this_module_s_own_body() {
    // the capture is a copy of the builtins mapping, carrying a `__build_class__` of ours,
    // put in this module's dict for the length of the fallback run — so no other module and
    // no other thread can reach it, which on a free-threaded interpreter is the difference
    // between a scoped trick and a race.
    //
    // python gives a function the builtins its defining frame had, though, so every
    // function the body defined holds that copy for as long as it lives. `make_held` is
    // that function: calling it once the import is over must make an ordinary class, with
    // nothing recorded into a mapping that has been released by then.
    //
    // it is also a class named `Held`, the same as the module-level one, and the body calls
    // it before the module is finished. that inner one must not stand in for the outer:
    // `EARLY` is the interpreted answer taken during the body, `Held.kind` is what the
    // emitted type carried, and they say different things
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_capturescope");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Held:
    kind = \"module\"

    def where(self) -> str:
        return \"outer\"


def make_held():
    class Held:
        kind = \"local\"

    return Held


EARLY = make_held().kind
";
    let built = match build_source(
        source,
        "by_diff_capturescope",
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
    // `make_held` writes a class in a function body, which nothing lowers. the `Held` the
    // module itself writes is the one that has to be emitted
    assert!(
        !built
            .declined
            .iter()
            .any(|declined| declined.name == "Held"),
        "declined: {:?}",
        built.declined
    );
    let out = run(
        &python,
        &dir,
        "import builtins\n\
         import by_diff_capturescope as m\n\
         print(m.Held.kind, m.EARLY, m.make_held().kind)\n\
         print(type(m.Held.where).__name__)\n\
         print(m.__builtins__ is builtins.__dict__)\n\
         print(m.make_held.__builtins__['__build_class__'] is builtins.__build_class__)\n",
    );
    assert_eq!(out, "module local local\nmethod_descriptor\nTrue\nTrue");
}

#[test]
fn fields_past_a_python_base_leave_that_class_to_its_interpreted_definition() {
    // `Wrapped` keeps its fields past a `codecs.IncrementalDecoder` instance, so it
    // supplies `tp_dealloc`, `tp_traverse` and `tp_clear` and each calls the base's.
    // that base is a class statement's type, whose three are python's own dispatchers:
    // each resolves which base to chain to from `Py_TYPE(self)`, finds `Wrapped`'s
    // function there, and calls it straight back. the two then called each other until
    // the stack ran out — a segfault at the first deallocation, and another at the first
    // collection, which 56 stdlib modules took.
    //
    // which type a base name stands for is the running interpreter's answer, so the
    // refusal is one too, and `Wrapped` is left as the interpreted definition already
    // built it. what that refusal costs is only `Wrapped`: nothing here reads one of its
    // instances, so `Stored` and `stays` are compiled beside it, where the refusal used
    // to be the whole module's. `type(m.Wrapped.get)` and `type(m.stays)` are what say
    // which is which — `function` for the class left behind, and
    // `builtin_function_or_method` for the module that went on being compiled
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_pythonbasestorage");
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
         5 builtin_function_or_method function"
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
    let dir = diff_root().join("by_diff_specdictplacement");
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
    let dir = diff_root().join("by_diff_finalizerfields");
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
    let dir = diff_root().join("by_diff_borrowedoffset");
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
    // which is a wrong answer rather than a slow one.
    //
    // `Inner` keeps its base by naming it: it stores nothing of its own, so it is built
    // on whatever the name holds at import — the interpreted `Outer` — exactly as a
    // class over a base out of this module is. `method_descriptor` against `function`
    // is what says which of the two answered
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_declinedbase");
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
        vec![(
            "Outer",
            "python makes this method implicitly static or class, so slot zero holds the class rather than a receiver"
        )]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_declinedbase as m\n\
         print(issubclass(m.Inner, m.Outer), isinstance(m.Inner(), m.Outer))\n\
         print(m.Inner().label(), m.Inner().tag())\n\
         print(type(m.Inner.__dict__['tag']).__name__,\n\
         \x20     type(m.Outer.__dict__['label']).__name__)\n",
    );
    assert_eq!(
        out,
        "True True\n\
         outer inner\n\
         method_descriptor function"
    );
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
    let dir = diff_root().join("by_diff_interpretedsub");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Container:
    def __init__(self, tag: str) -> None:
        self.tag = tag

    def label(self) -> str:
        return \"container:\" + self.tag


class Parser(Container):
    def __setattr__(self, name: str, value: object) -> None:
        object.__setattr__(self, name, value)

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
            (
                "Parser",
                "`__setattr__` fills a type slot with no adapter yet"
            ),
            // and the decline reaches the caller, which is what keeps `describe` from
            // calling the base's `label` directly past an override it cannot see. it is
            // named for `Parser` rather than for `Container` because the call tests the
            // receiver against the subclass first, and that test is the first thing in
            // the function that mentions a class no longer here
            ("describe", "`Parser` declined, so it has no layout"),
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
    let dir = diff_root().join("by_diff_zerosuper");
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
    let dir = diff_root().join("by_diff_supermro");
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
    let dir = diff_root().join("by_diff_superowner");
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
    let dir = diff_root().join("by_diff_supernoslot");
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
            // `A` is not here on its own account any more: its `@classmethod` carries
            // its convention on the method table entry, and it reads no slot zero. it
            // is at the end of the list instead, behind the subclasses that declined
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
            // every one of those is a class the fallback leaves interpreted, and each
            // extends `A` — so an emitted `A` would be a base with interpreted
            // subclasses, which a static type refuses to be
            (
                "A",
                "`Cm` declined, so it extends the interpreted definition rather than this type"
            ),
        ]
    );
    // a declined class still answers, through the interpreted definition the fallback
    // left behind. only the two whose answer does not turn on the interpreter version
    // are called: python raises in most of the rest, and differently across versions
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
    let dir = diff_root().join("by_diff_supershadow");
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
fn a_field_a_constructor_only_ever_nulls_takes_what_the_module_writes_into_it() {
    // `self.parent = None` in `__init__` and nothing else in the class is how a linked
    // structure is written, and the value that matters is the one put there later from
    // outside. sized for the constructor's assignment alone the field is a slot that
    // holds nothing but `None`: `link` could not be lowered at all, and the interpreted
    // `link` python was then left with raised `TypeError: expected None` against the
    // field's setter. `agree` is what says both halves are fixed — it refuses a decline,
    // so `link` reaching this point means the write compiled
    agree_python(
        "nullfield",
        "\
class Node:
    def __init__(self, name: str) -> None:
        self.name = name
        self.parent = None
        self.tag = None

    def path(self) -> str:
        if self.parent is None:
            return self.name
        return str(self.parent.name) + '/' + self.name


def link(child: Node, parent: Node) -> None:
    child.parent = parent


def label(node: Node, tag: int) -> None:
    node.tag = tag


def build() -> Node:
    root = Node('root')
    leaf = Node('leaf')
    link(leaf, root)
    label(leaf, 7)
    return leaf
",
        &[
            "m.build().path()",
            "m.build().tag",
            // the field takes whatever the module puts in it, and reads back the same
            // object rather than a copy of one
            "[(m.link(a, b), a.parent is b)[1] for a, b in [(m.Node('a'), m.Node('b'))]]",
            // and it still holds the `None` the constructor wrote, for an instance
            // nothing linked
            "(m.Node('x').parent, m.Node('x').tag)",
            // the write itself: `None` on both legs, and the compiled leg's `TypeError`
            // where the slot was too narrow for it
            "_capture(m.link, m.Node('a'), m.Node('b'))",
            "_capture(m.label, m.Node('a'), 3)",
        ],
    );
}

#[test]
fn a_field_a_base_only_ever_nulls_takes_a_subclass_write_too() {
    // a subclass's struct begins with its base's, so the two cannot disagree about what
    // a field holds. the base is where the widening happens and the subclass copies the
    // field across, which is what lets a write through a subclass-typed receiver reach a
    // field the base declared
    agree_python(
        "nullfieldbase",
        "\
class Base:
    def __init__(self) -> None:
        self.owner = None


class Sub(Base):
    def __init__(self) -> None:
        super().__init__()
        self.depth = 0


def adopt(child: Sub, owner: Base) -> None:
    child.owner = owner


def owner_of(node: Base) -> object:
    return node.owner
",
        &[
            "[(m.adopt(c, o), m.owner_of(c) is o)[1] for c, o in [(m.Sub(), m.Base())]]",
            "_capture(m.adopt, m.Sub(), m.Base())",
            "(m.Sub().owner, m.Sub().depth, m.Base().owner)",
            "isinstance(m.Sub(), m.Base)",
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
fn an_arity_error_is_worded_by_the_interpreter() {
    // python's arity wording has rules a reimplementation keeps getting one short of:
    // `and` from two names up, a comma *before* that `and` from three up, a range
    // rather than a count once a parameter has a default, and a near miss offered to a
    // caller who spelled a keyword almost right. the runtime does not reproduce them —
    // it builds a function of the same shape and lets the interpreter refuse the call
    //
    // the three-name cases are what prove that happened. the wording the runtime keeps
    // for itself is deliberately one comma short of python's, so a leg that answered
    // from it rather than from the shape would differ here rather than pass
    agree_python(
        "arityword",
        "\
class Bare:
    pass


class Three:
    def __init__(self, a: int, b: int, c: int) -> None:
        self.a = a


class Spread:
    def __init__(self, a: int, b: int = 1, c: int = 2) -> None:
        self.a = a


class Box:
    def __init__(self) -> None:
        self.n = 0

    def take(self, x: int, y: int, z: int) -> int:
        return x + y + z


def one(a: int) -> int:
    return a


def two(a: int, b: int) -> int:
    return a


def four(a: int, b: int, c: int, d: int) -> int:
    return a


def named(a: int, *, b: int, c: int, d: int) -> int:
    return a


def rest(a: int, b: int, *more: int) -> int:
    return a
",
        &[
            // one name, then two, then three: the separator changes at each step
            "str(_capture(m.one))",
            "str(_capture(m.two))",
            "str(_capture(m.four))",
            "str(_capture(m.named, 1))",
            "str(_capture(m.rest))",
            // a receiver is counted in the arity sentence and not in this one
            "str(_capture(lambda: m.Box().take()))",
            "str(_capture(lambda: m.Box().take(1, 2, 3, 4)))",
            "str(_capture(m.Three))",
            "str(_capture(lambda: m.Three(1)))",
            // a default turns the count into a range
            "str(_capture(lambda: m.Spread(1, 2, 3, 4)))",
            // `object.__init__` refuses anything at all, and does not say which kind
            "str(_capture(lambda: m.Bare(1)))",
            "str(_capture_kw(m.Bare, (), {'q': 1}))",
            // a keyword that nearly names a parameter is offered the parameter
            "str(_capture_kw(m.two, (1, 2), {'aa': 1}))",
            "str(_capture_kw(m.Three, (1, 2, 3), {'aa': 1}))",
            "str(_capture_kw(lambda **k: m.Box().take(1, 2, 3, **k), (), {'xx': 1}))",
            // and one that nearly names the *synthetic* receiver must not be offered it
            "str(_capture_kw(m.two, (1, 2), {'_by_self': 1}))",
            "str(_capture_kw(lambda **k: m.Box().take(1, 2, 3, **k), (), {'_by_rest': 1}))",
            "str(_capture_kw(lambda **k: m.Box().take(**k), (), {'_by_self': 1}))",
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
    // outermost last — the same order the statement itself applies them. the whole
    // *expression* is evaluated there, in this frame, which is what lets a call, a
    // dotted name and a name the frame itself binds all be taken: `@wraps(fn)` reads
    // `fn` out of a register, and `@held` reads the parameter holding the decorator.
    //
    // a type parameter is erased here as anywhere else, so a generic nested
    // function needed nothing beyond dropping the decline
    agree_python(
        "nesteddec",
        "\
import functools
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


def by_call(n: int) -> Callable[[int], int]:
    @scaled(3)
    def inner(x: int) -> int:
        return x + n

    return inner


def by_dotted(n: int) -> Callable[[int], int]:
    @functools.cache
    def inner(x: int) -> int:
        return x + n

    return inner


def by_held(n: int, held: Callable[[Callable[[int], int]], Callable[[int], int]]) -> Callable[[int], int]:
    @held
    def inner(x: int) -> int:
        return x + n

    return inner


def scaled(k: int) -> Callable[[Callable[[int], int]], Callable[[int], int]]:
    def outer(fn: Callable[[int], int]) -> Callable[[int], int]:
        def wrapper(x: int) -> int:
            return fn(x) * k
        return wrapper
    return outer
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
            // a decorator that is a call, evaluated where the `def` stands
            "m.by_call(10)(1)",
            "m.by_call(0)(5)",
            // a dotted name, read off a module this one imported
            "m.by_dotted(10)(1)",
            // and one the frame is *holding*, which a global lookup would have missed
            "m.by_held(10, m.twice)(1)",
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
    let dir = diff_root().join("by_diff_computedinitslot");
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
    //
    // the assertion is python's own wording, and python changed it: `By_Rephrase` renames
    // the shape function it builds through `__qualname__`, which is what the message reads
    // from 3.12 on, where 3.9 reads `__name__` and reports `_()` instead of the real name
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 12))) {
        eprintln!("skipping: the wording this asserts is python 3.12's");
        return;
    }
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
        "arithdunders",
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
    let dir = diff_root().join("by_diff_powslot");
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
fn a_membership_test_and_the_read_it_guards_agree() {
    // `if k in d: ... d[k]` hashes the same key and walks the same table twice, and
    // the second lookup only runs because the first said yes — so it is done once,
    // where the test is. that is only invisible where nothing can count the
    // lookups: a dict subclass may have overridden either dunder, and a key may
    // have a `__hash__` that counts its own calls. both of those come through here
    // and must see exactly what the interpreted twin sees
    agree_python(
        "dictfind",
        "\
class Probe:
    def __init__(self, tag: str) -> None:
        self.tag = tag
        self.hashes = 0

    def __hash__(self) -> int:
        self.hashes = self.hashes + 1
        return 7

    def __eq__(self, other: object) -> bool:
        return True


class Table:
    def __init__(self, rows: dict[str, int], claim: bool) -> None:
        self.rows = rows
        self.claim = claim
        self.asked = 0

    def __contains__(self, key: object) -> bool:
        self.asked = self.asked + 1
        return self.claim

    def __getitem__(self, key: str) -> int:
        return self.rows[key]


def fetched(d: dict[str, int], k: str) -> int:
    if k in d:
        return d[k]
    return -1


def unless_missing(d: dict[str, int], k: str) -> int:
    if k not in d:
        return -1
    return d[k]


def counted(words: list[str]) -> dict[str, int]:
    seen: dict[str, int] = {}
    for word in words:
        if word in seen:
            seen[word] = seen[word] + 1
        else:
            seen[word] = 1
    return seen


def whatever(d: dict[str, object], k: str) -> object:
    if k in d:
        return d[k]
    return \"absent\"


def through(t: Table, k: str) -> int:
    if k in t:
        return t[k]
    return -1


def hashed(d: dict[object, int], k: Probe) -> int:
    if k in d:
        v = d[k]
        return v + k.hashes
    return k.hashes


def kept(d: dict[str, int], k: str) -> object:
    if k in d:
        found = d[k]
        return (found, k)
    return None
",
        &[
            "[m.fetched({'a': 1}, k) for k in ['a', 'b']]",
            "[m.unless_missing({'a': 1}, k) for k in ['a', 'b']]",
            // the value the key maps to is itself false, so "found" cannot be read
            // off the value's truth
            "[m.fetched({'z': 0}, k) for k in ['z', 'y']]",
            "[m.whatever({'n': None, 'e': '', 'f': False}, k) for k in ['n', 'e', 'f', 'g']]",
            "m.counted(['a', 'b', 'a', 'c', 'a', 'b'])",
            "m.counted([])",
            "m.kept({'a': 1}, 'a')",
            "m.kept({'a': 1}, 'b')",
            // a subclass overriding one dunder or the other: `__contains__` saying
            // yes where the table says no is what makes the second lookup real
            "m.whatever(type('D', (dict,), {'__contains__': lambda s, k: True})({'a': 1}), 'a')",
            "type(_capture(m.whatever, \
             type('D', (dict,), {'__contains__': lambda s, k: True})(), 'a')).__name__",
            "m.whatever(type('D', (dict,), {'__getitem__': lambda s, k: 'over'})({'a': 1}), 'a')",
            "m.whatever(type('D', (dict,), {'__contains__': lambda s, k: False})({'a': 1}), 'a')",
            // a container that is not a dict at all, answering both halves itself —
            // and one that claims the key and then has none
            "m.through(m.Table({'a': 1}, True), 'a')",
            "m.through(m.Table({'a': 1}, False), 'a')",
            "type(_capture(m.through, m.Table({}, True), 'a')).__name__",
            "(lambda t: (m.through(t, 'a'), t.asked))(m.Table({'a': 1}, True))",
            // the key counts its own hashes, so hashing it once where the twin
            // hashes it twice shows up here
            "m.hashed({}, m.Probe('p'))",
            "(lambda p: m.hashed({p: 5}, p))(m.Probe('p'))",
            // an unhashable probe is a TypeError from the test, before any read
            "type(_capture(m.whatever, {'a': 1}, [])).__name__",
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
        "containerdunders",
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
    let dir = diff_root().join("by_diff_complexslot");
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
    let dir = diff_root().join("by_diff_awaitslot");
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
    let dir = diff_root().join("by_diff_delslot");
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
    let dir = diff_root().join("by_diff_getattrhook");
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
    let dir = diff_root().join("by_diff_descrget");
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
    let dir = diff_root().join("by_diff_plainpy");
    let interpreted = diff_root().join("by_diff_plainpy_i");
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
fn an_override_reached_through_a_base_agrees() {
    // a call on a base-typed name tests the receiver against each emitted class under
    // that base and calls the body directly where one matches. everything the tests do
    // not describe has to still reach what the protocol would have found: a class that
    // only *inherits* the method, a subclass written in the interpreter, and a method
    // rebound on either class after import — the last of which no type test can see, and
    // which is why the licence is taken once at import and checked on every call
    agree_python(
        "dispatch",
        "\
class Shape:
    def __init__(self, size: int):
        self.size = size

    def area(self) -> int:
        return self.size

    def scaled(self, by: int) -> int:
        return self.size * by

    def label(self) -> str:
        return 'shape'


class Square(Shape):
    def area(self) -> int:
        return self.size * self.size

    def scaled(self, by: int) -> int:
        return self.size * self.size * by

    def label(self) -> str:
        return 'square'


class Wide(Shape):
    pass


def area_of(shape: Shape) -> int:
    return shape.area()


def scale(shape: Shape, by: int) -> int:
    return shape.scaled(by)


def label_of(shape: Shape) -> str:
    return shape.label()
",
        &[
            "m.area_of(m.Shape(5))",
            "m.area_of(m.Square(5))",
            "m.scale(m.Shape(5), 3)",
            "m.scale(m.Square(5), 3)",
            "m.label_of(m.Shape(1))",
            "m.label_of(m.Square(1))",
            // a class that adds no body of its own is not one of the tested ones, so
            // the base's body has to be found the long way round
            "m.area_of(m.Wide(5))",
            "m.label_of(m.Wide(5))",
            // a subclass the interpreter writes has a type of its own, which no test
            // matches
            "m.area_of(type('Tri', (m.Shape,), {'area': lambda self: 100})(5))",
            "m.area_of(type('Tri', (m.Square,), {'area': lambda self: 100})(5))",
            // rebound after import, on the class the receiver is and on its base
            "(setattr(m.Shape, 'area', lambda self: 99), m.area_of(m.Shape(5)))",
            "(setattr(m.Shape, 'area', lambda self: 99), m.area_of(m.Square(5)))",
            "(setattr(m.Shape, 'area', lambda self: 99), m.area_of(m.Wide(5)))",
            "(setattr(m.Square, 'area', lambda self: -1), m.area_of(m.Square(5)))",
            "(setattr(m.Square, 'area', lambda self: -1), m.area_of(m.Shape(5)))",
            // writing anything else on the class is not a rebinding, and the answer
            // stands either way
            "(setattr(m.Shape, 'tag', 1), m.area_of(m.Square(5)))",
            "(delattr(m.Square, 'area'), m.area_of(m.Square(5)))",
        ],
    );
}

#[test]
fn an_override_reached_through_a_subscript_agrees() {
    // the receiver is an element of a `list[Shape]` rather than a name, so it is
    // narrowed to `Shape` on its way out of the subscript — and the dispatch runs on
    // the object from before that narrowing, because narrowing to a *base* is the one
    // thing here that walks an mro. everything the dispatch cannot see still has to
    // reach what the protocol would have found
    agree_python(
        "dispatchitem",
        "\
class Shape:
    def __init__(self, size: int):
        self.size = size

    def area(self) -> int:
        return self.size


class Square(Shape):
    def area(self) -> int:
        return self.size * self.size


class Wide(Shape):
    pass


def first_area(shapes: list[Shape]) -> int:
    return shapes[0].area()


def total(shapes: list[Shape]) -> int:
    running = 0
    i = 0
    while i < len(shapes):
        running = running + shapes[i].area()
        i = i + 1
    return running
",
        &[
            "m.first_area([m.Shape(5)])",
            "m.first_area([m.Square(5)])",
            // inherits the method rather than writing one, so no test matches it
            "m.first_area([m.Wide(5)])",
            "m.total([m.Shape(2), m.Square(3), m.Wide(4)])",
            // a subclass the interpreter writes has a type of its own
            "m.first_area([type('Tri', (m.Shape,), {'area': lambda self: 100})(5)])",
            // rebound after import, which no type test can see
            "(setattr(m.Shape, 'area', lambda self: 99), m.total([m.Shape(5), m.Square(5)]))",
            // a value on the instance shadows the class's method, and the shadow
            // guard on the devirtualised path is what has to notice
            "m.first_area([m.Wide(5)])",
        ],
    );
}

#[test]
fn a_subscripted_receiver_of_the_wrong_class_still_raises() {
    // the narrowing to the element's declared class is emitted in the arm no
    // candidate matched rather than before the dispatch, so that the mro walk it
    // costs is not paid on every trip. moving it must not lose it: a receiver that is
    // not a `Shape` at all reaches that arm, and has to meet exactly the `TypeError`
    // it met when the check came first — not the `AttributeError` the protocol call
    // beyond it would raise
    //
    // this is compiled-only on purpose. the check is the compiler's own, so the
    // interpreted twin does not raise it and the two legs cannot be compared here
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_dispatchcheck");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Shape:
    def __init__(self, size: int):
        self.size = size

    def area(self) -> int:
        return self.size


class Square(Shape):
    def area(self) -> int:
        return self.size * self.size


def first_area(shapes: list[Shape]) -> int:
    return shapes[0].area()
";
    if build_source(
        source,
        "by_diff_dispatchcheck",
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
        "import by_diff_dispatchcheck as m\n\
         print(m.first_area([m.Shape(5)]), m.first_area([m.Square(5)]))\n\
         class Duck:\n\
        \x20   def area(self):\n        return 7\n\
         for bad in (object(), 1, None, Duck()):\n\
        \x20   try:\n        print(m.first_area([bad]))\n\
        \x20   except TypeError as e:\n        print('TypeError:', e)\n",
    );
    // the duck is caught too: it is not a `Shape`, and the check does not care that
    // it happens to answer the name
    assert_eq!(
        out,
        "5 25\n\
         TypeError: expected by_diff_dispatchcheck.Shape, got object\n\
         TypeError: expected by_diff_dispatchcheck.Shape, got int\n\
         TypeError: expected by_diff_dispatchcheck.Shape, got NoneType\n\
         TypeError: expected by_diff_dispatchcheck.Shape, got Duck"
    );
}

#[test]
fn a_dispatched_call_does_not_run_an_argument_before_checking_its_receiver() {
    // moving the receiver's narrowing down to the dispatch's last arm moves it past
    // wherever the arguments are evaluated, so it is only offered for a call that has
    // none. this is what that restriction is for: with an argument in hand, a
    // receiver of the wrong class has to be caught before the argument's side effect
    // runs, exactly as it was when the check came first
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_dispatchorder");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
LOG = []


def noisy() -> int:
    LOG.append(1)
    return 2


class Shape:
    def __init__(self, size: int):
        self.size = size

    def scaled(self, by: int) -> int:
        return self.size * by


class Square(Shape):
    def scaled(self, by: int) -> int:
        return self.size * self.size * by


def scale_first(shapes: list[Shape]) -> int:
    return shapes[0].scaled(noisy())
";
    if build_source(
        source,
        "by_diff_dispatchorder",
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
        "import by_diff_dispatchorder as m\n\
         print(m.scale_first([m.Shape(5)]), m.scale_first([m.Square(5)]))\n\
         m.LOG.clear()\n\
         try:\n\
        \x20   m.scale_first([object()])\n\
         except TypeError as e:\n\
        \x20   print('TypeError:', e)\n\
         print('argument ran:', len(m.LOG))\n",
    );
    assert_eq!(
        out,
        "10 50\n\
         TypeError: expected by_diff_dispatchorder.Shape, got object\n\
         argument ran: 0"
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
    let dir = diff_root().join("by_diff_inheritedinitslot");
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
    let dir = diff_root().join("by_diff_slotsowned");
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
fn a_class_decorator_is_handed_the_body_the_class_statement_wrote() {
    // a decorator that only *reads* its class still reads the whole of it: the names the
    // body bound, the values it gave them and the annotations it wrote. the annotations
    // are the ones an emitted type gets last — they are carried over from the twin, and
    // that carrying used to happen *after* every decorator had run, so each one was
    // handed a class whose body had not arrived yet
    agree_python(
        "decoratorreads",
        "\
SEEN = {}


def inspecting(cls: type) -> type:
    SEEN[cls.__name__] = (cls.__annotations__, 'helper' in cls.__dict__, cls.limit)
    return cls


@inspecting
class Widget:
    tag: str
    size: int
    limit = 7

    def helper(self) -> int:
        return 4
",
        &[
            "sorted(m.SEEN['Widget'][0].items(), key=str)",
            "m.SEEN['Widget'][1]",
            "m.SEEN['Widget'][2]",
            "m.Widget.limit",
            "m.Widget().helper()",
        ],
    );
}

#[test]
fn a_dataclass_decorator_builds_a_constructor_that_runs() {
    // `@dataclass` does not read a class so much as *generate* from it — an `__init__`
    // taking one argument per annotation and assigning one attribute each — so it is the
    // strictest question that can be asked about how faithful an emitted class is. it
    // found two answers wrong at once: an empty `__annotations__` made it write
    // `__init__(self)`, and once that was fixed the assignments had nowhere to land,
    // because an emitted instance is its layout and this class has no fields at all.
    //
    // `y` carries a default, which is a class-level value the decorator reads and then
    // rewrites: `_process_class` leaves the bare `5` where the `field(default=5)` stood.
    // so the value the emitted type carries has to be the one the body wrote rather than
    // the one left behind, and `_capture(m.Point)` is where that shows — a `Point` built
    // with no arguments at all
    agree_python(
        "dataclassdecorator",
        "\
from dataclasses import dataclass, field, fields, replace


@dataclass
class Point:
    x: int
    y: int = field(default=5, repr=False)

    def total(self) -> int:
        return self.x + self.y


def make(n: int) -> str:
    return repr(Point(n, 5))
",
        &[
            "m.make(3)",
            "m.Point(1, 2)",
            "m.Point(1)",
            "m.Point(1, 2) == m.Point(1, 2)",
            "m.Point(1, 2) == m.Point(1, 3)",
            "m.Point(1, 2).total()",
            "[(f.name, f.type, f.repr) for f in m.fields(m.Point)]",
            "sorted(m.Point(1, 2).__dict__.items())",
            "m.Point.__match_args__",
            "m.replace(m.Point(1, 2), y=9)",
            "[(type(e).__name__, str(e)) for e in [_capture(m.Point)]]",
        ],
    );
}

#[test]
fn a_decorated_class_is_the_compiled_one_and_its_dict_is_collectable() {
    // the differential legs agree whichever class answered, so this is where the
    // compiled one is pinned down. it is not a formality: the managed dict this class
    // now carries moves `tp_dictoffset` off its base's, which `By_OffsetsHoldUp` read as
    // the wrong kind of inheritance — the type was quietly dropped for its interpreted
    // twin, and every compiled function went on reading that twin's instances as its own
    // struct. `m.on_final(m.Fixed(2, 'x'))` answered a pointer
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_decoratedlive");
    let _ = std::fs::remove_dir_all(&dir);
    // `from __future__ import annotations` is what keeps `through`'s parameter from
    // being a *read* of `Derived`: without it the module evaluates the annotation where
    // the `def` stands, which is inside the window where `Derived` still holds the
    // interpreted definition, and the class declines rather than move its decorator
    let source = "\
from __future__ import annotations

from dataclasses import dataclass


def tagged(cls: type) -> type:
    cls.tag = 'seen'
    return cls


@dataclass
class Point:
    x: int

    def doubled(self) -> int:
        return self.x * 2


@tagged
class Held:
    def __init__(self, n: int) -> None:
        self.n = n

    def read(self) -> int:
        return self.n


class Plain:
    def __init__(self, n: int) -> None:
        self.n = n

    def read(self) -> int:
        return self.n


@tagged
class Derived(Plain):
    def tripled(self) -> int:
        return self.n * 3


def through(d: Derived) -> int:
    return d.tripled()
";
    let built = match build_source(
        source,
        "by_diff_decoratedlive",
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
        "import gc\n\
         import by_diff_decoratedlive as m\n\
         print(type(m.Point.doubled).__name__, type(m.Held.read).__name__,\n\
         \x20     type(m.Plain.read).__name__, type(m.Derived.tripled).__name__)\n\
         print(m.Point(3).doubled(), m.Held(4).read(), m.Plain(5).read())\n\
         print(m.Point(3).__dict__, m.Held.tag)\n\
         print(m.through(m.Derived(2)), m.Derived(2).read())\n\
         gc.collect()\n\
         base = len(gc.get_objects())\n\
         for _ in range(200):\n\
         \x20   h = m.Held(1)\n\
         \x20   h.itself = h\n\
         del h\n\
         gc.collect()\n\
         print('collected' if len(gc.get_objects()) <= base + 10 else 'leaked')\n\
         print(gc.is_tracked(m.Held(1)), gc.is_tracked(m.Plain(1)))\n",
    );
    // `Plain` is collected too: it declares no `__slots__`, so it keeps a dict of its own
    // and a dict of arbitrary values has to be one the collector can reach. the class
    // that does *not* pay for collection is the one declaring `__slots__`, which
    // [`a_class_declaring_slots_keeps_the_bare_layout`] is about
    assert_eq!(
        out,
        "method_descriptor method_descriptor method_descriptor method_descriptor\n\
         6 4 5\n\
         {'x': 3} seen\n\
         6 2\n\
         collected\n\
         True True"
    );
}

#[test]
fn field_defaults_and_keyword_construction_agree() {
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

def read_two_up(start: int) -> object:
    n = start
    def middle() -> int:
        def bump() -> int:
            nonlocal n
            n = n + 10
            return n
        bump()
        return n
    return middle

def owned_in_the_middle(a: int) -> object:
    def middle() -> int:
        acc = 0
        def step() -> None:
            nonlocal acc
            acc = acc + a
        step()
        step()
        return acc
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
            // the middle frame reads a cell it does *not* own, so the read has to walk
            // out to the frame that does rather than look for a field of its own
            "[f(), f()][1] if (f := m.read_two_up(1)) else None",
            // the cell belongs to the *middle* frame and that frame reads it itself.
            // giving the middle frame a register for `acc` beside the cell its own
            // nested function writes leaves the two halves disagreeing
            "m.owned_in_the_middle(3)()",
            "m.owned_in_the_middle(10 ** 20)()",
        ],
    );
}

#[test]
fn a_closure_inside_a_method_agrees() {
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

#[test]
fn a_module_level_name_bound_to_a_class_follows_the_class_it_named() {
    // the whole module body runs against the interpreted definitions, so a name it binds
    // to a class holds that definition — and the compiled type only ever replaces the one
    // name the `class` statement wrote. what that left was two classes of the same name in
    // the same module: `Kind()` built an object `isinstance(obj, C)` denied, and the
    // compiled `hi` refused it outright.
    //
    // a name that *is* a twin is the one shape that moves soundly, so it is moved onto
    // whatever stands under the class's own name. the annotation on it is beside the
    // point: `Kind: type[C] = C` and `Bare = C` are the same binding.
    //
    // `method_descriptor` is what says the compiled type answered at all — a class that
    // fell back to its interpreted definition has one class and could not show this
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_classalias");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class C:
    def hi(self) -> int:
        return 7


Kind: type[C] = C
Bare = C
Rebound = Bare
";
    let built = match build_source(
        source,
        "by_diff_classalias",
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
        "import by_diff_classalias as m\n\
         print(m.Kind is m.C, m.Bare is m.C, m.Rebound is m.C)\n\
         print(isinstance(m.Kind(), m.C), isinstance(m.C(), m.Bare))\n\
         print(m.C.hi(m.Kind()), m.Kind().hi())\n\
         print(type(m.C.hi).__name__)\n",
    );
    assert_eq!(
        out,
        "True True True\n\
         True True\n\
         7 7\n\
         method_descriptor"
    );
}

#[test]
fn a_class_constant_naming_another_class_is_the_type_that_replaced_it() {
    // a class-level constant is taken off the interpreted definition, so `attr = C` in a
    // body hands over the *twin* — and copying that verbatim gave the compiled type an
    // attribute naming a class nothing else in the module could reach. it goes through the
    // same substitution every carried attribute does.
    //
    // `held` is the boundary in the other direction: a constant that only *reaches* a twin
    // cannot be substituted, because the tuple is the object the body built and its
    // identity is not the twin's. it is pinned here as still *present*, because dropping
    // those instead was built and backed out on the measurement — 65 attributes lost over
    // the stdlib, `ipaddress`'s network constants among them. what the reach holds is a
    // defect of its own and is left exactly as the interpreted definition had it
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_classconst");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class C:
    def hi(self) -> int:
        return 7


class Holder:
    attr = C
    held = (C,)
    plain = 3

    def tag(self) -> str:
        return \"holder\"
";
    let built = match build_source(
        source,
        "by_diff_classconst",
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
        "import by_diff_classconst as m\n\
         print(m.Holder.attr is m.C, m.Holder.plain)\n\
         print(m.C.hi(m.Holder.attr()))\n\
         print(hasattr(m.Holder, 'held'), len(m.Holder.held))\n\
         print(type(m.C.hi).__name__, type(m.Holder.tag).__name__)\n",
    );
    assert_eq!(
        out,
        "True 3\n\
         7\n\
         True 1\n\
         method_descriptor method_descriptor"
    );
}

#[test]
fn a_base_written_as_an_alias_reaches_the_class_it_was_bound_to() {
    // `Alias` and `Root` are one class, so the two spellings have to build one class.
    // taking the alias for a name out of this module built `Over` on the interpreted
    // definition instead — the emitted type goes into the namespace under `Root`, and an
    // alias is carried over to it only once every class has been built — so
    // `isinstance(Over(), Root)` answered `False` where python answers `True`, while
    // `Over.__mro__` still said `Root`. a wrong answer, and one no sweep reaches: the
    // class builds, imports, constructs and subclasses with only its contents lying
    agree_python(
        "aliasbase",
        "\
class Root:
    def root(self) -> str:
        return \"root\"


Alias = Root


class Over(Alias):
    def side(self) -> str:
        return \"over\"
",
        &[
            "isinstance(m.Over(), m.Root)",
            "m.Over.__mro__[1] is m.Root",
            "[c.__name__ for c in m.Over.__mro__]",
            "m.Over().root()",
            "m.Over().side()",
            "issubclass(m.Over, m.Root)",
        ],
    );
}

#[test]
fn an_alias_reaches_the_compiled_type_rather_than_the_twin() {
    // the same source again, because `agree` cannot say which build answered: the
    // interpreted definition answers every one of those calls identically. what is
    // wanted is that the *compiled* type stands under the name, and `method_descriptor`
    // against `function` is what says so
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_aliasbase_type");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
class Root:
    def root(self) -> str:
        return \"root\"


Alias = Root


class Over(Alias):
    def side(self) -> str:
        return \"over\"
";
    let built = match build_source(
        source,
        "by_diff_aliasbase_type",
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
    assert!(built.declined.is_empty(), "{:?}", built.declined);
    let out = run(
        &python,
        &dir,
        "import by_diff_aliasbase_type as m\n\
         print(type(m.Root.root).__name__, type(m.Over.side).__name__)\n\
         print(m.Alias is m.Root, m.Over.__mro__[1] is m.Root)\n\
         print(isinstance(m.Over(), m.Root))\n",
    );
    assert_eq!(
        out,
        "method_descriptor method_descriptor\n\
         True True\n\
         True"
    );
}

#[test]
fn an_alias_does_not_carry_a_base_this_module_lays_out_past_the_gate() {
    // `class OnLaid(Laid, codecs.Codec)` is refused because the layout would have to be
    // inherited from outside and laid out here at once. written through an alias it went
    // straight past — the gate asks `layouts` about the *name* — and compiled the one
    // shape it exists to refuse. so the refusal has to survive the spelling, and `Laid`
    // has to go interpreted with it or `isinstance` disagrees for the same reason again
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_aliaslaid");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
import codecs


class Laid:
    def __init__(self, n: int) -> None:
        self.n = n

    def held(self) -> int:
        return self.n


Alias = Laid


class OnLaid(Alias, codecs.Codec):
    def side(self) -> int:
        return 2
";
    let built = match build_source(
        source,
        "by_diff_aliaslaid",
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
                "OnLaid",
                "a base this module lays out cannot stand beside one it does not"
            ),
            (
                "Laid",
                "`OnLaid` declined, so it extends the interpreted definition rather than this type"
            )
        ]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_aliaslaid as m\n\
         print(m.Alias is m.Laid, m.OnLaid.__mro__[1] is m.Laid)\n\
         print(isinstance(m.OnLaid(1), m.Laid), m.OnLaid(1).held(), m.OnLaid(1).side())\n\
         print(type(m.Laid.held).__name__, type(m.OnLaid.side).__name__)\n",
    );
    assert_eq!(
        out,
        "True True\n\
         True 1 2\n\
         function function"
    );
}

#[test]
fn a_statement_after_a_return_is_dropped_rather_than_declining_the_function() {
    // ty types every expression in unreachable code as `Never`, and `Never` is
    // assignable to everything — so the first test in `map_type` won, which was
    // `None`, a representation with no width at all. a dead statement whose value is
    // unboxed then had nowhere to put it and declined the whole function: `textwrap
    // .dedent` and both of `quopri`'s entry points ran interpreted over code that
    // never runs
    //
    // `agree_python` rather than `agree_python_with_declines`: the point is that
    // nothing here declines, so a decline must fail the test rather than be tolerated
    agree_python(
        "deadcode",
        "\
def after_return(line: str) -> int:
    return len(line)
    b = not line


def after_return_compare(a: int) -> int:
    return a + 1
    c = a < 3


def under_a_false_guard(a: int) -> int:
    if 0:
        d = not a
    return a * 2


def after_raise(a: int) -> int:
    raise ValueError('no')
    e = a > 0


def live_negation(a: int) -> bool:
    return not a
",
        &[
            "m.after_return('abc')",
            "m.after_return_compare(4)",
            "m.under_a_false_guard(21)",
            "str(_capture(m.after_raise, 1))",
            "[m.live_negation(v) for v in (0, 1)]",
        ],
    );
}

/// a class built by a typing special form is left to the definition that builds it
///
/// `class Point(NamedTuple)` does not derive from `NamedTuple` — the special form reads
/// the annotations in the body and builds a `tuple` subclass with `_fields`, a generated
/// `__new__` and a fixed arity. none of that is a layout, and what python ends up with is
/// an ordinary `type` over `tuple`, so nothing downstream refused it the way a
/// `TypedDict`'s own metaclass is refused: it was emitted as a class with **no fields at
/// all**. that built, imported and constructed — and answered `Point(1, "x")` with a
/// `TypeError` while `Point()` succeeded, which is the silent wrong answer this whole
/// design ranks worst
#[test]
fn a_class_a_typing_special_form_builds_agrees() {
    agree_python_with_declines(
        "specialformbase",
        "\
from typing import NamedTuple, TypedDict


class Point(NamedTuple):
    a: int
    b: str


class Mapped(TypedDict):
    a: int


def make() -> object:
    return Point(1, 'x')


def fields() -> object:
    return Point._fields


def mapping() -> object:
    return Mapped(a=2)
",
        &[
            "m.make()",
            "m.fields()",
            "m.mapping()",
            "m.Point(1, 'x').a",
            "len(m.Point(1, 'x'))",
            // the arity the generated `__new__` enforces, which an empty layout lost
            "str(_capture(m.Point))",
            "str(_capture(m.Point, 1, 'x', 2))",
            "isinstance(m.Point(1, 'x'), tuple)",
        ],
    );
}

/// a default that names a class of this module means the class the name means now
///
/// a `def` evaluates its defaults where it stands, and every interpreted definition is
/// evaluated while the fallback source runs — before any emitted type is installed. so a
/// default naming a class of this module captured the **twin**, while every later read of
/// that name answered the type that replaced it. a body comparing the two by identity then
/// got the wrong answer, which is every sentinel-by-identity api at once: this is why a
/// compiled `inspect` rendered `Signature()` as `() -> _empty`
#[test]
fn a_sentinel_default_is_the_class_its_name_now_means() {
    agree_python_with_declines(
        "sentineldefault",
        "\
class Empty:
    pass


def free(ann: object = Empty) -> str:
    return 'empty' if ann is Empty else 'set'


def keyword_only(*, ann: object = Empty) -> str:
    return 'empty' if ann is Empty else 'set'


class Held:
    empty = Empty

    def __init__(self, ann: object = Empty) -> None:
        self._ann = ann

    def kind(self) -> str:
        return 'empty' if self._ann is Empty else 'set'

    def sentinel_is_shared(self) -> bool:
        return Held.empty is Empty


class Declined:
    # a body with a statement that is not a field or a method is left to the
    # interpreted definition, so its methods are the twin's own — the one shape the
    # emitted module keeps no handle to, and the only one the module-namespace walk
    # is what reaches
    for _seed in (1,):
        pass

    def __init__(self, ann: object = Empty) -> None:
        self._ann = ann

    def kind(self) -> str:
        return 'empty' if self._ann is Empty else 'set'
",
        &[
            "m.free()",
            "m.free(1)",
            "m.keyword_only()",
            "m.keyword_only(ann=1)",
            "m.Held().kind()",
            "m.Held(1).kind()",
            "m.Held().sentinel_is_shared()",
            // the name and what the default captured have to be one object
            "m.Held.empty is m.Empty",
            // and a declined class's own methods, which no emitted handle reaches
            "m.Declined().kind()",
            "m.Declined(1).kind()",
        ],
    );
}

/// a dunder the class body *assigns* is left to the definition that can fill its slot
///
/// a class-level constant is copied into the emitted type's `tp_dict`, and a name written
/// there does not fill a type slot — python reads `tp_repr` for `repr(x)` and never
/// consults the name. so `__repr__ = _repr` left `repr(x)` going to the slot the type
/// inherited while `x.__repr__()` answered the assignment: **two answers where the
/// interpreted class has one**, and the one everybody reads was `object`'s.
///
/// a `def __repr__` does not have this problem — the emitter writes a real slot for the
/// dunders it lists — but nothing reaches that path from an assignment. `optparse.Option`
/// and `http.cookies.BaseCookie` are the shape, and both were silently answering
/// `object.__repr__`
#[test]
fn a_dunder_the_body_assigns_agrees() {
    agree_python_with_declines(
        "assigneddunder",
        "\
def _repr(self) -> str:
    return '<made by _repr>'


def _text(self) -> str:
    return 'made by _text'


def _size(self) -> int:
    return 7


class Option:
    def __init__(self, n: int) -> None:
        self.n = n

    __repr__ = _repr
    __str__ = _text
    __len__ = _size


class Plain:
    # an ordinary class-level constant is still carried
    tag = 'kept'

    def __init__(self, n: int) -> None:
        self.n = n
",
        &[
            // the slot and the name have to give one answer, not two
            "repr(m.Option(1))",
            "str(m.Option(1))",
            "len(m.Option(1))",
            "m.Option(1).__repr__()",
            "m.Option(1).__str__()",
            "m.Option(1).n",
            // and a constant that fills no slot is unaffected
            "m.Plain(2).tag",
            "m.Plain(2).n",
        ],
    );
}

/// every shape an attribute write can take, in one module
///
/// an emitted instance **is** its layout: there is no `__dict__` behind it, so an
/// attribute the layout pass never heard about is not merely slower — the write falls
/// through to `PyObject_SetAttr` and raises where the interpreted class stored a value.
/// the pass used to read a plain `self.a = v` and nothing else, which left every other
/// shape here silently wrong.
///
/// `Pipe` is `concurrent.futures.process._ThreadWakeup`, which assigns
/// `self._reader, self._writer` from a pair
const ATTRIBUTE_WRITE_SHAPES: &str = "\
import contextlib


class Pipe:
    def __init__(self, values):
        self.reader, self.writer = values

    def ends(self):
        return (self.reader, self.writer)


class Tree:
    def __init__(self, values):
        (self.a, self.b), *self.rest = values

    def parts(self):
        return (self.a, self.b, self.rest)


class Swapped:
    def __init__(self, a, b):
        self.a = a
        self.b = b

    def swap(self):
        self.a, self.b = self.b, self.a
        return (self.a, self.b)


class Late:
    def configure(self, n):
        self.value = n
        return self.value

    def read(self):
        return self.value


class Counted:
    def __init__(self):
        self.total = 0

    def bump(self, by):
        self.total += by
        return self.total


class Bound:
    def __init__(self, values):
        for self.item in values:
            pass

    def read(self):
        return self.item


class Managed:
    def __init__(self):
        with contextlib.nullcontext(3) as self.held:
            self.inside = self.held + 1

    def read(self):
        return (self.held, self.inside)


class Chained:
    def __init__(self):
        self.a = self.b = 1

    def both(self):
        self.a = self.b = 2
        return (self.a, self.b)


class Renamed:
    def __init__(this, n):
        this.n = n

    def more(me):
        me.extra = me.n + 1
        return me.extra
";

#[test]
fn attribute_writes_of_every_shape_agree() {
    agree_python(
        "attrshapes",
        ATTRIBUTE_WRITE_SHAPES,
        &[
            // the unpacking a pair of pipe ends arrives as
            "m.Pipe((1, 2)).ends()",
            "m.Pipe(('a', 'b')).ends()",
            // a nested target and a starred one, in the same statement
            "m.Tree(((1, 2), 3, 4)).parts()",
            "m.Tree((('a', 'b'),)).parts()",
            // a swap writes both attributes from one tuple, and both reads have to
            // happen before either write
            "m.Swapped(1, 2).swap()",
            "[(s.swap(), s.swap(), (s.a, s.b)) for s in [m.Swapped('l', 'r')]]",
            // a class that sets itself up somewhere other than `__init__`, and the
            // `AttributeError` a read before that has to keep answering
            "[(o.configure(5), o.read()) for o in [m.Late()]]",
            "getattr(m.Late(), 'value', 'unset')",
            // an augmented assignment reads the attribute and writes it back
            "[(c.bump(2), c.bump(3), c.total) for c in [m.Counted()]]",
            // a loop target is an attribute, and an empty iterable leaves it unwritten
            "m.Bound([1, 2, 3]).read()",
            "getattr(m.Bound([]), 'item', 'unset')",
            // a `with` target is bound before its body, which reads it straight back
            "m.Managed().read()",
            // a chained assignment binds every target to the one value
            "m.Chained().both()",
            "[(c.a, c.b) for c in [m.Chained()]]",
            // a method that calls its receiver something other than `self`
            "[(r.more(), r.n, r.extra) for r in [m.Renamed(4)]]",
            // the representations the layout chose have to be the ones python sees
            "[type(v).__name__ for v in m.Pipe((1, 2)).ends()]",
            "[type(v).__name__ for v in m.Tree((('a', 2), 3)).parts()]",
        ],
    );
}

/// which build answered, which no comparison of the two legs can say
///
/// every class here would agree with its interpreted twin by falling back, so the pin is
/// what proves the layouts were fixed rather than the classes given up. `function` for any
/// of them is a class that declined
#[test]
fn the_compiled_types_are_what_answer_for_every_write_shape() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_attrshapes_t");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        ATTRIBUTE_WRITE_SHAPES,
        "by_diff_attrshapes_t",
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
        "import by_diff_attrshapes_t as m\n\
         print(' '.join(type(c.__dict__[n]).__name__ for c, n in (\n\
         \x20   (m.Pipe, 'ends'), (m.Tree, 'parts'), (m.Swapped, 'swap'),\n\
         \x20   (m.Late, 'read'), (m.Counted, 'bump'), (m.Bound, 'read'),\n\
         \x20   (m.Managed, 'read'), (m.Chained, 'both'), (m.Renamed, 'more'))))\n\
         # the fields are storage of the instance's own, with no namespace behind them\n\
         print(hasattr(m.Pipe((1, 2)), '__dict__'))\n",
    );
    assert_eq!(
        out,
        "method_descriptor method_descriptor method_descriptor method_descriptor \
         method_descriptor method_descriptor method_descriptor method_descriptor \
         method_descriptor\nFalse"
    );
}

/// `__dict__` is the one attribute no layout can be given, so the class that reaches for
/// its own declines while everything beside it stands
///
/// there is nothing behind an emitted instance for `__dict__` to stand for, and a field of
/// that name would be a different thing wearing the name. `multiprocessing.dummy.Namespace`
/// writes through one and `tkinter.Event.__repr__` reads one; both used to compile and
/// then raise at the read
const READS_ITS_OWN_DICT: &str = "\
class Namespace:
    def __init__(self, n):
        self.n = n

    def show(self):
        return sorted(self.__dict__)


class Kept:
    def __init__(self, n):
        self.n = n

    def show(self):
        return self.n * 2
";

#[test]
fn a_class_that_reads_its_own_dict_agrees_beside_the_one_that_stands() {
    agree_python_with_declines(
        "attrdict",
        READS_ITS_OWN_DICT,
        &[
            "m.Namespace(3).show()",
            "m.Namespace(3).n",
            "m.Kept(3).show()",
            "m.Kept(3).n",
        ],
    );
}

/// and it is the reaching class alone that falls back
#[test]
fn only_the_class_that_reads_its_own_dict_is_left_interpreted() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_attrdict_t");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        READS_ITS_OWN_DICT,
        "by_diff_attrdict_t",
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
    let reasons: Vec<&str> = built
        .declined
        .iter()
        .map(|declined| declined.reason.as_str())
        .collect();
    assert_eq!(
        reasons,
        [
            "`__dict__` is read off a `Namespace`, and an emitted instance is its layout \
             with nothing behind it"
        ]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_attrdict_t as m\n\
         print(type(m.Namespace.__dict__['show']).__name__,\n\
         \x20     type(m.Kept.__dict__['show']).__name__)\n",
    );
    assert_eq!(out, "function method_descriptor");
}

/// the source both attribute-deletion tests compile
///
/// `Held` deletes a refcounted attribute and `Branching` one no path may have written,
/// which are the two states the presence byte beside a field distinguishes
const DELETES_AN_ATTRIBUTE: &str = "\
class Held:
    def __init__(self) -> None:
        self.tag: object = 'first'
        self.kept: int = 2
        self.__hidden: int = 3

    def drop(self) -> None:
        del self.tag

    def drop_hidden(self) -> None:
        del self.__hidden

    def put(self, value: object) -> None:
        self.tag = value


class Branching:
    def __init__(self, flag: bool) -> None:
        self.kept: int = 1
        if flag:
            self.maybe: int = 2

    def drop(self) -> None:
        del self.maybe
";

/// `del self.tag` unbinds the attribute where it used to refuse
///
/// an emitted instance is its layout, so there is no dict entry to remove — the delete
/// clears the presence byte beside the field, which is the same byte a read already
/// consults and a write already sets. before that the emitted class answered
/// `AttributeError: cannot delete an attribute` where the interpreted twin deleted
#[test]
fn deleting_an_attribute_agrees() {
    agree_python(
        "deleteattr",
        DELETES_AN_ATTRIBUTE,
        &[
            "_deleted(m.Held())",
            "repr(_capture(m.Branching(False).drop))",
            "_capture(m.Branching(True).drop)",
            "hasattr(m.Branching(True), 'maybe')",
            "hasattr(m.Branching(False), 'maybe')",
            "m.Branching(True).kept",
        ],
    );
}

/// and it is the *compiled* class doing it
///
/// [`agree_python`] cannot say which build answered: a class that fell back to its
/// interpreted definition deletes the way python does and passes unchanged. the
/// descriptor's type is what tells the two apart
#[test]
fn the_class_that_deletes_an_attribute_is_the_compiled_one() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_deleteattr_t");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        DELETES_AN_ATTRIBUTE,
        "by_diff_deleteattr_t",
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
        "import by_diff_deleteattr_t as m\n\
         print(type(m.Held.__dict__['drop']).__name__,\n\
         \x20     type(m.Branching.__dict__['drop']).__name__)\n",
    );
    assert_eq!(out, "method_descriptor method_descriptor");
}

/// and it releases what the field held exactly once
///
/// a delete that forgot to release leaks the value; one that cleared the byte without
/// zeroing the member lets the deallocation release it a second time; and a second
/// delete that did not check the byte would release it twice on its own. none of the
/// three shows in a single cycle, so the observation is a held object's reference count
/// across many — and a count driven negative would have freed a live object rather than
/// reported anything
#[test]
fn deleting_an_attribute_releases_it_exactly_once() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_deleterefs_t");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        DELETES_AN_ATTRIBUTE,
        "by_diff_deleterefs_t",
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
        "import by_diff_deleterefs_t as m\n\
         import gc, sys\n\
         print(type(m.Held.__dict__['drop']).__name__)\n\
         sentinel = ['sentinel']\n\
         held = m.Held()\n\
         gc.collect()\n\
         start = sys.getrefcount(sentinel)\n\
         objects = len(gc.get_objects())\n\
         for _ in range(20000):\n\
         \x20   held.put(sentinel)\n\
         \x20   held.drop()\n\
         \x20   try:\n\
         \x20       held.drop()\n\
         \x20   except AttributeError:\n\
         \x20       pass\n\
         gc.collect()\n\
         print('after delete cycles', sys.getrefcount(sentinel) - start)\n\
         for _ in range(20000):\n\
         \x20   held.put(sentinel)\n\
         gc.collect()\n\
         print('while held', sys.getrefcount(sentinel) - start)\n\
         held.drop()\n\
         gc.collect()\n\
         print('after the last delete', sys.getrefcount(sentinel) - start)\n\
         for _ in range(20000):\n\
         \x20   fresh = m.Held()\n\
         \x20   fresh.put(sentinel)\n\
         \x20   fresh.drop()\n\
         \x20   fresh.put(sentinel)\n\
         del fresh\n\
         gc.collect()\n\
         print('after dropping the instances', sys.getrefcount(sentinel) - start)\n\
         # and an instance that dies while the field is still deleted. the\n\
         # deallocation releases every field without asking the byte, relying on the\n\
         # zero `tp_alloc` left, so a delete that cleared the byte and left the member\n\
         # pointing at the value releases it here a second time\n\
         for _ in range(20000):\n\
         \x20   gone = m.Held()\n\
         \x20   gone.put(sentinel)\n\
         \x20   gone.drop()\n\
         del gone\n\
         gc.collect()\n\
         print('after dropping deleted instances', sys.getrefcount(sentinel) - start)\n\
         print('objects grew', len(gc.get_objects()) - objects < 100)\n",
    );
    assert_eq!(
        out,
        "method_descriptor\n\
         after delete cycles 0\n\
         while held 1\n\
         after the last delete 0\n\
         after dropping the instances 0\n\
         after dropping deleted instances 0\n\
         objects grew True"
    );
}

/// a function nested in a method reaches the receiver of the frame around it
///
/// `self` is a parameter of the enclosing method like any other, so the nested function
/// captures it — but the only mention of it in `self.held = 2` is inside an assignment
/// *target*, and the capture pass read a statement's value and never its target. the
/// nested function then had no capture named `self` at all and resolved it as a global,
/// which raised `NameError` at the first call
#[test]
fn a_nested_function_writing_through_the_receiver_agrees() {
    agree_python(
        "nestedself",
        "\
class Held:
    def __init__(self, values: list) -> None:
        self.held: int = 1
        self.tag: object = 'first'
        self.item: int = 0
        self.bucket: list = [0, 0]

        def assign() -> None:
            self.held = 2

        def loop() -> None:
            for self.item in values:
                pass

        def store() -> None:
            self.bucket[0] = 9

        def drop() -> None:
            del self.tag

        assign()
        loop()
        store()
        drop()

    def rows(self) -> list:
        return [self.held, self.item, self.bucket, hasattr(self, 'tag')]
",
        &[
            "m.Held([7, 8]).rows()",
            "m.Held([]).rows()",
            "m.Held([1]).item",
        ],
    );
}

/// and it is the compiled `__init__` reaching it
#[test]
fn the_nested_function_reaching_the_receiver_is_in_the_compiled_init() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_nestedself_t");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        "\
class Held:
    def __init__(self) -> None:
        self.held: int = 1

        def go() -> None:
            self.held = 2

        go()
",
        "by_diff_nestedself_t",
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
        "import by_diff_nestedself_t as m\n\
         print(type(m.Held.__dict__['__init__']).__name__, m.Held().held)\n",
    );
    // a real slot answered: `__init__` is a name python reaches through `tp_init`, and
    // `PyType_Ready` gives it a wrapper that shadows any method table entry
    assert_eq!(out, "wrapper_descriptor 2");
}

/// the source both `classmethod` write tests compile
const WRITES_ON_THE_CLASS: &str = "\
class Counter:
    count: int = 0

    @classmethod
    def bump(cls) -> int:
        cls.count = cls.count + 1
        return cls.count


class Plain:
    def value(self) -> int:
        return 7
";

/// a `classmethod` that binds an attribute on the class declines, and takes the class
/// with it
///
/// slot zero holds the emitted type, and an emitted type is sealed — `cls.count = 1`
/// raises `TypeError: cannot set attribute of immutable type` where python binds a class
/// attribute. the decline is what makes the answer right: it reaches the whole class, so
/// python is left with the interpreted one, which takes the write
#[test]
fn a_classmethod_writing_on_the_class_declines_and_still_agrees() {
    agree_python_with_declines(
        "clswrite",
        WRITES_ON_THE_CLASS,
        &[
            "[m.Counter.bump(), m.Counter.bump(), m.Counter.count]",
            "m.Plain().value()",
        ],
    );
}

/// and the decline is that one class, not the module
///
/// a method left interpreted on a class that still compiled would be handed the emitted
/// type and raise exactly as the compiled method did, so the class is what has to
/// decline. the sibling staying native is what says the decline stopped there
#[test]
fn the_class_a_classmethod_write_declines_is_the_interpreted_one() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_clswrite_t");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        WRITES_ON_THE_CLASS,
        "by_diff_clswrite_t",
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
    let reasons: Vec<&str> = built
        .declined
        .iter()
        .map(|declined| declined.reason.as_str())
        .collect();
    assert_eq!(
        reasons,
        [
            "`cls.count` binds an attribute on the class, and the type this module emits \
          for it is sealed"
        ]
    );
    let out = run(
        &python,
        &dir,
        "import by_diff_clswrite_t as m\n\
         print(type(m.Counter.__dict__['bump']).__name__,\n\
         \x20     type(m.Plain.__dict__['value']).__name__,\n\
         \x20     m.Counter.bump())\n",
    );
    assert_eq!(out, "classmethod method_descriptor 1");
}

/// a class body that *assigns* its dunders rather than defining them
///
/// each of these fills a type slot, which a name in `tp_dict` cannot: python reads
/// `tp_repr` for `repr(x)` and never consults the name. `kind` and `__match_args__` are
/// the controls — neither fills a slot, and both still have to be carried. the dunder is
/// the sharper of the two: an ordinary name is carried by the twin adoption as well,
/// while a dunder is skipped there and the constant copy is its only route
const ASSIGNED_DUNDERS: &str = "\
def _describe(self):
    return 'described ' + self.tag


def _size(self):
    return len(self.tag)


def _at(self, index):
    return self.tag[index]


def _scale(self, factor):
    return self.tag * factor


def _same(self, other):
    return self.tag == getattr(other, 'tag', other)


def _shout(self, suffix='!', times=1):
    return self.tag.upper() + suffix * times


class Assigned:
    kind = 'constant'
    __match_args__ = ('tag',)
    __repr__ = _describe
    __str__ = __repr__
    __len__ = _size
    __getitem__ = _at
    __rmul__ = _scale
    __eq__ = _same
    __call__ = _shout

    def __init__(self, tag):
        self.tag = tag


class Unhashable:
    __hash__ = None

    def __init__(self, tag):
        self.tag = tag
";

/// the slot and the name give one answer, and it is the assignment's
///
/// `__repr__ = _describe` used to leave `repr(x)` on the slot python inherited while
/// `x.__repr__()` answered the assignment — two answers where the interpreted class has
/// one. every pair below asks the same question through the slot and through the name.
/// `__str__ = __repr__` is the shape that cost the most: `configparser.Error` writes it,
/// and declining it took every other class in the module with it
#[test]
fn an_assigned_dunder_fills_its_type_slot() {
    agree_python(
        "assigneddunder",
        ASSIGNED_DUNDERS,
        &[
            "repr(m.Assigned('a'))",
            "m.Assigned('a').__repr__()",
            "str(m.Assigned('b'))",
            "m.Assigned('b').__str__()",
            "len(m.Assigned('abc'))",
            "m.Assigned('abc').__len__()",
            "bool(m.Assigned(''))",
            "m.Assigned('abc')[1]",
            "m.Assigned('abc').__getitem__(1)",
            "3 * m.Assigned('ab')",
            "m.Assigned('ab').__rmul__(3)",
            "m.Assigned('a') == m.Assigned('a')",
            "m.Assigned('a') == m.Assigned('z')",
            "m.Assigned('a').__eq__('a')",
            "m.Assigned('hi')('?', times=2)",
            "m.Assigned('hi').__call__('?', times=2)",
            "m.Assigned('a').kind",
            "m.Assigned.__match_args__",
        ],
    );
}

/// `__hash__ = None` names nothing to call: it says the type has no hash at all
///
/// python spells that in the slot as `PyObject_HashNotImplemented`. putting the `None`
/// in the dict alone would leave `hash(x)` answering the slot python inherited — an
/// address — where the interpreted class raises.
///
/// what the `TypeError` *says* is the observation rather than that there was one: a slot
/// that called the `None` raises a `TypeError` too, about `NoneType` not being callable.
/// only the leading words are compared, because the two builds name the type differently
/// — a spec's `tp_name` carries the module and a class statement's does not
#[test]
fn a_hash_assigned_none_makes_the_type_unhashable() {
    agree_python(
        "assigneddunderhash",
        ASSIGNED_DUNDERS,
        &[
            "m.Unhashable.__hash__",
            "isinstance(m.Unhashable('a'), __import__('collections').abc.Hashable)",
            "(lambda e: (type(e).__name__, str(e)[:18]))(_capture(hash, m.Unhashable('a')))",
            "(lambda e: (type(e).__name__, str(e)[:18]))(\
             _capture(lambda x: {x: 1}, m.Unhashable('a')))",
        ],
    );
}

/// and the class every answer above came from is the compiled one
///
/// a class that fell back to its interpreted definition answers all of it identically,
/// so none of it can say which build answered. the descriptor types can: `PyType_Ready`
/// writes a `wrapper_descriptor` over each name the type filled a slot with, which an
/// interpreted class has for none of its own.
///
/// the two `__repr__` answers are the twin model rather than a fallback: the module's own
/// `_describe` is compiled, and what the class dict holds is the interpreted definition's
/// function — the same object the class body evaluated, because that is the only place
/// evaluating once can leave it. `__str__` being that same object again is the alias
/// following its target rather than being resolved twice
#[test]
fn the_class_an_assigned_dunder_fills_a_slot_on_is_compiled() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_assigneddunder_t");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        ASSIGNED_DUNDERS,
        "by_diff_assigneddunder_t",
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
        "import by_diff_assigneddunder_t as m\n\
         print(type(m.Assigned.__dict__['__init__']).__name__,\n\
         \x20     type(m.Unhashable.__dict__['__init__']).__name__)\n\
         print(m.Assigned.__dict__['__repr__'] is m.Assigned.__dict__['__str__'],\n\
         \x20     type(m.Assigned.__dict__['__repr__']).__name__,\n\
         \x20     type(m._describe).__name__)\n",
    );
    assert_eq!(
        out,
        "wrapper_descriptor wrapper_descriptor\n\
         True function builtin_function_or_method"
    );
}

#[test]
fn a_pair_returned_as_a_display_travels_in_registers() {
    // a body whose every `return` is a tuple display hands its elements back one per
    // register, and a caller that unpacks them never builds the object at all. python
    // builds a fresh tuple at each such display — `split(1) is split(1)` is already
    // false — so the one built at the boundary is the object it would have handed back.
    //
    // ordinary python, because `is` is what the identity of a rebuilt tuple would show
    // up in, and in `.by` `is` is the type test rather than the identity comparison
    agree_python(
        "pairreg",
        "\
def split(value: int) -> tuple[int, int]:
    return value // 7, value % 7


def summed(n: int) -> int:
    total = 0
    i = 0
    while i < n:
        whole, part = split(i)
        total = total + whole + part
        i = i + 1
    return total


# the two slots hold different representations, and each keeps its own
def labelled(n: int) -> tuple[int, str]:
    return n, str(n)


# every `return` has to agree on the layout, across branches
def either(n: int) -> tuple[int, int]:
    if n < 0:
        return 0, n
    return n, 0


# a pair that reaches a name is an ordinary object again, so it has an identity and
# keeps it however many times it is read
def aliased(n: int) -> bool:
    p = split(n)
    q = p
    return p is q


def held_twice(n: int) -> bool:
    p = split(n)
    xs = [p, p]
    return xs[0] is xs[1]


# an index the compiler cannot see still goes through the object, negative index and
# `IndexError` included
def at(n: int, i: int) -> int:
    return split(n)[i]


# a caught exception is taken out of the thread state, so a pair-returning call
# inside a handler does not read the handled exception as its own failure
def after_catching(n: int) -> int:
    try:
        raise ValueError(\"x\")
    except ValueError:
        whole, part = split(n)
        return whole + part
",
        &[
            "m.split(93)",
            "m.split(0)",
            // a value too wide for the tagged representation still rides in its slot
            "m.split(10 ** 30)",
            "m.summed(50)",
            "m.labelled(4)",
            "[m.either(n) for n in (-3, 3)]",
            "type(m.split(1)).__name__",
            "len(m.split(1))",
            "m.split(1) == m.split(1)",
            // the object handed to a python caller is a fresh tuple, exactly as the
            // display in the body builds one
            "m.split(1) is m.split(1)",
            "[m.aliased(9), m.held_twice(9)]",
            "[m.at(93, 0), m.at(93, 1), m.at(93, -1), m.at(93, -2)]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.at, 93, 5)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.at, 93, -5)]]",
            "m.after_catching(93)",
        ],
    );
}

/// a pair unpacked every trip round a loop, whose elements are the caller's own
/// object
///
/// the two element reads no longer retain, so what keeps the value alive is the
/// struct the call answered with. getting that window wrong frees the caller's string
/// mid-loop, which is fatal rather than merely wrong
const BORROWED_ELEMENTS: &str = "\
def split(text: str) -> tuple[str, str]:
    return text, text


def scanned(text: str, n: int) -> int:
    total = 0
    i = 0
    while i < n:
        head, tail = split(text)
        total = total + len(head) + len(tail)
        i = i + 1
    return total


# the struct is written again under the loop, which releases what the first pair put
# in it while the reads of the second are still to come
def replaced(a: str, b: str, n: int) -> int:
    total = 0
    i = 0
    while i < n:
        head, tail = split(a)
        other, rest = split(b)
        total = total + len(head) + len(other) + len(tail) + len(rest)
        i = i + 1
    return total


# an element that leaves the frame is a reference handed out, so it is owned however
# the loop above is compiled
def first(text: str) -> str:
    head, tail = split(text)
    return head
";

#[test]
fn a_pair_unpacked_in_a_loop_agrees() {
    agree(
        "borrowedelements",
        BORROWED_ELEMENTS,
        &[
            "[m.scanned(s, 3) for s in ('', 'a', 'abcdef', 'é🎉z')]",
            "m.scanned('word ' * 40, 25)",
            "[m.replaced('ab', 'cde', n) for n in (0, 1, 9)]",
            "[m.first(s) for s in ('', 'abc')]",
        ],
    );
}

#[test]
fn a_borrowed_tuple_element_does_not_move_its_source_references() {
    // the element read takes nothing, so a stray release shows as a falling count and
    // a missing one as a climbing count. a release per trip reaches zero long before
    // the loop ends, which is a crash rather than a wrong answer
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_elements_rc");
    let _ = std::fs::remove_dir_all(&dir);
    if build_source(
        BORROWED_ELEMENTS,
        "by_diff_elements_rc",
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
        "import sys, by_diff_elements_rc as m\n\
         kept = 'a bb ccc ' * 30\n\
         other = 'x y ' * 20\n\
         before = (sys.getrefcount(kept), sys.getrefcount(other))\n\
         for _ in range(2000):\n\
         \x20   m.scanned(kept, 5)\n\
         \x20   m.replaced(kept, other, 5)\n\
         \x20   m.first(kept)\n\
         print('stable' if (sys.getrefcount(kept), sys.getrefcount(other)) == before else 'moved')\n",
    );
    assert_eq!(out, "stable");
}

#[test]
fn a_tuple_the_body_did_not_build_keeps_its_identity() {
    // the register representation is licensed by *where the tuple came from*, not by
    // its type. a body that hands back the pair it was given is handing back that very
    // object — `passed(p) is p` is true — so it stays on the heap, and so does one
    // whose length the checker cannot pin down
    agree_python(
        "pairkeep",
        "\
def passed(p: tuple[int, int]) -> tuple[int, int]:
    return p


def chosen(p: tuple[int, int], q: tuple[int, int], first: bool) -> tuple[int, int]:
    if first:
        return p
    return q


# a display in one branch and a pass-through in the other is still a pass-through
def mixed(p: tuple[int, int], keep: bool) -> tuple[int, int]:
    if keep:
        return p
    return 0, 0


# `*` unpacking has no length until it runs
def spread(p: tuple[int, int]) -> tuple[int, int, int]:
    return (*p, 9)


# and neither has a tuple of unbounded length
def repeated(n: int) -> tuple[int, ...]:
    return (n,) * 3
",
        &[
            "(lambda p: [m.passed(p) is p, m.passed(p) == p])((1, 2))",
            "(lambda p, q: [m.chosen(p, q, True) is p, m.chosen(p, q, False) is q])((1, 2), (3, 4))",
            "(lambda p: [m.mixed(p, True) is p, m.mixed(p, False)])((1, 2))",
            "(lambda p: m.spread(p))((1, 2))",
            "m.repeated(5)",
        ],
    );
}

#[test]
fn a_returned_pair_holds_each_slot_in_its_own_representation() {
    // a slot is whatever its element type is: a `double` is not refcounted at all and
    // a `str` is, so the tuple built at the boundary has to box each of them its own
    // way. `.by` rather than `.py` because python's `float` annotation admits an
    // `int`, which puts the slot back on the object protocol and so tests nothing.
    //
    // an instance of a class this module emits is refused and stays on the heap —
    // the tuple structs are written before the class structs, so a slot naming one
    // would name a type the C does not have yet
    agree(
        "pairslots",
        "\
class Point:
    def __init__(self, x: int) -> None:
        self.x = x


def placed(n: int) -> tuple[Point, int]:
    return Point(n), n


def scaled(n: int) -> tuple[float, float]:
    return n * 1.5, n * 0.5


def described(n: int) -> tuple[str, int]:
    return str(n), n


def total(n: int) -> float:
    lo, hi = scaled(n)
    return lo + hi


def named(n: int) -> str:
    text, count = described(n)
    return text * count
",
        &[
            "m.placed(3).__class__.__name__",
            "[m.placed(3)[0].x, m.placed(3)[1]]",
            "m.scaled(4)",
            "m.described(3)",
            "[m.total(4), m.named(2)]",
            // the tuple the boundary builds owns its elements, and nothing else does:
            // a leaked reference would keep the point alive after the tuple went
            "(lambda: [len(m.placed(i)) for i in range(200)][-1])()",
        ],
    );
}

#[test]
fn a_return_inside_a_match_case_is_seen_by_the_representation_choice() {
    // the walk that decides how a body hands its pair back has to reach a `case`
    // body: a `return` it missed would be lowered against a layout nothing proved it
    // had. so a pass-through hidden in a `case` keeps the whole body on the heap
    agree_python(
        "pairmatch",
        "\
def picked(p: tuple[int, int], n: int) -> tuple[int, int]:
    match n:
        case 0:
            return p
        case _:
            return n, n
    return 0, 0
",
        &["(lambda p: [m.picked(p, 0) is p, m.picked(p, 3)])((1, 2))"],
    );
}

#[test]
fn globals_reaches_this_module_and_not_the_caller() {
    // a compiled function pushes no python frame, so calling the builtin would answer
    // about the frame underneath — the caller's, in another module. the read came back
    // `None` for a name this module plainly binds, and the write bound `written` in the
    // caller's namespace instead of here. every body shape asks the same question,
    // because a nested function, a lambda and a method are all compiled the same way
    agree_python(
        "globalsdict",
        "\
marker = 7


def read() -> object:
    return globals().get('marker')


def write() -> object:
    globals()['written'] = 11
    return globals().get('written')


def nested() -> object:
    def inner() -> object:
        return globals().get('marker')

    return inner()


def by_lambda() -> object:
    return (lambda: globals().get('marker'))()


class Holder:
    def read(self) -> object:
        return globals().get('marker')
",
        &[
            "m.read()",
            "[m.write(), m.marker, sorted(k for k in vars(m) if not k.startswith('__'))]",
            "m.nested()",
            "m.by_lambda()",
            "m.Holder().read()",
        ],
    );
}

#[test]
fn a_globals_of_the_modules_own_is_not_the_builtin() {
    // python resolves the name like any other, so a module that binds `globals` itself
    // has to be called — answering with the module namespace would be a wrong answer
    // that only a module defining an unusual name would ever see
    agree_python(
        "globalsshadow",
        "\
marker = 7


def globals() -> dict[str, object]:
    return {'marker': 99}


def read() -> object:
    return globals().get('marker')


def local_shadow() -> object:
    globals = {'marker': 42}
    return globals.get('marker')
",
        &["m.read()", "m.local_shadow()"],
    );
}

#[test]
fn the_frame_reading_builtins_answer_from_the_interpreted_definition() {
    // `locals()`, `vars()` and `dir()` with nothing to be about, and `eval`/`exec` with
    // no namespace of their own, all read the calling frame — which a compiled function
    // does not have. they are declined, and the interpreted definition answers
    agree_python_with_declines(
        "framebuiltins",
        "\
marker = 7


def here() -> object:
    alpha = 1
    beta = 2
    return locals()


def as_vars() -> object:
    gamma = 3
    return vars()


def names() -> object:
    delta = 4
    return dir()


def evaluated() -> object:
    return eval('marker')


def ran() -> object:
    out = []
    exec('out.append(marker)')
    return out
",
        &[
            "m.here()",
            "m.as_vars()",
            "m.names()",
            "m.evaluated()",
            "m.ran()",
        ],
    );
}

#[test]
fn exec_handed_a_namespace_of_its_own_stays_compiled() {
    // the fallback is what makes the bare form frame-dependent; handed a `dict` neither
    // builtin looks at a frame at all, so declining one would be pure cost
    agree_python(
        "execnamespace",
        "\
def ran(ns: dict[str, object]) -> object:
    exec('produced = 3 * 4', ns)
    return ns.get('produced')
",
        &["m.ran({})"],
    );
}

#[test]
fn a_frame_walk_answers_from_the_interpreted_definition() {
    // `sys._getframe()` hands back the frame of whoever called it, so in a compiled
    // function it reaches the caller's — and `f_globals` then reads another module's
    // namespace while looking exactly like it read this one's
    agree_python_with_declines(
        "getframe",
        "\
import sys

marker = 7


def owner() -> object:
    return sys._getframe().f_globals.get('marker')
",
        &["m.owner()"],
    );
}

#[test]
fn a_warning_at_the_default_level_carries_its_own_context() {
    // `warnings.warn` picks the module to report against by counting frames back from
    // its own caller, so a compiled caller — which pushes no frame — moves the count
    // one module outwards. that decides the `file:line` the message prints and, through
    // the blamed module's `__name__`, whether the filters show it at all:
    // `urllib.request.URLopener.__init__` warns with `stacklevel=3` and the compiled leg
    // printed nothing where python printed a `DeprecationWarning`.
    //
    // at the default stack level there is nothing to count — the frame `warn` would
    // have read is the warning function's own — so the call is lowered into
    // `warn_explicit` with that context written in, and every one of these compiles
    agree_python(
        "warnhere",
        "\
import warnings


def plain() -> None:
    warnings.warn('plain')


def deprecated() -> None:
    warnings.warn('gone', DeprecationWarning)


def written_level() -> None:
    warnings.warn('one', DeprecationWarning, stacklevel=1)


def zero_level() -> None:
    warnings.warn('nought', DeprecationWarning, stacklevel=0)


def negative_level() -> None:
    warnings.warn('under', DeprecationWarning, stacklevel=-4)


def by_keyword() -> None:
    warnings.warn(message='kw', category=DeprecationWarning, stacklevel=1)


def no_source() -> None:
    warnings.warn('unsourced', DeprecationWarning, source=None)


def computed(text: str) -> None:
    warnings.warn('made ' + text, DeprecationWarning)
",
        &[
            "_warnings_from(m.plain)",
            "_warnings_from(m.deprecated)",
            "_warnings_from(m.written_level)",
            "_warnings_from(m.zero_level)",
            "_warnings_from(m.negative_level)",
            "_warnings_from(m.by_keyword)",
            "_warnings_from(m.no_source)",
            "_warnings_from(m.computed, 'up')",
            "_warned_into(m, m.plain)",
            "_warned_into(m, m.deprecated)",
        ],
    );
}

#[test]
fn a_warning_repeated_is_shown_as_often_as_python_shows_it() {
    // the module's `__warningregistry__` is what stops the same warning being printed
    // twice, and it is the *blamed* module's — so a lowering that wrote its own
    // registry somewhere else, or made a fresh one each call, would keep printing.
    // the version key is the other half: mutating the filters clears the registry, and
    // a warning already shown is then shown again
    agree_python(
        "warnrepeat",
        "\
import warnings


def once() -> None:
    warnings.warn('again', DeprecationWarning)
",
        &[
            "[m.once() for _ in range(3)] and None",
            "_warned_into(m, m.once)",
            "_repeated_warning(m)",
            "_registry_after_a_filter_change(m)",
        ],
    );
}

#[test]
fn a_warning_is_blamed_on_the_module_python_blames_it_on() {
    // the whole defect this lowering fixes is a warning blamed on the wrong module, so
    // this asks the question directly rather than through the file name. `warn` takes
    // the blamed module from the frame's `__name__`, keeping a string or `None` and
    // standing in `<string>` for anything else — a missing name included, which is a
    // branch of the C accelerator that `warnings.py` does not have
    agree_python(
        "warnblame",
        "\
import warnings


def f() -> None:
    warnings.warn('blame', DeprecationWarning)
",
        &[
            "_blamed_module(m, m.f, m.__name__)",
            "_blamed_module(m, m.f, 'renamed')",
            "_blamed_module(m, m.f, None)",
            "_blamed_module(m, m.f, 5)",
            "_blamed_module(m, m.f)",
        ],
    );
}

#[test]
fn a_warning_takes_its_category_from_the_shapes_python_takes_it_from() {
    // `warn`'s preamble decides the category before any of the context does, and the
    // rules are not the obvious ones: a `Warning` *instance* supplies it whatever was
    // written, the instance test honours a `__class__` property while the category
    // taken is the real type, and a category that is not a `Warning` subclass raises
    // with a wording of python's own. the runtime helper reproduces all of it, and
    // this is where that is checked rather than asserted.
    //
    // the refusals name the offending *type*, and python builds that name out of
    // `tp_name` — which an emitted class carries dotted, where its interpreted twin's
    // is bare. that difference belongs to how classes are emitted rather than to
    // warnings: `'t.Marker' object is not callable` against `'Marker' object is not
    // callable` is the same gap with no warning in sight. so the refusals here are
    // asked with categories whose type is the same object in both legs
    agree_python(
        "warncategory",
        "\
import warnings


class Mine(UserWarning):
    pass


class Deeper(Mine):
    pass


class NotAWarning:
    pass


class Liar:
    @property
    def __class__(self) -> type:
        return Mine


def instance() -> None:
    warnings.warn(Mine('inst'))


def instance_overrides(category: type) -> None:
    warnings.warn(Mine('override'), category)


def deep_instance() -> None:
    warnings.warn(Deeper('deep'))


def category_none() -> None:
    warnings.warn('none', None)


def given(category: object) -> None:
    warnings.warn('given', category)


def message(value: object) -> None:
    warnings.warn(value, Mine)


def liar() -> None:
    warnings.warn(Liar())
",
        &[
            "_warnings_from(m.instance)",
            "_warnings_from(m.instance_overrides, DeprecationWarning)",
            "_warnings_from(m.deep_instance)",
            "_warnings_from(m.category_none)",
            "_warnings_from(m.given, m.Mine)",
            "_warnings_from(m.given, m.NotAWarning)",
            "_warnings_from(m.given, int)",
            "_warnings_from(m.given, 3)",
            "_warnings_from(m.given, 'Mine')",
            "_warnings_from(m.given, UserWarning('x'))",
            "_warnings_from(m.given, None)",
            // a non-type that names `Warning` among its bases: `issubclass` says yes
            // where a type check says no, and python's own test is `issubclass` — so
            // this reaches the *call* and fails there instead. built out of
            // `SimpleNamespace` rather than a class of the module's own, because the
            // refusal names a type through `tp_name`
            "_warnings_from(m.given, _types.SimpleNamespace(__bases__=(Warning,)))",
            "_warnings_from(m.message, 5)",
            "_warnings_from(m.message, None)",
            "_warnings_from(m.liar)",
        ],
    );
}

#[test]
fn a_warning_that_walks_frames_answers_from_the_interpreted_definition() {
    // above the default stack level the frame to blame is the *caller's*, and how many
    // frames are missing under a compiled function is not a static question: a compiled
    // function calling another compiled one loses both, so nothing at the call site can
    // say how far out the real caller is. each of these is left to its interpreted
    // definition, which is why the answers still match.
    //
    // asked through `_warned_into` rather than `_warnings_from`, because a declined
    // function's frames are named `<string>`: the compiled leg reaches its interpreted
    // definition through `PyRun_String`, which has no file to name
    agree_python_with_declines(
        "warnstack",
        "\
import warnings


def far() -> None:
    warnings.warn('far', DeprecationWarning, stacklevel=2)


def further() -> None:
    warnings.warn('further', DeprecationWarning, stacklevel=3)


def computed(level: int) -> None:
    warnings.warn('computed', DeprecationWarning, stacklevel=level)


def spread(rest: tuple) -> None:
    warnings.warn('spread', *rest)


def sourced() -> None:
    warnings.warn('sourced', ResourceWarning, source=[1, 2])


def skipped() -> None:
    warnings.warn('skipped', DeprecationWarning, skip_file_prefixes=('/nowhere/',))


def skipped_over_nothing() -> None:
    warnings.warn('nothing', DeprecationWarning, skip_file_prefixes=())


def too_many() -> None:
    warnings.warn('crowded', DeprecationWarning, 1, None, ())


def unknown_keyword() -> None:
    warnings.warn('odd', DeprecationWarning, nonsense=1)
",
        &[
            "_warned_into(m, m.far)",
            "_warned_into(m, m.further)",
            "_warned_into(m, m.computed, 1)",
            "_warned_into(m, m.computed, 2)",
            "_warned_into(m, m.spread, (DeprecationWarning, 2))",
            // the `source` is on the record and nowhere else, so a lowering that
            // dropped it would be invisible to every other assertion here
            "_warned_source(m.sourced)",
            // a non-empty prefix list forces the level to at least two, and the
            // keyword does not exist at all before 3.12 — where python raises for
            // both of these and the compiled leg has to raise with it
            "_warned_into_safely(m, m.skipped)",
            "_warned_into_safely(m, m.skipped_over_nothing)",
            // shapes python refuses outright: a lowering that read the arguments it
            // knows and ignored the rest would answer where python raised
            "_warned_into_safely(m, m.too_many)",
            "_warned_into_safely(m, m.unknown_keyword)",
        ],
    );
}

/// the source both dispatch-table tests below compile
///
/// the shape `pickle.Unpickler` writes 68 times and `pprint.PrettyPrinter` 18: a table
/// bound in the class body and then filled, one entry per method, with the method the
/// body has just defined. `run` reads the table back off the instance, which is the only
/// way any of those entries is ever reached
const A_DISPATCH_TABLE: &str = "\
class Table:
    dispatch = {}

    def load_int(self, v):
        return ('int', v * 2)
    dispatch[int] = load_int

    def load_str(self, v):
        return ('str', v.upper())
    dispatch[str] = load_str

    # one method under two keys. the name is still one, so this is not the ambiguity
    # that stops a method being paired with what the type publishes
    dispatch[bool] = load_int

    def run(self, v):
        return self.dispatch[type(v)](self, v)


def through(v):
    return Table().run(v)


def entries():
    return sorted((k.__name__, v.__name__) for k, v in Table.dispatch.items())


def identities():
    return [Table.dispatch[int] is Table.load_int,
            Table.dispatch[str] is Table.load_str,
            Table.dispatch[bool] is Table.load_int]
";

#[test]
fn a_dispatch_table_a_class_body_fills_answers_the_same_either_way() {
    agree_python(
        "dispatchtable",
        A_DISPATCH_TABLE,
        &[
            "m.through(3)",
            "m.through('ab')",
            "m.through(True)",
            "m.entries()",
            "m.identities()",
        ],
    );
}

#[test]
fn a_dispatch_table_holds_the_compiled_methods_the_type_publishes() {
    // `agree` cannot see this: the interpreted definition of every one of these methods
    // is still in the module, and a table left pointing at it answers exactly the same.
    // what says which leg ran is the kind of object the table holds — a compiled type
    // publishes a `method_descriptor` where the interpreted class holds a plain function
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_dispatchtablekind");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        A_DISPATCH_TABLE,
        "by_diff_dispatchtablekind",
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
        "import by_diff_dispatchtablekind as m\n\
         print(type(m.Table.__dict__['load_int']).__name__)\n\
         print(type(m.Table.dispatch[int]).__name__)\n\
         print(m.identities())\n",
    );
    assert_eq!(
        out,
        "method_descriptor\nmethod_descriptor\n[True, True, True]"
    );
}

#[test]
fn a_method_the_class_body_bound_under_a_second_name_is_left_where_it_stands() {
    // `show = render` puts one function under two names in the body, and there is no
    // single compiled method it should become — so the table keeps what the body wrote.
    // the entry then agrees with `show`, which is a copy of that same function, and both
    // legs call the same body
    agree_python(
        "dispatchaliased",
        "\
class Aliased:
    def render(self):
        return 'r'

    show = render
    table = {'r': render}

    def call(self):
        return self.table['r'](self)


def through():
    return [Aliased().call(), Aliased.table['r'] is Aliased.show]
",
        &["m.through()"],
    );
}

#[test]
fn a_class_body_writing_one_value_under_two_names_agrees() {
    agree_python(
        "chainedclassconstant",
        "\
class Bounds:
    low = high = 3

    def span(self):
        return (self.low, self.high, self.low is self.high)


def through():
    return Bounds().span()
",
        &["m.through()", "[m.Bounds.low, m.Bounds.high]"],
    );
}

#[test]
fn the_str_of_an_int_agrees() {
    // the digits are written straight into an ascii string rather than reached through
    // a `PyLong` and `int.__str__`, so every property of the answer the formatter used
    // to establish has to be established here: the sign, the length the object claims,
    // the terminator a compact string carries past its last character, and the whole of
    // the range on either side of the one a machine word holds
    agree_python(
        "strofint",
        "\
import sys


def digits(n: int) -> str:
    return str(n)


def joined(n: int) -> str:
    return 'k' + str(n)


def each(values: list[int]) -> list[str]:
    return [str(v) for v in values]


def counted(n: int) -> str:
    last = ''
    i = -3
    while i < n:
        last = str(i)
        i = i + 1
    return last


def edges() -> list[str]:
    return [
        str(sys.maxsize),
        str(-sys.maxsize - 1),
        str(sys.maxsize + 1),
        str(-sys.maxsize - 2),
        str(10 ** 30),
        str(-(10 ** 30)),
    ]
",
        &[
            "[m.digits(n) for n in (0, 1, -1, 9, -9, 10, 99, 100, 256, 257, -12345)]",
            "[m.joined(n) for n in (0, 7, -7)]",
            "m.each([-2, -1, 0, 1, 2, 10 ** 25])",
            "m.counted(4)",
            "m.edges()",
            // the answer is a whole `str` and not merely one that prints right: an
            // object whose claimed length or terminator disagreed with its digits
            // would still compare equal on the ones it did hold. the hash is asked
            // against this same process's `str`, because python salts it per run
            "[(len(m.digits(n)), hash(m.digits(n)) == hash(str(n)), m.digits(n).encode()) \
             for n in (0, -1, -12345, 10 ** 20)]",
            "[type(m.digits(3)).__name__, m.digits(3) == '3']",
        ],
    );
}

#[test]
fn a_str_the_module_rebinds_is_the_one_called() {
    // the fast path rests on the name still resolving to the builtin, which is asked
    // every trip rather than assumed once. a module that writes its own `str` into its
    // namespace — which a compiled function's `globals()` reaches — has to be obeyed,
    // and obeyed again when it puts the builtin back
    agree_python(
        "strofintrebind",
        "\
from typing import Any

builtin: Any = str


def shouted(n: object) -> str:
    return '<' + builtin(n) + '>'


def digits(n: int) -> str:
    return str(n)


def rebind() -> None:
    globals()['str'] = shouted


def restore() -> None:
    globals()['str'] = builtin
",
        &["[m.digits(41), (m.rebind(), m.digits(41))[1], (m.restore(), m.digits(41))[1]]"],
    );
}

#[test]
fn a_str_subclass_bound_to_the_name_is_the_one_called() {
    // the guard is an identity test against the builtin type object, so a *subclass* of
    // `str` bound to the name fails it and is constructed the ordinary way — which is
    // the only answer that keeps the result's type
    agree_python(
        "strofintsubclass",
        "\
from typing import Any


class Loud(str):
    pass


builtin: Any = str


def digits(n: int) -> str:
    return str(n)


def rebind() -> None:
    globals()['str'] = Loud


def restore() -> None:
    globals()['str'] = builtin
",
        &[
            "[(m.rebind(), type(m.digits(7)).__name__, m.digits(7))[1:], \
              (m.restore(), type(m.digits(7)).__name__)[1]]",
        ],
    );
}

#[test]
fn the_str_of_an_int_boxes_nothing_and_still_resolves_the_name() {
    let Some((_, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_strofint_shape");
    let _ = std::fs::remove_dir_all(&dir);
    // nothing is concatenated onto the digits, so this is the conversion on its own
    // — a prefix in front of it would be fused further still, into `By_StrConcatInt`
    let source = "\
def keys(n: int) -> str:
    last = 'k'
    i = 0
    while i < n:
        last = str(i)
        i = i + 1
    return last
";
    let options = Options {
        language: by_irbuild::Language::Python,
        ..Options::default()
    };
    if build_source(source, "by_diff_strofint_shape", &toolchain, &dir, &options).is_err() {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let emitted = std::fs::read_to_string(dir.join("by_diff_strofint_shape.c"))
        .expect("the generated C is written beside the extension");
    let body = emitted_function(&emitted, "by_by_diff_strofint_shape_keys");
    // the conversion is fused, the argument's `PyLong` is gone with it, and the
    // resolution the fusion is guarded on is still made — through the memo, which
    // re-derives it whenever a namespace has been written to since the last answer
    assert!(body.contains("By_StrOfInt("), "{body}");
    assert!(!body.contains("By_BoxInt("), "{body}");
    assert!(
        body.contains("By_LookupGlobalSite(&by_gs_str, by_module_dict, by_g_str)"),
        "{body}"
    );
    assert!(
        body.contains("static ByGlobalSite by_gs_str = BY_GLOBAL_SITE_INIT;"),
        "{body}"
    );
}

#[test]
fn the_str_of_an_int_in_a_loop_does_not_leak() {
    // the fast path allocates the answer itself and the slow one boxes an argument to
    // throw away, so a reference kept or dropped on either shows up as growth and
    // nowhere else
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_strofintleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def build(n: int, base: int) -> str:
    last = ''
    i = 0
    while i < n:
        last = 'k' + str(base + i)
        i = i + 1
    return last
";
    let options = Options {
        language: by_irbuild::Language::Python,
        ..Options::default()
    };
    if build_source(source, "by_diff_strofintleak", &toolchain, &dir, &options).is_err() {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import gc, by_diff_strofintleak as m\n\
         big = 2**80\n\
         m.build(50, 0); m.build(50, big)\n\
         gc.collect(); before = len(gc.get_objects())\n\
         print(m.build(8000, 0) == 'k7999')\n\
         print(m.build(8000, big) == 'k' + str(big + 7999))\n\
         gc.collect(); after = len(gc.get_objects())\n\
         print('stable' if after <= before + 100 else f'grew {before}->{after}')\n",
    );
    assert_eq!(out, "True\nTrue\nstable");
}

#[test]
fn a_prefix_and_the_digits_of_an_int_take_one_allocation() {
    let Some((_, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_strconcatint_shape");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def keys(n: int) -> str:
    last = 'k'
    i = 0
    while i < n:
        last = 'k' + str(i)
        i = i + 1
    return last


def joined(n: int) -> str:
    out = ''
    i = 0
    while i < n:
        out = out + str(i)
        i = i + 1
    return out
";
    let options = Options {
        language: by_irbuild::Language::Python,
        ..Options::default()
    };
    if build_source(
        source,
        "by_diff_strconcatint_shape",
        &toolchain,
        &dir,
        &options,
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let emitted = std::fs::read_to_string(dir.join("by_diff_strconcatint_shape.c"))
        .expect("the generated C is written beside the extension");

    // the digits, the check on them and the concatenation are one operation, so the
    // string the digits used to be built into is gone — and with it the second
    // allocation and the copy back out of it. the resolution the fast path is
    // guarded on is still made, through the same memo the unfused shape used
    let body = emitted_function(&emitted, "by_by_diff_strconcatint_shape_keys");
    assert!(body.contains("By_StrConcatInt("), "{body}");
    assert!(!body.contains("By_StrOfInt("), "{body}");
    assert!(!body.contains("By_StrConcat("), "{body}");
    assert!(!body.contains("By_UnboxStr("), "{body}");
    assert!(
        body.contains("By_LookupGlobalSite(&by_gs_str, by_module_dict, by_g_str)"),
        "{body}"
    );

    // an accumulation is left alone: `str_append` has already turned it into a
    // resize in place, and a chain of those is linear where a chain of copies would
    // be quadratic — which is worth more than the one small allocation fusing it
    // would save
    let body = emitted_function(&emitted, "by_by_diff_strconcatint_shape_joined");
    assert!(body.contains("By_StrAppend("), "{body}");
    assert!(!body.contains("By_StrConcatInt("), "{body}");
}

#[test]
fn an_intermediate_the_program_can_still_see_is_not_fused_away() {
    // `t = str(i)` is the program's own value, and the fusion is only entitled to
    // remove a string nothing outside it can reach. `twice` reads the intermediate a
    // second time, so removing the operation that fills it would leave that read
    // looking at a register nothing ever wrote; `once` reads it only the once but
    // gives it a name, which is where this pass stops asking — a named register is
    // the program's, and whether the string it holds was built is not this pass's to
    // decide
    let Some((_, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_strconcatint_shared");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def twice(i: int) -> str:
    t = str(i)
    return 'k' + t + t


def once(i: int) -> str:
    t = str(i)
    return 'k' + t
";
    let options = Options {
        language: by_irbuild::Language::Python,
        ..Options::default()
    };
    if build_source(
        source,
        "by_diff_strconcatint_shared",
        &toolchain,
        &dir,
        &options,
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let emitted = std::fs::read_to_string(dir.join("by_diff_strconcatint_shared.c"))
        .expect("the generated C is written beside the extension");
    for name in ["twice", "once"] {
        let body = emitted_function(&emitted, &format!("by_by_diff_strconcatint_shared_{name}"));
        assert!(body.contains("By_StrOfInt("), "{name}: {body}");
        assert!(!body.contains("By_StrConcatInt("), "{name}: {body}");
    }
}

#[test]
fn fusing_a_prefix_onto_the_digits_does_not_change_what_a_bad_str_raises() {
    // a rebound `str` may hand back anything, and what the unfused shape did with
    // that is raise from its own unbox — before the concatenation, and in the
    // compiler's own words rather than the interpreter's. the fused operation makes
    // the same check in the same place, so the two shapes have to say the same thing.
    //
    // that they say something other than what cpython says for `'k' + 1` is older
    // than this fusion and is what any annotated `-> str` already answers; the point
    // here is only that fusing did not move it
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_strconcatint_badstr");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
from typing import Any

builtin: Any = str


def counted(n: object) -> int:
    return 1


def fused(n: int) -> str:
    return 'k' + str(n)


def unfused(n: int) -> str:
    t = str(n)
    return 'k' + t
";
    let options = Options {
        language: by_irbuild::Language::Python,
        ..Options::default()
    };
    if build_source(
        source,
        "by_diff_strconcatint_badstr",
        &toolchain,
        &dir,
        &options,
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import by_diff_strconcatint_badstr as m\n\
         m.__dict__['str'] = m.counted\n\
         said = []\n\
         for fn in (m.fused, m.unfused):\n\
         \x20   try:\n\
         \x20       fn(7)\n\
         \x20       said.append('no error')\n\
         \x20   except TypeError as e:\n\
         \x20       said.append(f'{type(e).__name__}: {e}')\n\
         print(said[0])\n\
         print('same' if said[0] == said[1] else f'differs: {said[1]}')\n",
    );
    assert_eq!(out, "TypeError: expected str, got int\nsame");
}

#[test]
fn a_prefix_and_the_digits_agree_at_every_width_sign_and_magnitude() {
    // the fused allocation is sized before either half is written, so a length it
    // gets wrong is memory it writes past. what settles the length is the prefix's
    // own — including none at all — and how many digits the integer needs; what
    // settles the *width* is the prefix's storage, since a decimal is all ascii and
    // widens nothing.
    //
    // the magnitudes bracket the tagged representation: either side of the largest
    // value a tagged word carries, and either side of the widest an ssize_t reaches.
    // past those the integer is an object and the slow path answers
    agree_python(
        "strconcatintwidths",
        "\
def plain(i: int) -> str:
    return 'k' + str(i)


def empty(i: int) -> str:
    return '' + str(i)


def latin(i: int) -> str:
    return '\\u00e9' + str(i)


def wide(i: int) -> str:
    return '\\u65e5' + str(i)


def widest(i: int) -> str:
    return '\\U0001d11e' + str(i)


def long_prefix(i: int) -> str:
    return 'abcdefghijklmnopqrstuvwxyz0123456789' + str(i)


def around(i: int) -> str:
    return 'a' + str(i) + 'b' + str(i + 1)
",
        &[
            "[[f(v) for f in [m.plain, m.empty, m.latin, m.wide, m.widest, m.long_prefix, m.around]] \
              for v in [0, 1, 9, 10, -1, -9, -10, 123456789, -123456789]]",
            "[[f(v) for f in [m.plain, m.empty, m.latin, m.wide, m.widest, m.long_prefix, m.around]] \
              for v in [4611686018427387903, 4611686018427387904, \
                        -4611686018427387904, -4611686018427387905]]",
            "[[f(v) for f in [m.plain, m.empty, m.latin, m.wide, m.widest, m.long_prefix, m.around]] \
              for v in [9223372036854775807, -9223372036854775808, 2 ** 70, -(2 ** 70)]]",
            // the storage a string is kept in is the narrowest its characters fit,
            // and the answer's characters are the prefix's plus ascii — so a result
            // stored any wider than its prefix is one cpython would never build
            "[[len(f(v)), max(ord(c) for c in f(v))] \
              for f in [m.plain, m.empty, m.latin, m.wide, m.widest] \
              for v in [7, -7, 2 ** 70]]",
            // an intermediate the program keeps is not fused away, and reads the same
            "[m.around(-1), m.around(2 ** 70)]",
        ],
    );
}

#[test]
fn a_prefix_and_the_digits_obey_a_rebound_name() {
    // the name is resolved through the module namespace on every trip and the answer
    // is compared against the builtin rather than assumed, so a module that rebinds
    // `str` is obeyed — and what a rebound one hands back is checked to be a `str`
    // before it is concatenated, because it may be anything at all
    agree_python(
        "strconcatintrebound",
        "\
from typing import Any


class Loud(str):
    pass


builtin: Any = str


def shouted(n: object) -> str:
    return '<' + builtin(n) + '>'


def counted(n: object) -> int:
    return 1


def prefixed(n: int) -> str:
    return 'k' + str(n)


def rebind(fn: Any) -> None:
    globals()['str'] = fn


def restore() -> None:
    globals()['str'] = builtin
",
        &[
            "[m.prefixed(7), (m.rebind(m.shouted), m.prefixed(7))[1], \
              (m.restore(), m.prefixed(7))[1]]",
            // a *subclass* bound to the name fails the identity test, so it is
            // constructed the ordinary way and its characters are what is joined on
            "[(m.rebind(m.Loud), m.prefixed(7), type(m.prefixed(7)).__name__)[1:], \
              (m.restore(), m.prefixed(7))[1]]",
            // and one that hands back something that is not a `str` at all raises,
            // where the unfused shape's own check raised
            "[type(e).__name__ for e in \
              [(m.rebind(m.counted), _capture(m.prefixed, 7))[1]]] \
             + [(m.restore(), m.prefixed(7))[1]]",
        ],
    );
}

#[test]
fn a_prefix_and_the_digits_in_a_loop_do_not_leak() {
    // the fast path allocates the answer itself and the slow one allocates the digits
    // and then throws them away, so a reference kept or dropped on either shows up as
    // growth and nowhere else
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_strconcatintleak");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def build(n: int, base: int) -> str:
    last = ''
    i = 0
    while i < n:
        last = '\\u65e5k' + str(base + i)
        i = i + 1
    return last
";
    let options = Options {
        language: by_irbuild::Language::Python,
        ..Options::default()
    };
    if build_source(
        source,
        "by_diff_strconcatintleak",
        &toolchain,
        &dir,
        &options,
    )
    .is_err()
    {
        eprintln!("skipping: no working C toolchain");
        return;
    }
    let out = run(
        &python,
        &dir,
        "import gc, by_diff_strconcatintleak as m\n\
         big = 2**80\n\
         m.build(50, 0); m.build(50, big)\n\
         gc.collect(); before = len(gc.get_objects())\n\
         print(m.build(8000, 0) == '\\u65e5k7999')\n\
         print(m.build(8000, big) == '\\u65e5k' + str(big + 7999))\n\
         gc.collect(); after = len(gc.get_objects())\n\
         print('stable' if after <= before + 100 else f'grew {before}->{after}')\n",
    );
    assert_eq!(out, "True\nTrue\nstable");
}

#[test]
fn an_indexed_read_agrees_on_every_container_and_at_every_edge() {
    // only the element read is written at the call site now; the message an
    // out-of-range index carries, and the protocol a container the fast path does
    // not know takes, both live behind one call. so what has to agree is every
    // shape that leaves the fast path — and every shape that stays on it
    agree_python(
        "indexedread",
        "\
def at(xs: object, i: int) -> object:
    return xs[i]

def scan(xs: object) -> int:
    total = 0
    i = 0
    while i < len(xs):
        total = total + 1
        i = i + 1
    return total


class Own:
    def __getitem__(self, i: int) -> str:
        return 'own' + str(i)


class Widened(list):
    pass
",
        &[
            // the three the fast path knows, forwards and backwards
            "[m.at([10, 20, 30], 0), m.at([10, 20, 30], 2), m.at([10, 20, 30], -1)]",
            "[m.at((10, 20, 30), 0), m.at((10, 20, 30), -3)]",
            "[m.at('abc', 0), m.at('abc', 2), m.at('abc', -1)]",
            // past either end of each, where the message is what has to match
            "[(type(e).__name__, str(e)) for e in [_capture(m.at, [1, 2], 5)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.at, [1, 2], -5)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.at, (1, 2), 5)]]",
            "[(type(e).__name__, str(e)) for e in [_capture(m.at, 'ab', 5)]]",
            // a subclass of one the fast path knows is not one the fast path knows
            "m.at(m.Widened([7, 8]), 1)",
            // a container of its own, and one with no length at all
            "[m.at(m.Own(), 4), m.scan([1, 2, 3]), m.scan('abcd'), m.scan((1, 2))]",
            "[(type(e).__name__) for e in [_capture(m.at, 5, 0)]]",
            "[(type(e).__name__) for e in [_capture(m.at, {'k': 1}, 0)]]",
            // a mapping answers a key the sequence fast path would have refused
            "m.at({0: 'zero', 1: 'one'}, 1)",
        ],
    );
}

#[test]
fn a_returned_value_outlives_the_releases_that_follow_it() {
    // a `return` now hands the caller the reference the frame already held rather
    // than taking a second one, so the frame must not release that register on the
    // way out. what makes the question sharp is that the releases it *does* still
    // run can each run a `__del__`, which is arbitrary python — so the value being
    // handed back has to survive one running in between
    agree_python(
        "returnedmove",
        "\
class Loud:
    def __init__(self, log: list) -> None:
        self.log = log

    def __del__(self) -> None:
        self.log.append('gone')


def pick(log: list, n: int) -> str:
    watcher = Loud(log)
    answer = 'v' + str(n)
    return answer


def pick_early(log: list, n: int) -> str:
    watcher = Loud(log)
    if n > 0:
        return 'high' + str(n)
    return 'low' + str(n)


def pick_finally(log: list, n: int) -> str:
    watcher = Loud(log)
    try:
        return 'try' + str(n)
    finally:
        log.append('ran')
",
        &[
            "[m.pick([], 1), m.pick([], 2)]",
            "[m.pick_early([], 3), m.pick_early([], -3)]",
            "m.pick_finally([], 4)",
            // the `__del__` runs, and it runs before the caller sees the answer
            "[_l := [], m.pick(_l, 5), _l][2]",
            "[_l := [], m.pick_finally(_l, 6), _l][2]",
            // the answer is not a borrowed reference into a frame that has gone:
            // building many of them and holding them all would find one freed once
            // too often
            "[m.pick([], i) for i in range(200)][-1]",
            "len({m.pick([], i) for i in range(200)})",
        ],
    );
}

/// the shapes a written `__new__` takes, in one module
///
/// `__new__` is python's allocator hook, and the whole of what an emitted class has to
/// get right about it is that python's own construction decides what happens next: it
/// runs `__init__` only where `__new__` answered with an instance of the class that was
/// asked for. so a constructor that interns, one that answers with something else
/// entirely, and one that allocates and fills are the same mechanism seen from three
/// sides
const WRITTEN_NEW: &str = "\
class Point:
    __slots__ = ('x', 'y')

    def __new__(cls, x, y):
        self = object.__new__(cls)
        self.x = x + 1
        self.y = y * 2
        return self

    def read(self):
        return (self.x, self.y)


class Initialised:
    __slots__ = ('a', 'b')

    def __new__(cls, a, b=5):
        self = object.__new__(cls)
        self.a = a
        return self

    def __init__(self, a, b=5):
        self.b = b + len(type(self).__name__)

    def read(self):
        return (self.a, self.b)


class Under(Initialised):
    __slots__ = ()

    def named(self):
        return type(self).__name__


class Deeper(Initialised):
    __slots__ = ()

    def __init__(self, a, b=5):
        self.b = b * 100


CACHE = {}


class Interned:
    __slots__ = ('key',)

    def __new__(cls, key):
        held = CACHE.get(key)
        if held is not None:
            return held
        self = object.__new__(cls)
        self.key = key
        CACHE[key] = self
        return self

    def read(self):
        return self.key


def points():
    return [Point(1, 2).read(), Point(0, 0).read(), Point(x=4, y=5).read()]


def initialised():
    return [Initialised(1).read(), Initialised(1, 2).read(), Initialised(b=3, a=9).read()]


def under():
    it = Under(7, 1)
    # `Deeper` writes an `__init__` of its own and inherits the `__new__` above it, which
    # is the shape a construction lowered to a plain allocation would skip
    deeper = Deeper(3, 2)
    return [it.read(), it.named(), isinstance(it, Initialised), deeper.read()]


def interned():
    first = Interned('k')
    second = Interned('k')
    return [first is second, first.read(), Interned('other') is first]


def arity():
    out = []
    for call in (lambda: Point(1), lambda: Point(1, 2, 3), lambda: Point(1, 2, z=3)):
        try:
            call()
        except TypeError as error:
            out.append(str(error))
        else:
            out.append('no error')
    return out
";

#[test]
fn a_written_new_agrees_on_every_shape_of_construction() {
    agree_python(
        "writtennew",
        WRITTEN_NEW,
        &[
            "m.points()",
            "m.initialised()",
            "m.under()",
            "m.interned()",
            "m.arity()",
            "[m.Point(3, 4).x, m.Point(3, 4).y]",
            "m.Under(2, 2).read()",
        ],
    );
}

#[test]
fn a_written_new_is_the_one_the_compiled_type_runs() {
    // `agree` cannot see this: the interpreted definition of `__new__` is still in the
    // module, and a class left standing on it answers identically. what says which leg
    // ran is the kind of object under the name — a compiled class publishes a
    // `staticmethod` over a builtin, where the interpreted one holds a plain function
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = diff_root().join("by_diff_writtennewkind");
    let _ = std::fs::remove_dir_all(&dir);
    let built = match build_source(
        WRITTEN_NEW,
        "by_diff_writtennewkind",
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
        "import by_diff_writtennewkind as m\n\
         print(type(m.Point.__dict__['__new__']).__name__)\n\
         print(type(m.Point.__dict__['__new__'].__func__).__name__)\n\
         print(type(m.Point.__dict__['read']).__name__)\n\
         print(type(m.Initialised.__dict__['__init__']).__name__)\n\
         print(m.Point(1, 2).read())\n",
    );
    assert_eq!(
        out,
        "staticmethod\nbuiltin_function_or_method\nmethod_descriptor\nwrapper_descriptor\n(2, 4)"
    );
}

#[test]
fn a_written_new_that_reaches_a_base_outside_the_module_is_declined() {
    // the allocation is the base's: only `tuple` knows how big one of its instances is,
    // and the subclass check python runs on the way into that allocator reads the emitted
    // type as an allocator of its own. so the class is left where it stands, and the
    // construction it publishes is the interpreted one
    agree_python_with_declines(
        "newoveratuple",
        "\
class Pair(tuple):
    def __new__(cls, a, b):
        return super().__new__(cls, (a, b))

    def total(self):
        return self[0] + self[1]


def through():
    it = Pair(2, 3)
    return [tuple(it), it.total(), isinstance(it, tuple)]
",
        &["m.through()"],
    );
}

#[test]
fn a_new_that_answers_another_class_is_declined() {
    // python lets `__new__` answer with anything and runs `__init__` only where the answer
    // is an instance of the class asked for. the checker does not follow it, so every
    // construction in the module is compiled believing it got a `Diverting` — and the
    // boundary that catches the mismatch raises where the interpreted class hands the
    // object straight over
    agree_python_with_declines(
        "newdiverts",
        "\
class Elsewhere:
    __slots__ = ('tag',)

    def __init__(self, tag):
        self.tag = tag


class Diverting:
    __slots__ = ('never',)

    def __new__(cls, tag):
        return Elsewhere(tag)

    def __init__(self, tag):
        raise AssertionError('__init__ ran after __new__ answered another class')


def through():
    it = Diverting('t')
    return [type(it).__name__, it.tag, isinstance(it, Diverting)]
",
        &["m.through()"],
    );
}

#[test]
fn a_new_the_class_body_assigns_rather_than_defines_is_declined() {
    // an assignment binds the very name python fills from the allocator slot, so the two
    // would answer differently: the construction reaches the slot, and `Box.__new__`
    // reaches whatever the body put under the name
    agree_python_with_declines(
        "newassigned",
        "\
def make(cls, tag):
    self = object.__new__(cls)
    self.tag = tag
    return self


class Box:
    __slots__ = ('tag',)

    __new__ = make

    def read(self):
        return self.tag


def through():
    return [Box('a').read(), Box('b').read()]
",
        &["m.through()"],
    );
}

#[test]
fn a_written_hash_larger_than_a_slot_holds_agrees() {
    // `tp_hash` is a `Py_hash_t`, and python's own conversion takes the answer as it
    // stands wherever it fits one — only a value too large is folded, through
    // `int.__hash__`, into the range a hash occupies. hashing every answer instead folded
    // the large ones a second time, which is how a cached hash of a state tuple came back
    // as a number the interpreted class never produced
    agree_python(
        "hashwidth",
        "\
class Cached:
    __slots__ = ('value',)

    def __init__(self, value):
        self.value = value

    def __eq__(self, other):
        return self.value == other.value

    def __hash__(self):
        return self.value


def hashes(values):
    return [hash(Cached(v)) for v in values]
",
        &[
            "m.hashes([0, 1, -1, -2, 7])",
            "m.hashes([2**61 - 2, 2**61 - 1, 2**61, 2**62, 2**63 - 1])",
            "m.hashes([-(2**61), -(2**62), -(2**63)])",
            "m.hashes([2**70, -(2**70), 2**200])",
            "hash(m.Cached(3010437511937009226))",
        ],
    );
}
