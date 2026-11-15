# error traces

> **status.** a design, not yet implemented. nothing below is built

## summary

an error should be able to tell its whole story: where it came from, how it got
to where it was handled, and what it was turned into on the way. python's
traceback tells a piece of that story per exception, and a chain of exceptions
prints those pieces as separate segments that the reader stitches together by
hand

```python
def f():
    g()

def g():
    try:
        h()
    except Exception as e:
        raise KeyError from e

def h():
    raise TypeError

f()
```

python prints:

```text
Traceback (most recent call last):
  File "demo.py", line 6, in g
    h()
  File "demo.py", line 11, in h
    raise TypeError
TypeError

The above exception was the direct cause of the following exception:

Traceback (most recent call last):
  File "demo.py", line 13, in <module>
    f()
  File "demo.py", line 2, in f
    g()
  File "demo.py", line 8, in g
    raise KeyError from e
KeyError
```

basedpython prints one tree:

```text
error: KeyError
  caused by: TypeError

  demo:13
    f()
  demo:2 in f
    g()
  demo in g
  ├─ :6
  │    h()
  │  └─ demo:11 in h
  │       raise TypeError             ← TypeError created
  ├─ :7  caught by `except Exception as e`
  └─ :8
       raise KeyError from e          ← KeyError created
```

the frames both errors share are printed once, the tree branches inside the one
frame where the two stories split, and the headline is the error the program
actually failed with

## why python's output is hard to read

the chain itself is not the problem. two facts about `__traceback__` are

### a traceback only records unwinding

a traceback holds the frames an exception passed through between the `raise`
and the handler that caught it, and nothing else. in the example the cause's
segment starts at `g`, because `g` caught it, so it never says how `g` was
reached. the wrapper's segment has the top of the stack but none of the cause's
frames. `g` appears in both, at line 6 and at line 8, and nothing says that it
is one call of `g` and the place where the story forks

the same limit shows without any chaining. an error caught and handed elsewhere
loses everything above its catch point:

```python
import traceback

def f():
    raise RuntimeError

def x():
    try:
        f()
    except Exception as e:
        return e

e = x()
traceback.print_exception(e)  # prints x and f, never the `e = x()` that called x
```

and that part cannot be recovered when the error is printed. the catch frame's
`f_back` is its caller's live frame, not a record of where the caller was: read
after `x` returned, it reports the line the module is running now rather than
the line that called `x`, and a caller that has itself returned is gone. what is
above the catch point has to be recorded when the error is caught

### an error that is never raised has no traceback at all

a [wrapped result](../features/wrapped-results.md) carries its error as a value:

```by
def parse_port(text: str) -> int ? ValueError:
    return ValueError(f"not a port: {text}")
```

that `ValueError` is never raised, so it has no traceback. by the time it
surfaces, three `^` later, nothing says where it was made. for errors as values,
recording where they were created is not an improvement on the traceback, it is
the only location the error will ever have

## the trace

every error can carry a **trace**, the record of the frames python's traceback
does not keep. it has two parts:

- the **origin**: the stack the error belongs to, from the root of the program
    down to where it was created, or down to where it was caught
- the **path**: each `^` the error passed through, in order

the trace never holds frame objects. each entry is a code object and an
instruction offset, which is enough to name the module, the function
(`co_qualname`, or `co_name` before python 3.11), the line and the column
(`co_positions`) when the error is printed, and holds none of the frame's locals.
an error stored in a long-lived structure keeps nothing else alive, which is
the memory leak `except ... as e` deletes `e` to avoid. entries resolve to
plain strings, so a trace survives pickling and crossing a process boundary

the trace lives on the error itself, as `__by_trace__`, so it travels wherever
the error goes and is printed wherever the error is printed. an exception class
written in python, or one built in C, carries a trace as well as one written in
basedpython

### when the origin is recorded

an origin is recorded at the moment an error stops being raised and becomes a
value. there are two such moments

**an error caught by name.** `except ... as e` binds a value that may outlive the
handler. the frames above the catch point are recorded there, when they are
still running and still at the lines that led to the catch. the traceback
already holds the frames from the catch point down to the `raise`, so the two
together are the whole stack. a handler that names nothing records nothing,
since nothing can escape it:

```by
try:
    h()
except KeyError:  # nothing recorded
    pass
```

**an error made as a value.** returning an error from a function that returns a
[result](../features/wrapped-results.md) records the stack where it was created.
an error that already has an origin keeps it, so a result returned through ten
functions records once

an error that is raised and never caught needs no record: its traceback reaches
the top of the program by itself

python code calling basedpython code is covered, since the recording happens in
the basedpython frame that catches or creates the error. an error caught by name
in python code is not, unless the `foreign` option below is on

### what `^` records

in the simple case the path repeats the origin: an error created three calls
down and propagated by `^` three times passes back up the same frames it was
created in. the path earns its place in two other cases

**conversion.** when `^` propagates an error that the enclosing function's
result does not name, it converts it through the enclosing error type's
[`__from__`](../features/conversions.md), links the original as the new error's
`__cause__`, and records the site:

```by
class ConfigError(Exception):
    @classmethod
    def __from__(cls, error: OSError) -> Self:
        return cls(f"could not load: {error}")

def load(path: str) -> Config ? ConfigError:
    text = read(path)^  # an OSError becomes a ConfigError here
    ...
```

this is the wrapping `try` / `except ... as e` / `raise ... from e` writes by
hand, with no handler to write and nothing to forget. both errors belong to one
stack, so they print as one tree

**divergence.** an error stored, handed to a callback, collected into a list or
sent to another thread or task travels along a path that has nothing to do with
the stack it was created on. the `^` sites are the only record of that path

every `^` records its site. deciding at runtime whether the site repeats the
origin would mean comparing frames, so the printer is the one that shows where
the path leaves the origin rather than the recorder guessing

### every link is deliberate

python links two errors implicitly when a handler raises, as `__context__`, and
prints them as "during handling of the above exception, another exception
occurred". it is the hardest chain to read, and it is usually either noise or a
bug in the handler

in a `.by` file, a `raise` inside a handler must say what it is linked to:

```by
try:
    h()
except Exception as e:
    raise KeyError  # error: [implicit-exception-context]
```

`raise KeyError from e` links it as the cause, and `raise KeyError from None`
links nothing. a bare `raise`, which re-raises the caught error, needs neither.
an implicit context that still arises, from python code, is printed as a single
`while handling ...` line rather than a segment of its own

## printing

### the tree

the printer builds one tree from an error, its `__cause__` chain, their
tracebacks and their traces:

- the headline is the error the program failed with, followed by its causes in
    order, so the first line read is the one that matters
- frames shared by an error and its cause are printed once, and the tree
    branches inside the frame where they split. a wrapper is almost always made
    in a frame its cause passed through, and their tracebacks hold that frame as
    the same frame object, so the shared part is found by identity, not by
    comparing names or lines
- the handler that caught each cause is its own line, `caught by` and the
    clause, since that is where one story became the other
- a `^` that converted an error is marked where it stands
- where an error was created and where it was raised are shown apart when they
    differ, as in `err = E()` followed later by `raise err`
- a run of frames from the standard library or a dependency is folded to one
    line, `… 6 frames in asyncio`
- an `ExceptionGroup` prints each of its errors as a branch in the same form

the tree for a chain of raises needs no trace at all, since frame identity is in
the tracebacks themselves. the trace adds the frames above each catch point and
gives errors made as values a location

### module names, not paths

a frame is named by its module, its line, and the function it was in:

```text
demo:2 in f
asyncio.tasks:314 in Task.__step
```

a module name reads the same on every machine and is how the program refers to
its own code, where a path into `site-packages` is long and different on each.
it also hides whether the frame ran in the `.by` source or its lowered python:
the line is mapped back to the `.by` source through its
[sourcemap](sourcemaps.md). the function is written `in f` rather than `demo.f`,
which would read as a submodule

inside a branch a frame in the same function shows only its line (`:6`), and a
frame in another function names it in full (`demo:11 in h`), so a deeper call
and a later line of the same call look different at a glance

code with no module, from `exec`, the repl or a notebook cell, is named as python
names its file, `<repl>:3`

a terminal that supports hyperlinks gets each module name linked to its file. a
plain log loses that, so `--error-paths` prints file paths instead of module
names

### where the tree is printed

the tree is printed by basedpython's own tools: an error that ends `by run`, a
failing test under the [pytest integration](../frameworks/pytest.md), and the
[debugger](../features/debugger.md). `traceback.print_exception`, `logging`,
pytest's own report and error trackers keep printing python's format, since they
walk `__cause__` and `__traceback__`, which keep meaning what they mean in
python

the `stitch` option below lets those tools see the frames above the catch point
too, by adding them to `__traceback__`

## overhead

recording costs time, and stitching costs memory, so each is a separate option
and each can be turned off. `--error-traces` on `by run`, `by build` and
`by transpile` takes a spec in the same form as `--soundness`:

```sh
by run main                                  # the default set
by run main --error-traces none              # record nothing
by run main --error-traces all               # everything, incl. stitch and foreign
by run main --error-traces values,propagation
```

| option        | what it records                                             | cost                                                             | default |
| ------------- | ----------------------------------------------------------- | ---------------------------------------------------------------- | ------- |
| `values`      | the origin of an error made as a value                      | a stack walk per error made, once per error                      | on      |
| `catches`     | the frames above the catch point of an error caught by name | a stack walk per caught-and-named error                          | on      |
| `propagation` | each `^` an error passes through                            | one append per `^` that propagates an error                      | on      |
| `stitch`      | the recorded frames, added to `__traceback__`               | keeps the frames above each catch point, and their locals, alive | off     |
| `foreign`     | `catches`, for python code as well                          | a callback on every handled exception, python 3.12 or later      | off     |

`default` is `values`, `catches` and `propagation`; `all` adds `stitch` and
`foreign`; `none` records nothing. an unknown name is an error, so a typo never
silently turns off a record you expected

with `none`, nothing in the emitted python records anything and no runtime
helper is called for it. the tree is still printed, from what python itself
keeps: the chain of raises, with its shared frames found by identity

conversion by `^` and the `implicit-exception-context` rule are language
semantics, not records, so they cost nothing at runtime and are not part of the
spec. the rule's severity is set like any other's, under `rules`

### opting out one error class

an error used for control flow in a hot loop pays for a stack walk it will never
print. a class turns its own recording off:

```by
class Miss(Exception):
    __traced__ = False
```

an instance of `Miss`, or of any subclass that does not set it back, records no
origin, whatever `--error-traces` says. `^` still converts it and still links
causes. it is a class attribute rather than syntax, so a class written in python
can set it too

## lowering

what the recording looks like in the emitted python, with the default spec

an error caught by name records the frames above its catch point as the handler's
first statement:

```by
try:
    h()
except Exception as e:
    handle(e)
```

```python
try:
    h()
except Exception as e:
    _by_caught(e)
    handle(e)
```

`^` on a result records its site when it propagates, and converts first when the
enclosing result names another error type:

```by
def load(path: str) -> Config ? ConfigError:
    text = read(path)^
```

```python
def load(path):
    __by_prop_0__ = read(path)
    if isinstance(__by_prop_0__, BaseException): return _by_propagate(ConfigError.__from__(__by_prop_0__), __by_prop_0__)
    text = __by_prop_0__
```

an error returned as a value is passed through `_by_originate` where it is
returned. each helper does nothing when its option is off, and is not emitted at
all when the spec turns it off at transpile time

## open questions

- **`!` on an error.** force-unwrap raises `RuntimeError` with the error as its
    `__cause__`, which is one more link to print. it could raise the original
    error with its trace instead, which reads better, but then
    `except ValueError` catches what was meant to be an unwrap failure
- **async.** inside a coroutine the stack ends at the task, not at whoever awaited
    it. python 3.14's `asyncio.capture_call_graph` gives the await chain, and
    could extend `values` and `catches` there, gated on the python version
- **order.** the tree reads from the root down, like python's frames, with the
    headline first. whether the causes in the headline read newest first, as
    proposed, or oldest first, as python's segments do, is open
