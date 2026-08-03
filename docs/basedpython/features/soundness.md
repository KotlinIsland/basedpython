# runtime type-soundness checks

a type checker is only sound up to the assumptions it makes. some of those
assumptions are pure annotation-level claims with no runtime backing — a
`def t[T]() -> T` promises to return a `T`, but nothing at runtime enforces
it. basedpython inserts a runtime `isinstance` guard at each point where a
value crosses from that unverified world into a typed slot:

```by
def t[T]() -> T: ...

def f():
    a: str = t()
```

transpiles to (targeting python 3.12):

```python
def _soundness_check(_v, _t):
    if not isinstance(_v, _t):
        raise TypeError(
            f"type soundness violation: expected {getattr(_t, '__name__', _t)}, "
            f"got {type(_v).__name__}"
        )
    return _v

def t[T]() -> T: ...

def f():
    a: str = _soundness_check(t(), str)
```

if `t()` returns something that isn't a `str`, the program raises a
`TypeError` at the point the lie enters, instead of failing later somewhere
unrelated (or not at all)

## positions

a check is inserted at these syntactic positions, each independently
toggleable:

| position        | validates                                        | example                            |
| --------------- | ------------------------------------------------ | ---------------------------------- |
| `generic-calls` | results of calls whose type is typevar-derived   | `d.get(k)` on a `dict[str, int]`   |
| `projections`   | element reads out of a specialized container     | `a[0]` on an `a: list[str]`        |
| `iterations`    | loop / comprehension elements                    | `for x in a` on `a: list[str]`     |
| `assignments`   | `dynamic` flowing into an annotated binding      | `a: str = dyn_val`                 |
| `returns`       | a returned value against the declared return     | `def g() -> str: return dyn_val`   |
| `arguments`     | a call argument against its parameter annotation | `takes(dyn_val)` — `takes(s: str)` |
| `parameters`    | a function's own parameters, at entry            | `def f(a: A[int]): ...`            |

the first six defend against gaps in ty's own inference and are on by default.
`parameters` is different — it validates a function's parameters *inside the
function body*, defending its contract against callers the checker never saw
(untyped or third-party code that imports and calls it):

```by
class A[T]:
    t: T

def f(a: A[int]): ...
```

with `parameters` enabled, `f` transpiles to:

```python
def f(a: A[int]):
    _soundness_parametric(a, A[int], (0,))
    ...
```

now `f(x)` raises if `x` isn't really an `A[int]`, no matter who calls it or
whether they suppressed the error with `# type: ignore`. because it runs on
every call, `parameters` is **off in the default set** — opt in with
`--soundness all` or `--soundness parameters` (see [configuration](#configuration)).
guards are placed after any docstring; `self`, unannotated, `*args`/`**kwargs`,
and untestable-typed parameters are skipped.

iteration wraps the iterable in a generator (`_soundness_iter`, or
`_soundness_aiter` for `async for`) that validates each element as it is
produced:

```by
def f(a: list[str]):
    for x in a:
        print(x)
```

→ `for x in _soundness_iter(a, str):`

## what gets a check

the second argument is a shallow `isinstance` target derived from the
inferred type. the check is deliberately shallow — `list[str]` validates as
`list`; the element claim is validated at its own projection sites, not by
walking the whole container:

| inferred type | check target        |
| ------------- | ------------------- |
| `str`         | `str`               |
| `int \| None` | `(int, type(None))` |
| `list[str]`   | `list`              |
| a user class  | the class name      |

no check is emitted when there is no faithful shallow runtime test —
protocols, callables, unsolved type parameters, and `object` (which every
value satisfies) are all skipped, as is a type whose name doesn't resolve at
module scope (a class shadowed by a local binding, or one not in scope where
the check would run). a pure-`None` result is skipped too: validating a value
that carries no data guards nothing

## deep checks for user generics

for a target that is a *user-defined* generic specialization (`A[int]`), the
check validates the type arguments too, not just the base class. python stamps
`A[int]()` instances with `__orig_class__`, so the specialization survives to
runtime and can be verified:

```by
class A[T]:
    t: T

def f(a: A[int]): ...

def g(x: dynamic):   # x is a real A[…], but ty can't see which
    f(x)
```

→ `f(_soundness_parametric(x, A[int], (0,)))`

if `x` is an `A[bool]` (say it came from unchecked third-party code and was
handed to you as `dynamic`), the check raises rather than letting the wrong
specialization through. the match respects the target's declared variance — an
`out T` (covariant) parameter accepts a subtype argument.

the arguments are only checked when the value actually carries them: a value
with no `__orig_class__` (a bare `A()`, or an instance from a non-generic
code path) passes the argument check, leaving the base `isinstance` as the
guarantee — validate what's available, never invent a failure.

builtin collections (`list[int]`, `dict[str, int]`) erase their type arguments
at runtime, so they keep the shallow base check (`list`, `dict`); there is
nothing to probe.

## configuration

the inference-gap checks are on by default. control which positions are active
with `--soundness` on `by run`, `by build`, and `by transpile`:

```sh
by run main                                   # the default set (no `parameters`)
by run main --soundness all                   # everything, incl. `parameters`
by run main --soundness none                  # disable entirely
by run main --soundness parameters            # only the entry checks
by run main --soundness returns,arguments     # only these two
by transpile main.by --soundness projections  # only element reads
```

the spec is `default`, `all`, `none`, or a comma-separated subset of the
position names in the table above. `default` is the six inference-gap checks;
`all` adds the opt-in `parameters` entry checks. an unknown name is a hard
error, so a typo never silently disables a check you expected

checks are never emitted for stub files (`.byi`), which don't execute

## composition

the guards compose with the other basedpython lowerings inside the value they
wrap — a `??` coalesce, a `?.` chain, a `!` force-unwrap, or a nested
projection all lower normally inside the check:

```by
def f(a: list[list[str]]):
    b = a[0][1]
```

→ `b = _soundness_check(_soundness_check(a[0], list)[1], str)`

a value that is already checkable on its own is validated once, at the
innermost position that knows a concrete target — the `returns`,
`assignments`, and `arguments` gates defer to `generic-calls` / `projections`
rather than wrapping a second time

## limits

the checks are conservative — a missed check is a silent no-op, a wrong one
would change behaviour, so anything ambiguous is skipped. there is no check on
`await` results, unpacking targets, `*args` / `**kwargs` spreads, or arguments
to class constructors and overloaded functions. a check argument that names a
class defined later in the module can raise `NameError` if the guarded line
runs at import time before the class body
