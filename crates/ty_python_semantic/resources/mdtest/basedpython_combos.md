# basedpython: combinations of tuple/callable/lambda

Combinations of named tuple types with lambda assignments to non-denotable callable types.

```toml
[environment]
python-version = "3.12"
```

## tuple type with named field — incompatible value errors

```by
a: (int, name: str) = (1, 1)  # error: [invalid-assignment]
```

## tuple type with named field — compatible value passes

```by
a: (int, name: str) = (1, "x")
reveal_type(a)  # revealed: (int, name: str)
reveal_type(a.name)  # revealed: str
```

## lambda assigned to non-denotable callable

```by
b: (int, name: str) -> str = lambda a, name: "asdf"
reveal_type(b(1, "y"))  # revealed: "asdf"
reveal_type(b(1, name="z"))  # revealed: "asdf"
```

## variadic callable

```by
v: (*: int) -> int = lambda *a: sum(a)
# variadic Protocol return narrows to the inferred lambda body type
reveal_type(v(1, 2, 3))  # revealed: UnsafeUnion[int, Unknown | 0]
```

## marker callable

```by
m: (int, /, name: str) -> bool = lambda x, name: True
# lambda body literal narrows the bidirectional return — bool widens elsewhere
reveal_type(m(1, name="ok"))  # revealed: True
```

## kwargs callable

```by
kw: (**kwargs: str) -> int = lambda **kw: len(kw)
reveal_type(kw(a="x", b="y"))  # revealed: int
```

## combined: tuple value + lambda + call

```by
a: (int, name: str) = (1, "x")
b: (int, name: str) -> str = lambda x, name: f"{x}-{name}"

reveal_type(a)  # revealed: (int, name: str)
reveal_type(b(a[0], a.name))  # revealed: f"{int}-{str}"
```
