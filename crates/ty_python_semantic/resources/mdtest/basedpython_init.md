# basedpython: `init(...)` shorthand

`init(...)` inside a class body is exactly `def __init__(...) -> None`.

```toml
[environment]
python-version = "3.12"
```

## `init` implies a `__init__` returning `None`, enforcing the signature

`m.byi`:

```byi
class C:
    init(self, x: int, name: str)
```

```by
from m import C

c = C(1, "a")
reveal_type(c)  # revealed: C

reveal_type(C.__init__)  # revealed: def __init__(self, x: int, name: str) -> None

# too few arguments
C(1)  # error: [missing-argument]
# wrong type
C("no", "a")  # error: [invalid-argument-type]
```

## `init` body cannot return a value

the synthesised `-> None` is enforced on the body too, so returning a value errors:

```by
class C:
    init(self, x: int):
        self.x = x
        return x  # error: [invalid-return-type]
```
