# Repeated `_` parameters

basedpython relaxes the python rule that all parameter names must be unique: when the duplicated
name is `_`, the parameters are allowed and the transpiler renames trailing occurrences (`_2`, `_3`,
...) so the lowered python is valid. references to `_` inside the body always resolve to the first
parameter

## allowed in basedpython

```by
def f(_, _):
    reveal_type(_)  # revealed: _@f

def g(_, _, _, x: int):
    reveal_type(x)  # revealed: int

h = lambda _, _: 1
reveal_type(h)  # revealed: (_, _) -> 1
```

## still rejected for non-`_` names

```by
# error: [invalid-syntax] "Duplicate parameter "x""
def f(x: int, x: str) -> int:
    return 1
```

## still rejected in python

```py
# error: [invalid-syntax] "Duplicate parameter "_""
def f(_, _):
    return 1
```
