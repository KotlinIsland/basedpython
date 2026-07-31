# basedpython: `init(...)` method shorthand

basedpython lets a class declare its constructor with `init(...)` instead of `def __init__(...)`.
Parameters prefixed with `let` or `var` are auto-assigned to `self.<name>`, giving the class an
instance attribute of the annotated type. A `private` prefix name-mangles that attribute
(`self.__name`). The transpiler lowers `init` to `def __init__` and emits the self-assignments at
the top of the method body.

`self` may be omitted from the parameter list — it is implied.

## bodyless `init` with `let` params

```by
class A:
    init(self, let a: int, b: str)

x = A(1, "y")
reveal_type(x.a)  # revealed: int
```

## `init` with explicit body

```by
class A:
    init(self, a: int):
        self.b = str(a)

x = A(1)
reveal_type(x.b)  # revealed: str
```

## `let` parameter inside body-bearing `init`

```by
class A:
    init(self, let a: int):
        self.b = a * 2

x = A(5)
reveal_type(x.a)  # revealed: int
reveal_type(x.b)  # revealed: int
```

## multiple `let` parameters

```by
class Point:
    init(self, let x: int, let y: int)

p = Point(1, 2)
reveal_type(p.x)  # revealed: int
reveal_type(p.y)  # revealed: int
```

## non-`let` parameters are not attributes

A parameter without `let` is just a parameter — no `self.<name>` is created for it.

```by
class A:
    init(self, let a: int, b: str)

x = A(1, "y")
# `b` is a parameter, not an attribute
x.b  # error: [unresolved-attribute]
```

## `var` parameter creates an attribute

`var` is the mutable counterpart of `let`; for an `init` parameter it self-assigns just like `let`.

```by
class A:
    init(self, var a: int)

x = A(1)
reveal_type(x.a)  # revealed: int
```

## `self` is implied when omitted

The author may leave `self` out of the parameter list. It is still bound, so `let` / `var`
attributes and `self` references in the body resolve.

```by
class A:
    init(let a: int)

x = A(1)
reveal_type(x.a)  # revealed: int
```

```by
class B:
    init(var a: int):
        reveal_type(self.a)  # revealed: int
```

## `private` name-mangles the attribute

A `private let` / `private var` parameter self-assigns to the name-mangled `self.__name`. The
parameter itself keeps its declared name, so the constructor signature is unchanged.

```by
class A:
    init(self, private var a: int):
        reveal_type(self.__a)  # revealed: int

x = A(1)
# the public name is not an attribute — it is name-mangled
x.a  # error: [unresolved-attribute]
```

## call diagnostics name the class

`init` has no `__init__` in the source to point at, so a bad constructor call names the class the
caller actually wrote.

```by
class A:
    init(a: int)

# error: [invalid-argument-type] "Argument to class `A` is incorrect: Expected `int`, found `"s"`"
A("s")

# error: [missing-argument] "No argument provided for required parameter `a` of class `A`"
A()

# error: [too-many-positional-arguments] "Too many positional arguments to class `A`: expected 1, got 2"
A(1, 2)

# error: [unknown-argument] "Argument `b` does not match any known parameter of class `A`"
# error: [missing-argument] "No argument provided for required parameter `a` of class `A`"
A(b=1)
```
