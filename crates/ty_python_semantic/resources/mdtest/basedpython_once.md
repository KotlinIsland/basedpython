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

## passing the callback on is not "never called"

`run` might call it, so `once-not-called` stays silent.

```by
def run(cb: () -> None):
    cb()

def f(once done: () -> None):
    run(done)
```
