# basedpython: `once` callbacks are called exactly once

A `once` callback parameter must be called exactly once on every path that completes the function
normally. Forgetting to call it is `once-not-called`; calling it twice — two unconditional calls, or
a call inside a loop — is `once-called-twice`. The analysis is conservative: mutually-exclusive
branch calls count as a single call, and a callback that is merely passed elsewhere is left alone.

```toml
[environment]
python-version = "3.12"
```

## calling exactly once is fine

```by
def f(once done: () -> None):
    done()
```

## never calling is rejected

```by
def f(once done: () -> None):  # error: [once-not-called] "once callback `done` is never called"
    do_work()

def do_work() -> None: ...
```

## two unconditional calls are rejected

```by
def f(once done: () -> None):
    done()
    done()  # error: [once-called-twice] "once callback `done` may be called more than once"
```

## a call inside a loop is rejected

```by
def f(once done: () -> None):
    for _ in range(3):
        done()  # error: [once-called-twice]
```

## calling once in each branch is fine

Every path calls it exactly once.

```by
def f(once done: () -> None, c: bool):
    if c:
        done()
    else:
        done()
```

## a single conditional call is accepted

The static analysis does not flag the path where the branch is skipped; the runtime guard is the
tool for that.

```by
def f(once done: () -> None, c: bool):
    if c:
        done()
```

## a `local` modifier inside the callback type is fine

`local` / `once` inside the callback's own signature parse and strip, so the enclosing `once` check
still sees a single, correct call — no spurious errors.

```by
def f(once fn: (local int) -> None):
    fn(1)
```

## passing the callback on to another `once` is not "never called"

A `once` callback is a borrow, so it may only be handed to another `once` parameter. That still
counts as a use, so `once-not-called` stays silent.

```by
def run(once cb: () -> None):
    cb()

def f(once done: () -> None):
    run(done)
```

## a `once` callback cannot escape the call

`once` is a `local` borrow with an extra "exactly once" obligation — it cannot escape, since a
callback that outlives the call can no longer be guaranteed to run exactly once.

```by
_saved: object = None

def f(once done: () -> None) -> object:
    global _saved
    _saved = done  # error: [escaping-local] "once `done` cannot escape the call: it is stored where it outlives the call"
    return done  # error: [escaping-local] "once `done` cannot escape the call: it is returned from the call"
```

## a `once` callback may only be passed to another `once`

Handing it to a plain (non-`once`) parameter would delegate the exactly-once obligation to code that
is not required to discharge it, so it is rejected — even a `local` recipient, which could call it
zero or many times.

```by
def sink(cb: () -> None):
    cb()

def borrow(local cb: () -> None):
    cb()

def keep(once cb: () -> None):
    cb()

def f(once done: () -> None):
    sink(done)  # error: [escaping-local] "once `done` cannot escape the call: it is passed as a non-`once` argument"
    borrow(done)  # error: [escaping-local]
    keep(done)  # ok — the obligation is preserved
```

## a callback's parameter may be declared `once`

A callable type can mark one of its own parameters `once`, which obliges whatever implements that
callable to call it exactly once. A trailing lambda block's implicit `it` binds that position, so
the block carries the obligation.

```by
def with_retry(once fn: (once cb: (int) -> None) -> None):
    fn(print)

# error: [once-not-called] "once callback `it` is never called"
with_retry:
    pass
```

## a `once` callback parameter may not be called twice

```by
def with_retry(once fn: (once cb: (int) -> None) -> None):
    fn(print)

with_retry:
    it(1)
    it(2)  # error: [once-called-twice] "once callback `it` may be called more than once"
```

## calling the borrowed `once` callback exactly once is fine

```by
def with_retry(once fn: (once cb: (int) -> None) -> None):
    fn(print)

with_retry:
    it(1)
```

## a `once` callback with a receiver is called through it

A callback declared with an [implicit receiver](basedpython_implicit_receiver.md) is called as
`x.cb()`, which is the same call as `cb(x)`. It discharges the obligation.

```by
class Html: ...

def build(once builder: Html.() -> None) -> Html:
    result = Html()
    result.builder()
    return result
```

## a receiver call counts toward the twice check

```by
class Html: ...

def build(once builder: Html.() -> None) -> Html:
    result = Html()
    result.builder()
    result.builder()  # error: [once-called-twice] "once callback `builder` may be called more than once"
    return result
```

## a real member of the same name is not the callback

`shout` is declared on `Text`, so `value.shout()` is that method — the receiver callable is only
ever the last fallback, and the obligation is still outstanding.

```by
class Text:
    def shout(self) -> None: ...

# error: [once-not-called] "once callback `shout` is never called"
def build(once shout: Text.() -> None) -> None:
    value = Text()
    value.shout()
```
