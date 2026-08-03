# type reification

standard python erases inferred specializations: `A(1)` constructs an `A`
with no record that it was an `A[int]`. the transpiler makes the inferred
specialization of a user-defined generic constructor explicit in the generated
python:

```by
class A[T]:
    def __init__(self, t: T):
        self.t = t

a1 = A(1)
a2 = A("x")
```

→

```python
a1 = A[int](1)
a2 = A[str]("x")
```

`A[int](…)` routes through `types.GenericAlias.__call__`, which stamps
`__orig_class__` on the constructed instance — the specialization becomes an
observable runtime value:

```python
a = A(1)          # in basedpython source
print(a.__orig_class__)   # A[int] at runtime
```

the stamp is what makes a runtime specialization visible — see
[parametric type tests](parametric-type-tests.md) for `x is A[int]`, which
reads it back.

## collection literals stay bare

a display is left exactly as written, even where its element type is known:

```by
xs = [1, 2]
d = {"k": 1}
s = {3}
t = 1, "x"
```

the builtin collections silently reject the `__orig_class__` stamp, so
`list[int]([1, 2])` constructs a value indistinguishable from `[1, 2]` — the
wrap would carry no runtime information and cost a constructor call

that is why a parametric test cannot read a specialization back off a builtin:
`x is list[int]` resolves statically or through a [reified](reified-generics.md)
type parameter instead

## where the types come from

the injected spelling is read from the specialization ty already inferred for
the expression — the same solution the checker reports — promoting each
argument as it is spelled (`Literal[1]` → `int`, since only a class object can
be written at runtime; a covariant parameter keeps its literal in the checker's
own view, see [fluid specializations](fluid-specializations.md)). there is no
separate solver, so the injected arguments never disagree with the checker, and
usage-based widening of an inferred specialization flows straight into the
injection

an explicit specialization is always kept as written: `A[int](1)` transpiles
unchanged, and is never wrapped twice

## best-effort, never an error

unlike [reified type parameters](reified-generics.md) — where the runtime
*needs* the type argument and an uninjectable bare call is a checker error —
constructor reification changes nothing the body can observe, so it simply
doesn't fire when no runtime spelling exists:

- an unsolved or dynamic argument — `A()` inferred as `A[Unknown]`, or `A(x)`
    for an unannotated `x`
- a type argument with no spelling at the call site, such as a class defined
    inside a function: `A(Local())` stays bare
- a non-generic class, which has no specialization to make explicit

## what never reifies

- type expressions: annotations, type-parameter lists, `type X = …` values,
    and type-context subscript slices (the `[int]` list of a legacy
    `Callable[[int], str]` is type syntax, not a value)
- the values of dunders that static readers consume structurally: `__all__`,
    `__slots__`, `__match_args__`
- `sys.version_info` comparisons — every static reader (including ty on the
    generated python) must see the literal tuple gate
- function parameter defaults — a non-scalar default is consumed whole by
    the [mutable defaults](mutable-defaults.md) lowering and re-evaluated in a
    body guard. lambda defaults are not sentinel-lowered, so they do reify
- stubs (no runtime to observe) and targets below python 3.9
