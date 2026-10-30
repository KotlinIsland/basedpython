# basedpython: the scope of a type variable

A type variable is written by its name, and only says which scope it belongs to where another type
variable of that name is in the same type.

## a type variable is written by its name

```by
def f[T](x: T) -> T:
    reveal_type(x)  # revealed: T
    return x

class A:
    def f(self):
        reveal_type(self)  # revealed: Self
        return self
```

## two type variables of one name keep their scopes

```by
def f[T](x: T):
    # error: [shadowed-type-variable]
    def g[T](y: T):
        reveal_type((x, y))  # revealed: (T@f, T@g)
        # error: [invalid-assignment] "Object of type `T@f` is not assignable to `T@g`"
        a: T = x
```

## a python file spells the scope

```toml
[environment]
python-version = "3.12"
```

```py
def f[T](x: T) -> T:
    reveal_type(x)  # revealed: T@f
    return x
```
