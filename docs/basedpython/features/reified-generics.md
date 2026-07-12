# reified type parameters

standard python erases type parameters: a `def f[T]` has no way to recover
`T` at runtime, so `isinstance(x, T)`, `T()`, or `print(T)` inside the body
are impossible. basedpython makes a type parameter available as a real
runtime value when the body actually uses it as one:

```by
def f[T]():
    print(T)   # prints int

f[int]()
```

the type parameter behaves like an ordinary positional parameter that happens
to be filled by the `[...]` specialization step rather than the `(...)` call
step

## why this is safe to do

PEP 695 already compiles the type-parameter list as an implicit enclosing
scope, so inside `f` the name `T` is a *free variable* of the function's code
object — `f.__code__.co_freevars == ("T",)` — bound to a cell holding the
`TypeVar`. reification just swaps that cell's contents from the `TypeVar` to
the concrete type argument before the body runs. no bytecode is rewritten and
no name resolution is special-cased; we reuse the closure machinery cpython
already builds

## desugaring

a function is reified only when one of its type parameters is referenced in a
*value* position (anywhere other than a type annotation). that function is
wrapped in the `generic` [polyfill](polyfills.md) and its call sites route
through specialization instead of being stripped:

```by
def f[T](t: object):
    return isinstance(t, T)

f[int](1)
```

→

```python
@generic
def f[T](t: object):
    return isinstance(t, T)

f[int](1)   # not stripped — routes through generic.__getitem__
```

the `generic` wrapper, emitted into the preamble:

```python
@dataclass
class generic:
    fn: object
    args: object = None
    instance: object = None

    def __get__(self, obj, objtype=None):
        if obj is None:
            return self
        return generic(self.fn, self.args, obj)

    def __getitem__(self, item):
        if self.args is not None:
            raise TypeError("type arguments already specified")
        if not isinstance(item, tuple):
            item = (item,)
        return generic(self.fn, item, self.instance)

    def __call__(self, *args, **kwargs):
        freevars = self.fn.__code__.co_freevars
        values = list(self.args) if self.args is not None else []
        params = self.fn.__type_params__
        while len(values) < len(freevars):
            values.append(params[len(values)].__default__)
        temp_fn = FunctionType(
            self.fn.__code__,
            self.fn.__globals__,
            self.fn.__name__,
            None,
            tuple(CellType(value) for value in values),
        )
        if self.instance is not None:
            return temp_fn(self.instance, *args, **kwargs)
        return temp_fn(*args, **kwargs)
```

a reified method binds its receiver through the descriptor `__get__`, so
`obj.m[int]()` passes `self` exactly like an ordinary method call

`f[int]` produces a specialized `generic` carrying `args=(int,)`; calling it
rebuilds the function with a closure whose cells hold the type arguments, so
the body sees `T is int`. an omitted slot is filled from its [PEP 696] default,
read off the function's `__type_params__`. a type parameter used only in
annotations is left erased exactly as in
[explicit generic call sites](generic-calls.md) — the wrapper is the cost of
reification and we only pay it when the body needs it

the lowered `def` keeps its native `[T]` syntax because reification reuses the
pep 695 closure cells cpython already builds. that syntax is only available on
python 3.12+, so reification requires `min_version >= 3.12`; a reified function
on an older target is a transpile error rather than code that cannot run. a
defaulted parameter ([PEP 696]) raises the bar to `min_version >= 3.13` — the
default syntax is not valid on 3.12, and the erased polyfill can't stand in
because it discards the native parameter list reification depends on

## specialization is mandatory

because python carries no runtime type information, a reified type parameter
**cannot** be inferred from the arguments — it must be supplied explicitly:

```by
f[int](1)   # ok
f(1)        # error — T is reified and has no value
```

ty reports the bare call as an error: a reified generic is structurally a
two-step callable (`f[...]` then `(...)`), and the first step is not optional.
the one exception is a type parameter with a default ([PEP 696]), which fills
the reified slot when omitted:

```by
def f[T = int]():
    print(T)

f()         # prints int — default supplies the reified value
f[str]()    # prints str
```

## subtyping

reification changes what counts as a valid subtype. once `[...]` is a
runtime-significant step, the type-parameter list becomes part of the
callable's interface — it is no longer phantom. assignability between two
reified generics requires the type-parameter lists to be compatible, on top of
the usual value-parameter and return-type rules

> the [reified-and-erased distinctness](#reified-and-erased-are-distinct) rule
> below is implemented: a reified generic is not assignable to a plain
> callable. the writable `generic[[T], () -> bool]` type-form — and with it the
> full arity / name / bound-contravariance rules between two reified generics —
> is not yet available as an annotation; those examples describe the intended
> behaviour

### arity must match

a caller specializes through the *target* type, so the source must accept
every specialization the target permits. the type-parameter lists must agree
on arity, counted with the same rules value parameters use — defaults, `*Ts`,
and `**P` all participate:

```by
type F = generic[[T], () -> bool]
type G = generic[[T, U], () -> bool]

def one[T]() -> bool: ...
def two[T, U]() -> bool: ...

f: F = one   # ok
g: F = two   # error — F supplies one type argument, two needs two
```

a default on the target makes the corresponding source slot optional in the
same way a defaulted value parameter does

### names do not matter, positions do

specialization is positional (`f[int]`), so type-parameter *names* are
irrelevant to assignability — the same way value parameters match by position,
not by name:

```by
def a[T](x: T) -> T: ...
def b[U](x: U) -> U: ...

a_ref: generic[[T], (T) -> T] = b   # ok — T and U are positional
```

the exception is keyword-specializable type parameters (see
[generics](generics.md)), which are matched by name just as keyword value
parameters are

### bounds are contravariant

a reified type parameter constrains what concrete types a caller may supply,
so its bound behaves like an input — assignability is contravariant in the
bound, mirroring value-parameter types. the target's bound must be assignable
to the source's bound:

```by
def narrow[T: int](): ...
def wide[T: object](): ...

n: generic[[T: int], () -> None] = wide   # ok — wide accepts everything narrow does
w: generic[[T: object], () -> None] = narrow   # error — narrow rejects non-int args
```

### reified and erased are distinct

a reified generic is **not** assignable to a plain callable, and vice versa.
the plain callable has no slot for the specialization step, so substituting one
for the other would skip a structurally required call:

```by
def f[T](): print(T)        # reified

c: () -> None = f           # error — f requires f[...] before ()
```

an erased generic (type parameter used only in annotations) keeps its existing
PEP 695 assignability and remains usable wherever a plain callable is expected

## variadics and paramspecs

> not yet implemented — only plain type parameters reify today; a `*Ts` or
> `**P` is never reified and stays erased

`*Ts` reifies to a tuple of the supplied type arguments and `**P` to the
typed-dictionary view described in [generics](generics.md). both follow the
arity rules above — a trailing `*Ts` absorbs zero or more positional type
arguments, and the `generic` wrapper packs them into a single cell so the body
observes one tuple value

## round-tripping

the reverse transpiler recognizes the `@generic` wrapper by its
`ReifiedGeneric` provenance in the `LoweringMap` and re-sugars it back to a
bare `def f[T]` with reified body usage, restoring the unstripped `f[int](...)`
call sites. a hand-written `@generic` decoration without that provenance is
left untouched

[pep 696]: https://peps.python.org/pep-0696/
