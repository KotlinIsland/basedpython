# basedpython: `some` parameters

`some T` in a parameter's annotation gives the parameter a type parameter of its own, bounded by `T`
and named after the parameter. the rest of the signature names that type by the parameter's name, so
a function can hand back the exact type it was called with without declaring a type parameter list

## the argument's own type comes back

`echo` accepts any `str`, and returns the type of the argument it was given

```by
def echo(s: some str) -> s:
    return s

class Name(str):
    pass

reveal_type(echo)  # revealed: def echo(s: some str) -> s
reveal_type(echo("hi"))  # revealed: "hi"
reveal_type(echo(Name("x")))  # revealed: Name
```

## the bound is checked at the call

an argument outside the bound is rejected, as it is for a declared type parameter with that bound

```by
def echo(s: some str) -> s:
    return s

echo(1)  # error: [invalid-argument-type]
```

## the body reads the parameter's type

inside the body, the parameter has the type parameter's type

```by
def echo(s: some str) -> s:
    reveal_type(s)  # revealed: s@echo
    return s
```

## each `some` parameter is a type parameter of its own

two `some` parameters are solved independently, and a return type can name both

```by
def pair(a: some int, b: some str) -> tuple[a, b]:
    return (a, b)

reveal_type(pair(1, "x"))  # revealed: (1, "x")
```

## a union bound

the bound is any type expression, a union among them

```by
def either(v: some int | str) -> v:
    return v

reveal_type(either(1))  # revealed: 1
reveal_type(either("a"))  # revealed: "a"
```

## `some` is written at the top of a parameter annotation

`some` names the parameter's own type parameter, so it is not a type of its own. written anywhere
else — a return type, inside another type, a variable's annotation — it is reported where it stands,
and the type after it reads as though `some` were not there

```by
# error: [invalid-syntax] "`some` is only allowed at the top of a parameter annotation"
def f(n: int) -> some int:
    return n

# error: [invalid-syntax] "`some` is only allowed at the top of a parameter annotation"
def g(xs: list[some int]) -> None:
    reveal_type(xs)  # revealed: list[int]

# error: [invalid-syntax] "`some` is only allowed at the top of a parameter annotation"
x: some int = 1

reveal_type(f(1))  # revealed: int
```

## `some` is not written on `*args` or `**kwargs`

the annotation of a variadic parameter is the type of each element, while the parameter itself is a
tuple or a dict of them. the parameter's name cannot name both, so `some` is reported there, and the
annotation reads as though `some` were not there

```by
# error: [invalid-syntax] "`some` is not allowed on a variadic parameter: its annotation is the type of each element, not of the parameter"
def first(*xs: some int) -> int:
    reveal_type(xs)  # revealed: (*: int)
    return xs[0]

# error: [invalid-syntax] "`some` is not allowed on a variadic parameter: its annotation is the type of each element, not of the parameter"
def named(**kw: some int) -> int:
    reveal_type(kw)  # revealed: dict[str, int]
    return kw["a"]
```

## on a target without type parameter syntax

the type parameter is declared the way the target python can declare one, which includes an optional
bound on a python without `X | None`

```toml
[environment]
python-version = "3.9"
```

```by
def label(n: some int?) -> str:
    return "none" if n is None else str(n)

def echo(s: some str) -> s:
    return s

assert label(None) == "none"
assert label(3) == "3"
assert echo("hi") == "hi"
```
