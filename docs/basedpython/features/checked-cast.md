# checked cast

the plain [`cast`](cast.md) only widens. for a downcast — taking an `object` as
an `int` — basedpython has two operators that check the value at runtime and
differ in what they do when it does not match. `cast!` is **checked**: it
raises. `cast?` is **safe**: it yields `None`.

## `cast!` — checked (raises)

`<value> cast! <type>` narrows the value to the target type and verifies it at
runtime:

```by
def f(a: object):
    b = a cast! int
    print(b)

f(1)    # prints 1
f("x")  # raises TypeError: cast to int failed: value is str
```

transpiles to:

```python
def _checked_cast(_v, _t):
    if not isinstance(_v, _t):
        raise TypeError(
            f"cast to {getattr(_t, '__name__', _t)} failed: value is {type(_v).__name__}"
        )
    return _v

def f(a: object):
    b = _checked_cast(a, int)
    print(b)
```

its type is the target type (`b` is `int`).

## `cast?` — safe (returns `None`)

`<value> cast? <type>` yields the value when it matches and `None` otherwise,
so its type is `<type> | None`:

```by
def f(a: object):
    b = a cast? int
    reveal_type(b)  # revealed: int | None

f(1)    # b is 1
f("x")  # b is None
```

transpiles to `b = _try_cast(a, int)`, with the helper:

```python
def _try_cast(_v, _t):
    return _v if isinstance(_v, _t) else None
```

## shared rules

both forms:

- check as deeply as the target allows at runtime:
    - a **user generic** is checked *in full*. its instances carry
        `__orig_class__` (stamped by `A[int](…)`), so `x cast! A[int]` rejects an
        `A[str]`, respecting each type parameter's variance. a value carrying no
        reification passes the argument check — there is nothing to compare —
        leaving the base class as the guarantee
    - **anything else** collapses to its runtime origin: `a cast! list[int]`
        checks `isinstance(a, list)`, because a builtin erases its type
        arguments and `isinstance(a, list[int])` is itself a `TypeError`. the
        dropped `[int]` claim is reported by the `erased-cast-argument` warning.
        a union checks each arm's origin (`a cast? list[int] | None` →
        `isinstance(a, (list, type(None)))`)
- skip the check entirely when it is provably redundant. if the value is already
    the target, the probe would always pass, so the cast degrades to the same
    plain `typing.cast` the unsuffixed `cast` emits
- evaluate the value exactly once, even when it has side effects
    (`g() cast! int` calls `g()` once)
- are basedpython-only: a `.py` file using either produces a parse error. they
    never collide with a plain `cast` identifier — the suffix follows the keyword
    directly, and no expression can begin with `!` or `?`
