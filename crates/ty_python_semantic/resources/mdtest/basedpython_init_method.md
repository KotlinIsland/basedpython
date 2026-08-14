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
# `str(a)` constructs, so the attribute's only assignment is exactly a `str` — see
# basedpython/features/exact-construction.md
reveal_type(x.b)  # revealed: final str
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

## a `let` attribute is read-only, a `var` attribute is not

The constructor binds a `let` parameter's attribute once, exactly like a class-body `let a: int`.
Writing to it afterwards is rejected; the `var` counterpart accepts the write.

```by
class A:
    init(let a: int, var b: int)

x = A(1, 2)
x.b = 3
x.a = 3  # error: [invalid-assignment]
```

An unannotated `let` parameter declares read-only state too — only the attribute's type is left to
the value.

```by
class Unannotated:
    init(let a)

Unannotated(1).a = 2  # error: [invalid-assignment]
```

## a class that only stores a `let` parameter is covariant

Nothing can write to a `let` attribute, so a widened view of the class cannot corrupt it and the
type parameter is inferred covariant. A `var` attribute is writable, which pins it invariant.

```by
class ReadOnly[T]:
    init(let t: T)

class Mutable[T]:
    init(var t: T)

def _(a: ReadOnly[int], b: Mutable[int]):
    widened: ReadOnly[object] = a
    also_widened: Mutable[object] = b  # error: [invalid-assignment]
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
