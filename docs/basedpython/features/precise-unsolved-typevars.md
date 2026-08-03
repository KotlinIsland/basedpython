# precise unsolved type variables

a call can leave a type variable entirely unsolved, because no argument constrains it

```python
def f[T]() -> T: ...

a = f()   # Never
```

`Never` is the precise answer. no value ever reaches that position, so nothing the call returns can
be observed at type `T`. python's [gradual guarantee] asks for `Unknown` instead, which quietly
accepts every later use of `a` — including the ones that are mistakes

this is on by default, and applies to `.py` files as well as `.by`. to turn it off

```toml
[analysis]
precise-unsolved-typevars = false
```

a pep 696 default takes priority over `Never`. `ParamSpec`, `TypeVarTuple` and keyword-variadic
packs are unaffected: their specializations are callable-, tuple- and mapping-shaped, and `Never` is
not a valid value for one

for a [reified](reified-generics.md) type parameter an unsolved type variable is always an error
(`unspecialized-reified-generic`), whatever this option says — the specialization is a runtime step,
so there is no type that could stand in for it

## only where the type variable is an output

`Never` describes a value nobody can observe. that is what a type variable in a return position
means, and nothing else

where the type variable is also *written through* or *passed back in*, the same substitution says
something quite different: that nothing can ever be put there. so the answer follows the type
variable's variance, and an invariant or contravariant occurrence keeps the gradual `Unknown`

```python
def build[T](key: T | None) -> dict[T, int]: ...

reveal_type(build(None))   # dict[Unknown, int]  — a `dict[Never, int]` could never be written to
```

```python
def sink(x) -> None: ...
def pipe[A, B](f: Callable[[A], B]) -> Callable[[A], B]: ...

reveal_type(pipe(sink))    # (Unknown, /) -> None  — a `(Never, /)` could never be called
```

without this rule, a type variable the checker simply failed to infer would produce an uninhabited
container or an uncallable callable, and the error would land at every later *use* of the result
rather than at the call that could not infer it

variance is read positionally for a type variable bound to a function: python only gives a declared
variance meaning for a generic class, and a legacy `TypeVar("T")` is invariant under its own rules,
so `def f[T]() -> T` and its legacy spelling say the same thing here

## the call still returns

a return type of `Never` normally says the callee does not return, and a statement-level call to
such a callee ends the flow. an unsolved type variable says nothing about control flow, so the code
after the call stays reachable

```python
def f[T]() -> T: ...

def g() -> None:
    x = 1
    f()
    reveal_type(x)   # Literal[1] — still reachable
```

a callee that genuinely does not return is unaffected, including through a generic call that solves
its type variable from a `Never` argument (`identity(exit())`)

## per-module configuration

the option is resolved per module. the rule is that **the module declaring a function governs how
its calls are solved**, and callers see the result whatever their own setting is

```toml
[[overrides]]
include = ["vendor/**"]

[overrides.analysis]
precise-unsolved-typevars = false
```

a synthesized signature that no module declares follows the default

## related

- [sound types](sound-types.md) — the same trade for a missing annotation, opt-in
- [fluid specializations](fluid-specializations.md) — how an inferred specialization widens on use

[gradual guarantee]: https://typing.python.org/en/latest/spec/concepts.html#the-gradual-guarantee
