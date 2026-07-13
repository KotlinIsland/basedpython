# type reification

standard python erases inferred specializations: `A(1)` constructs an `A`
with no record that it was an `A[int]`, and `[1, 2]` is just a `list`. the
transpiler makes every inferred specialization explicit in the generated
python, for generic constructor calls and for collection literals:

```by
class A[T]:
    def __init__(self, t: T):
        self.t = t

a = A(1)
xs = [1, 2]
d = {"k": 1}
t = 1, "x"
s = {3}
```

→

```python
a = A[int](1)
xs = list[int]([1, 2])
d = dict[str, int]({"k": 1})
t = tuple[int, str]((1, "x"))
s = set[int]({3})
```

`A[int](…)` routes through `types.GenericAlias.__call__`, which stamps
`__orig_class__` on the constructed instance — the specialization becomes an
observable runtime value:

```python
a = A(1)          # in basedpython source
print(a.__orig_class__)   # A[int] at runtime
```

the builtin collections silently reject the `__orig_class__` stamp, so for
literals the reification lives in the generated source (and costs one extra
constructor call); the constructed value is identical

## where the types come from

the injected spelling is read from the specialization ty already inferred for
the expression — the same solution the checker reports — after literal
promotion (`Literal[1]` → `int`). there is no separate solver, so the
injected arguments never disagree with the checker, and usage-based widening
of an inferred specialization flows straight into the injection

an explicit specialization is always kept as written: `A[int](1)` and
`list[int]([1, 2])` transpile unchanged

## best-effort, never an error

unlike [reified type parameters](reified-generics.md) — where the runtime
*needs* the type argument and an uninjectable bare call is a checker error —
constructor and literal reification changes nothing the body can observe, so
it simply doesn't fire when no runtime spelling exists:

- an unsolved or dynamic argument (`A()` inferred as `A[Unknown]`, `[]`)
- a scope-local class (its bare name doesn't resolve at module scope)
- a shadowed builtin (`list` rebound at module level)
- a variable-length tuple (`(1, *rest)`)

## what never reifies

- type expressions: annotations, type-parameter lists, `type X = …` values,
    and type-context subscript slices (the `[int]` list of a legacy
    `Callable[[int], str]` is type syntax, not a value)
- dunders that static readers consume structurally: `__all__`, `__slots__`,
    `__match_args__`
- `sys.version_info` comparisons — every static reader (including ty on the
    generated python) must see the literal tuple gate
- value-position subscript keys (`d[(1, 2)]`) — a key is a structural index
    read verbatim by tuple-key and kw-subscript handling, not a constructed
    value (displays nested inside a key still reify)
- function parameter defaults — a non-scalar default is consumed whole by
    the [mutable defaults](mutable-defaults.md) lowering and re-evaluated in a
    body guard. lambda defaults are not sentinel-lowered, so they reify
- stubs (no runtime to observe) and targets below python 3.9 (pep 585 is
    what makes the builtins subscriptable at runtime)
