# basedpython: keyword-variadic packs

in a `.by` file the PEP-695 spelling `[**Kwargs]` declares a *keyword-variadic pack* — an ordered
mapping of parameter name to type — rather than a `ParamSpec`. a pack is specialized by keyword
(`A[foo=int, bar=str]`) and unpacked with `**` in a callable arrow, where its fields become
keyword-only parameters

```toml
[environment]
python-version = "3.12"
```

## specialization by keyword

```by
class A[**Kwargs]:
    def get(self) -> (**Kwargs) -> None:
        raise NotImplementedError

def f(a: A[foo=int, bar=str]):
    reveal_type(a)  # revealed: A[foo=int, bar=str]
    reveal_type(a.get())  # revealed: (*, foo: int, bar: str) -> None
```

## calls against the unpacked pack

```by
class A[**Kwargs]:
    def get(self) -> (**Kwargs) -> None:
        raise NotImplementedError

def f(a: A[foo=int, bar=str]):
    g = a.get()
    g(foo=1, bar="x")
    g(foo="wrong", bar="x")  # error: [invalid-argument-type]
    g(foo=1)  # error: [missing-argument]
    # error: [too-many-positional-arguments]
    # error: [missing-argument] "No arguments provided for required parameters `foo`, `bar`"
    g(1, "x")
```

## inference from keyword arguments

`**kwargs: **Kwargs` unpacks the pack into a parameter list, so a call's keyword arguments solve it.
each argument contributes one field, in source order, with its type promoted the way an ordinary
inferred type argument is

```by
class A[**Kwargs]:
    def __init__(self, **kwargs: **Kwargs) -> None: ...

a = A(x=1, y="s")
reveal_type(a)  # revealed: A[x=int, y=str]

empty = A()
reveal_type(empty)  # revealed: A[()]
```

the pack is solved as a whole, so a class that also has ordinary type variables still infers them
positionally:

```by
class Two[T, **Kwargs]:
    def __init__(self, first: T, **kwargs: **Kwargs) -> None: ...

# TODO: `T` should promote to `bytes`. a generic context containing a parameter pack skips
# literal promotion for its other type variables — pre-existing, and shared with `ParamSpec`
reveal_type(Two(b"x", p=1.5))  # revealed: Two[b"x", p=float]
```

## the inferred pack flows into the rest of the class

```by
class A[**Kwargs]:
    def __init__(self, **kwargs: **Kwargs) -> None: ...
    def get(self) -> (**Kwargs) -> None:
        raise NotImplementedError

a = A(x=1, y="s")
reveal_type(a.get())  # revealed: (*, x: int, y: str) -> None
a.get()(x=1, y="v")
a.get()(x="wrong", y="v")  # error: [invalid-argument-type]
```

## an explicitly specialized pack checks the constructor

```by
class A[**Kwargs]:
    def __init__(self, **kwargs: **Kwargs) -> None: ...

A[x=int, y=str](x=1, y="s")
A[x=int, y=str](x="wrong", y="s")  # error: [invalid-argument-type]
A[x=int, y=str](x=1)  # error: [missing-argument]
```

## `**kwargs: **Pack` is rejected in a `.py` file

```py
# error: [invalid-syntax] "keyword-pack unpacking `**kwargs: **Pack` is not valid in .py files"
# error: [invalid-type-form] "Unpacked value for `**kwargs` must be a TypedDict, not `tuple[Unknown, ...]`"
# error: [invalid-type-form] "`*` can only unpack a tuple type or `TypeVarTuple`"
def f(**kwargs: **int): ...
```

## the star count follows the pack's declaration

`[**Kwargs]` is unpacked with `**`, the way `[*Ts]` is unpacked with `*`. a single star on a pack is
the `TypeVarTuple` spelling and does not parse here

```by
class A[**Kwargs]:
    # error: [invalid-syntax] "Starred expression cannot be used here"
    # error: [invalid-type-form] "Unpacked value for `**kwargs` must be a TypedDict, not `(*: Unknown)`"
    # error: [invalid-type-form] "Bare keyword-variadic pack `Kwargs` is not valid in this context in a parameter annotation"
    def __init__(self, **kwargs: *Kwargs) -> None: ...
```

## unpacking a pack into a dict literal type

`{**Kwargs}` splices the pack's fields into a [dict literal type](typed_dict_literal.md). the pack
contributes nothing until it is specialized, so the fields appear once the class is

### the pack's fields become the shape

```by
class A[**Kwargs]:
    def __init__(self, **kwargs: **Kwargs) -> None: ...
    def get(self) -> {**Kwargs}:
        raise NotImplementedError

def f():
    a = A(a=1, b="s")
    reveal_type(a.get())  # revealed: {"a": int, "b": str}
    reveal_type(a.get()["a"])  # revealed: int
    reveal_type(a.get()["b"])  # revealed: str
```

### alongside declared fields

a pack splices in beside ordinary keys, and the whole shape is ordered by key

```by
class A[**Kwargs]:
    def __init__(self, **kwargs: **Kwargs) -> None: ...
    def get(self) -> {"tag": int, **Kwargs}:
        raise NotImplementedError

def f():
    reveal_type(A(zzz=1).get())  # revealed: {"tag": int, "zzz": int}
```

### unspecialized, the pack stays pending

```by
class A[**Kwargs]:
    def get(self) -> {"tag": int, **Kwargs}:
        reveal_type(self.get())  # revealed: {"tag": int, **Kwargs@A}
        raise NotImplementedError
```

### the empty pack contributes no fields

```by
class A[**Kwargs]:
    def __init__(self, **kwargs: **Kwargs) -> None: ...
    def get(self) -> {"tag": int, **Kwargs}:
        raise NotImplementedError

def f():
    reveal_type(A().get())  # revealed: {"tag": int}
```

### a pack field shadows a declared key

the pack is spliced last, so a field it carries wins over the key written inline

```by
class A[**Kwargs]:
    def __init__(self, **kwargs: **Kwargs) -> None: ...
    def get(self) -> {"tag": int, **Kwargs}:
        raise NotImplementedError

def f():
    reveal_type(A(tag="s").get())  # revealed: {"tag": str}
```

## the empty pack

```by
class A[**Kwargs]:
    def get(self) -> (**Kwargs) -> None:
        raise NotImplementedError

def f(a: A[()]):
    reveal_type(a)  # revealed: A[()]
    reveal_type(a.get())  # revealed: () -> None
    a.get()()
```

## alongside ordinary type variables

a pack takes every keyword argument, so the other type variables are given positionally

```by
class Two[T, **Kwargs]:
    def get(self) -> (T, **Kwargs) -> None:
        raise NotImplementedError

def f(t: Two[bytes, foo=int]):
    reveal_type(t)  # revealed: Two[bytes, foo=int]
    reveal_type(t.get())  # revealed: (bytes, /, *, foo: int) -> None
    t.get()(b"", foo=1)
    t.get()(b"", foo="wrong")  # error: [invalid-argument-type]
```

## a prefix in the arrow

```by
class A[**Kwargs]:
    def prefixed(self) -> (int, **Kwargs) -> None:
        raise NotImplementedError

def f(a: A[foo=int]):
    reveal_type(a.prefixed())  # revealed: (int, /, *, foo: int) -> None
    a.prefixed()(3, foo=1)
```

## packs are invariant in their fields

```by
class A[**Kwargs]:
    def get(self) -> (**Kwargs) -> None:
        raise NotImplementedError

def f(one: A[foo=int], two: A[foo=int, bar=str]):
    a: A[foo=int] = one
    b: A[foo=int] = two  # error: [invalid-assignment]
```

## too many type arguments

```by
class A[**Kwargs]: ...

# error: [invalid-type-arguments] "Too many type arguments for class `A`: expected 1"
def f(a: A[int]): ...
```

## the parameter-list spelling is not the pack spelling

a pack has no positional fields, so the `ParamSpec` parameter-list form has nowhere to bind

```by
class A[**Kwargs]: ...

# error: [invalid-type-arguments] "Too many type arguments for class `A`: expected 1"
def f(a: A[(*, foo: int)]): ...
```

## a keyword argument that names no type variable

```by
class B[T]: ...

# error: [invalid-type-arguments] "No type argument provided for required type variable `T` of class `B`"
# error: [invalid-type-arguments] "No type variable named `Nope` for class `B`"
def f(b: B[Nope=int]): ...
```

## a bare pack is not a type

```by
class A[**Kwargs]:
    def get(self) -> Kwargs: ...  # snapshot
```

```snapshot
error[invalid-type-form]: Bare keyword-variadic pack `Kwargs` is not valid in this context in a return type annotation
 --> src/mdtest_snippet.by:2:22
  |
2 |     def get(self) -> Kwargs: ...  # snapshot
  |                      ^^^^^^
  |
info: A keyword-variadic pack is only valid:
info:  - unpacked with `**` in a callable parameter list
info:  - as the default for another keyword-variadic pack
info:  - as part of a type parameter list when defining a generic class
info:  - or as part of an argument list when specializing a generic class
```

## `**kwargs: T` in an arrow is still a catch-all

an annotated `**name: T` is an ordinary keyword-variadic parameter, not a pack unpacking

```by
def h(cb: (**kwargs: str) -> int):
    reveal_type(cb)  # revealed: (**kwargs: str) -> int
```
