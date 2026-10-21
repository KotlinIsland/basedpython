# basedpython: `typeof` keyword

In basedpython, `typeof X` is sugar for `ty_extensions.TypeOf[X]`. It returns the static type of an
expression. The transpiler lowers `typeof X` to `TypeOf[X]` and adds the import.

## variable annotation

`typeof X` returns the inferred type of `X` (matching `ty_extensions.TypeOf[X]`)

```by
b = 1
a: typeof b = 1
reveal_type(a)  # revealed: 1
```

## attribute access

```by
class C:
    x: str = ""

c: C = C()

def f(a: typeof c.x) -> None:
    reveal_type(a)  # revealed: str
```

## return type

```by
b = 1

def f() -> typeof b:
    return 1

reveal_type(f())  # revealed: 1
```

## parameter annotation

```by
b = ""

def f(x: typeof b) -> None:
    reveal_type(x)  # revealed: ""
```

## composes with other type operators

```by
b = 1

def f(x: typeof b | None) -> None:
    reveal_type(x)  # revealed: 1 | None
```

## not valid in `.py` files

`typeof` is a basedpython-only keyword. A `.py` file using it gets a parse error from the parser.

```py
b: int = 1
# error: [invalid-syntax] "`typeof` keyword is not valid in .py files"
a: typeof b = 1
```
