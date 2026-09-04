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

## declaring it

reification is inferred from the body, which means it can also be *declared*.
`reified` ahead of a type parameter reifies it whether or not the body ever
reads it as a value:

```by
def f[reified T](t: T):
    print(T)
```

writing the keyword is the difference between reification being a consequence
of how the body happens to be written and it being part of the declaration —
the specialization step is required either way, so a signature that promises a
runtime `T` keeps promising it when the body stops printing it. it stacks ahead
of the [variance](variance.md) keywords (`reified out T`) and applies to a
variadic and a keyword pack as well (`reified *Ts`, `reified **Kwargs`)

`reified` is a soft keyword: a type parameter *named* `reified` is still just
that, since nothing that can open a parameter follows it

a class reifies too, though it reads the argument from the instance rather than
from a closure — see [reified class type parameters](reified-class-generics.md).
nothing else has a step to hang a runtime value off, so `reified` on a type
alias or a `type def` is an error (`invalid-reified-type-param`), as it is on a
class's keyword pack, which a subscript has no way to supply

the editor hints the modifier wherever the body reifies a type parameter
without declaring it, so what is inferred and what is written read the same

## why this is safe to do

PEP 695 already compiles the type-parameter list as an implicit enclosing
scope, so inside `f` the name `T` is a *free variable* of the function's code
object — `f.__code__.co_freevars == ("T",)` — bound to a cell holding the
`TypeVar`. reification just swaps that cell's contents from the `TypeVar` to
the concrete type argument before the body runs. the swap is keyed by
free-variable *name*, so every other cell — a captured outer local, the
implicit `__class__` behind zero-arg `super()` — carries over untouched, and
the rebuilt function keeps its parameter defaults, keyword-only defaults and
qualname. no bytecode is rewritten and no name resolution is special-cased; we
reuse the closure machinery cpython already builds

## desugaring

a function is reified when one of its type parameters is declared `reified` or
is referenced in a *value* position (anywhere other than a type annotation).
that function is wrapped in the `generic` [polyfill](polyfills.md) and its call
sites route through specialization instead of being stripped:

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
class generic:
    def __init__(self, fn, args=None, instance=None):
        self.fn = fn
        self.args = args
        self.instance = instance

    def __repr__(self):
        return f"<generic {self.fn!r}>"

    def __getattr__(self, name):
        if name == "fn":
            raise AttributeError(name)
        return getattr(self.fn, name)

    def __get__(self, obj, objtype=None):
        if obj is None:
            return self
        return generic(self.fn, self.args, obj)

    def __getitem__(self, item):
        if self.args is not None:
            raise TypeError("type arguments already specified")
        if not isinstance(item, tuple):
            item = (item,)
        params = self.fn.__type_params__
        if len(item) > len(params):
            raise TypeError(
                f"too many type arguments for {self.fn.__name__}: "
                f"expected {len(params)}, got {len(item)}"
            )
        return generic(self.fn, item, self.instance)

    def __call__(self, *args, **kwargs):
        fn = self.fn
        code = fn.__code__
        supplied = self.args if self.args is not None else ()
        values = {}
        for index, param in enumerate(fn.__type_params__):
            name = param.__name__
            if index < len(supplied):
                values[name] = supplied[index]
                continue
            has_default = getattr(param, "has_default", None)
            if has_default is not None and has_default():
                values[name] = param.__default__
            elif name in code.co_freevars:
                raise TypeError(f"{fn.__name__}() missing a type argument for {name!r}")
        closure = tuple(
            CellType(values[name]) if name in values else cell
            for name, cell in zip(code.co_freevars, fn.__closure__ or ())
        )
        temp_fn = FunctionType(code, fn.__globals__, fn.__name__, fn.__defaults__, closure)
        temp_fn.__kwdefaults__ = fn.__kwdefaults__
        temp_fn.__qualname__ = fn.__qualname__
        if self.instance is not None:
            return temp_fn(self.instance, *args, **kwargs)
        return temp_fn(*args, **kwargs)
```

a reified method binds its receiver through the descriptor `__get__`, so
`obj.m[int]()` passes `self` exactly like an ordinary method call. attribute
access falls through to the wrapped function, so introspection (`f.__name__`,
`f.__doc__`, `f.__type_params__`) keeps working, and the wrapper hashes by
identity like the function it replaces

`f[int]` produces a specialized `generic` carrying `args=(int,)`; calling it
rebuilds the function with a closure whose type-parameter cells hold the type
arguments, so the body sees `T is int`. an omitted slot is filled from its
[PEP 696] default, read off the function's `__type_params__`. a type parameter
used only in annotations is left erased exactly as in
[explicit generic call sites](generic-calls.md) — the wrapper is the cost of
reification and we only pay it when the body needs it

over- and under-specialization that get past the type checker still fail
cleanly at runtime: too many type arguments raise `TypeError` at the
subscription, and a reified slot with neither a value nor a default raises
`TypeError` at the call

## decorators

the wrapper is inserted *innermost* — directly above the `def`, below any
user decorators — so it always receives the raw function object whose closure
it rebuilds. outer decorators then compose with the wrapper exactly as the
type checker models them:

```by
class C:
    @staticmethod
    def f[T]() -> object:
        return T

C.f[int]()   # staticmethod's descriptor passes the wrapper through
```

a decorator that returns a *different* callable erases the specialization
step — `f[int]` on its result is an ordinary subscript on whatever the
decorator returned, and ty flags it at the use site if that type has no
`__getitem__`

the one binding that cannot compose is `classmethod`: it hides the function
behind an opaque bound method with no `__getitem__`, so a reified classmethod
could be neither specialized nor called. ty reports `reified-classmethod` at
the definition and the transpiler refuses to emit it. `__init_subclass__` and
`__class_getitem__` are implicitly classmethods and get the same treatment

the lowered `def` keeps its native `[T]` syntax because reification reuses the
pep 695 closure cells cpython already builds. that syntax is only available on
python 3.12+, so reification requires `min_version >= 3.12`; a reified function
on an older target is a transpile error rather than code that cannot run. a
defaulted parameter ([PEP 696]) raises the bar to `min_version >= 3.13` — the
default syntax is not valid on 3.12, and the erased polyfill can't stand in
because it discards the native parameter list reification depends on

## explicit or inferred specialization

a reified generic is structurally a two-step callable (`f[...]` then `(...)`),
and the first step is not optional — but it doesn't have to be written. when a
reified type parameter appears in the signature, a bare call reifies it
through inference: the transpiler injects the statically inferred
specialization at the call site, so the two-step call still happens at runtime

```by
def f[T](t: T):
    print(1 is T)

f(1)    # transpiles to f[int](1) — prints True
f("")   # transpiles to f[str]("") — prints False
```

the injected argument is a *runtime type expression*: literal solutions
promote to their class first (`1` infers `int`, not a `Literal`), structured
annotations solve through their type arguments (`list[T]` + `[1]` → `int`),
and unions and tuples spell as `int | str` / `tuple[int, str]`. a class name
is injected only when the bare name resolves — in the module's globals or
builtins — to that same class, so the emitted expression evaluates to the
intended type object

when no injectable solution exists, the bare call is an error
(`unspecialized-reified-generic`):

```by
def f[T](t: object):
    print(T)

f[int](1)   # ok
f(1)        # error — `T` appears nowhere in the signature

def g[T](t: T):
    print(T)

def local():
    class Hidden: ...
    g(Hidden())   # error — `Hidden` has no runtime spelling at the call site

args = (1,)
g(*args)          # error — the injection cannot pass through unpacking
```

a type parameter with a default ([PEP 696]) fills its reified slot when
neither the call's arguments nor an explicit `[...]` supply one — and an
inferred solution beats the default, exactly like a passed argument beats a
parameter default:

```by
def d[T = int](t: T):
    print(T)

d("")       # transpiles to d[str]("") — the argument wins
d(0)        # prints int

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

> these rules are enforced today in the two places they can already be
> expressed: a reified generic is not assignable to a plain callable (the
> [reified-and-erased distinctness](#reified-and-erased-are-distinct) rule),
> and a method **override** must keep its base's reified interface — arity,
> positional matching, contravariant bounds, and reified/erased status are all
> checked at the definition and reported as `invalid-method-override` (see
> [overrides](#overrides-keep-the-reified-interface)). the writable
> `generic[[T], () -> bool]` type-form is not yet available as an annotation;
> examples written with it describe the intended behaviour

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

### overrides keep the reified interface

a method override is where these rules bite today: `a.f[int]()` on `a: A`
dispatches to the override at runtime, so the override must accept every
specialization the base permits. an incompatible list is reported at the
override's definition as `invalid-method-override`:

```by
class A:
    def f[T](self):
        print(T)

class B(A):
    def f[A2, B2](self):   # error — the base supplies one type argument
        print(A2, B2)
```

the same applies to bound narrowing (contravariance), to erasing a reified
method (the base's `f[...]` would subscript a plain function), and to
reifying an erased one without [PEP 696] defaults (a bare call through the
base could not supply the values)

## variadics and keyword packs

a `*Ts` reifies like any other type parameter, to the *run* of type arguments
it absorbs: the parameters before it claim theirs from the front, those after
it from the back, and the wrapper packs the rest into one tuple cell, so a
list containing a variadic has no upper arity bound

```by
def f[T, *Args]() -> None:
    print(T, Args)

f[int, str, bool]()  # T is int, Args is (str, bool)
f[int]()             # T is int, Args is ()
```

a variadic never makes the specialization step mandatory the way a plain
reified parameter does — supplying it nothing is a complete answer, not a
missing one — so a bare call stays legal and binds the empty run. the run is
not inferred from the call's arguments, so a non-empty one has to be written
out:

```by
def f[*Ts](*args: *Ts) -> None:
    print(Ts)

f(1, "a")            # Ts is ()
f[int, str](1, "a")  # Ts is (int, str)
```

a [PEP 696] default is a run too, and fills the slot as one:

```by
def f[*Ts = *tuple[int, str]]() -> None:
    print(Ts)

f()  # Ts is (int, str)
```

a [keyword-variadic pack](generics.md) reifies to the mapping of its fields.
the pack sits outside the positional slots entirely — the other parameters are
given positionally and the pack takes the keyword fields — and, like a
variadic, an unfilled one is empty rather than missing:

```by
def f[T, **Kwargs]() -> None:
    print(T, Kwargs)

f[int, foo=str]()  # T is int, Kwargs is {"foo": str}

def g[**Kwargs]() -> None:
    print(Kwargs)

g()  # Kwargs is {}
```

unlike a variadic, a pack *is* inferred from a call's keyword arguments when
`**kwargs: **Kwargs` unpacks it into the parameter list:

```by
def f[**Kwargs](**kwargs: **Kwargs) -> None:
    print(Kwargs)

f(a=1, b="x")  # Kwargs is {"a": int, "b": str}
```

python's subscript grammar takes no keywords, so a keyword specialization
lowers to the wrapper's `__getitem__` call — `f[int, foo=str]()` becomes
`f.__getitem__(int, foo=str)()`. an *erased* pack is stripped like any other
erased specialization, so nothing calls a `__getitem__` a plain function does
not have

> `**P` in a `.py` or `.byi` file is a [PEP 612] `ParamSpec`, not a pack, and
> is never reified: a parameter list has no runtime object to bind to a cell

## round-tripping

the reverse transpiler recognizes the `@generic` wrapper by its
`ReifiedGeneric` provenance in the `LoweringMap` and re-sugars it back to a
bare `def f[T]` with reified body usage, restoring the unstripped `f[int](...)`
call sites. a hand-written `@generic` decoration without that provenance is
left untouched

[pep 612]: https://peps.python.org/pep-0612/
[pep 696]: https://peps.python.org/pep-0696/
