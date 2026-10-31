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

## a builtin cannot record a specialization

the stamp needs somewhere to live, and `__orig_class__` is an ordinary
attribute: `types.GenericAlias.__call__` sets it on the value it just built,
and moves on silently when the value won't take it. a c type has no instance
dictionary, so it never takes it — every builtin generic is in the same
position, whatever it looks like at the type level:

```python
list[int]([1, 2]).__orig_class__        # None — silently dropped
enumerate[str](["a"]).__orig_class__    # None — the same
zip[tuple[int, bool]]([1], [True])      # TypeError: not subscriptable
```

the last line is the same fact one step earlier. subscripting a class needs a
`__class_getitem__`, which a python-level class inherits from `Generic` but a c
type only has if cpython wrote one by hand — `list` got one in 3.9,
`array.array` in 3.12, `memoryview` in 3.14, and `zip`, `map`, `filter`,
`reversed` and `itertools.count` never did. so a builtin either accepts the
subscript and drops the stamp, or refuses the subscript outright

so a builtin never reports a specialization back, which is one reason a call of a class of the
standard library is left as it is written, whatever the class would do with a subscript. the rest
follows from that

a **display** is left exactly as written, even where its element type is known.
wrapping it would turn syntax into a constructor call and record nothing:

```by
xs = [1, 2]
d = {"k": 1}
s = {3}
t = 1, "x"
```

a **call** of a class of the standard library reads as it is written, so a module with no generic
code of its own transpiles to itself. a call of any other generic class is rewritten, if the
subscript is known to evaluate, whether its definition or a stub of it is in view:

```by
pairs = zip([1, 2], [True, False])   # stays as it is written
counts = enumerate(["a"])            # stays as it is written
letters = list("ab")                 # stays as it is written
box = Box(1)                         # → Box[int](1)
```

a subscript the source writes itself is another matter: `map[int]` is valid
basedpython, whether or not `map` would allow it, so it evaluates to what it
would have been, a `types.GenericAlias` with the same `__origin__` and
`__args__`. an annotation is quoted instead wherever python would evaluate it,
so it stays the type expression its readers expect:

```by
a = map[int]                  # a = _by_subscript(map, int)
def f(x: map[int]): ...       # def f(x: "map[int]"): ...
```

this is also why a parametric test cannot read a specialization back off a
builtin: `x is list[int]` resolves statically or through a
[reified](reified-generics.md) type parameter instead

## where the types come from

the injected spelling is read from the specialization ty already inferred for
the expression — the same solution the checker reports. that includes whether a
literal widened: `A(1)` is an `A[int]` when `T` is invariant, and a covariant
`B(1)` is a `B[1]` that also passes for a `B[int]`, see
[fluid specializations](fluid-specializations.md). there is no separate solver,
so the injected arguments never disagree with the checker, and usage-based
widening of an inferred specialization flows straight into the injection

an explicit specialization is always kept as written: `A[int](1)` transpiles
unchanged, and is never wrapped twice

## best-effort, never an error

unlike [reified type parameters](reified-generics.md) — where the runtime
*needs* the type argument and an uninjectable bare call is a checker error —
constructor reification changes nothing the body can observe, so it simply
doesn't fire when no runtime spelling exists:

- an unsolved or dynamic argument — `A()` inferred as `A[Unknown]`, or `A(x)`
    for an unannotated `x`

- a type parameter of a classmethod, or of the enclosing class, which has no
    runtime value here. a type parameter of a plain function does have one, see
    [a type parameter the body forwards](reified-generics.md#a-type-parameter-the-body-forwards)

- a type argument with no spelling at the call site, such as a class defined
    inside a function: `A(Local())` stays bare

- a non-generic class, which has no specialization to make explicit

- a class of the standard library — see [above](#a-builtin-cannot-record-a-specialization)

- a class the target runtime is not known to subscript

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
