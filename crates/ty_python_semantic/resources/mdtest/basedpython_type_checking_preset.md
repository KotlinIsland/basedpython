# basedpython: the type checking preset

`type-checking-preset` supplies the defaults that `rules` and `analysis` start from. `strict` is the
default and turns everything on; `ty-compatible` uses [ty](https://github.com/astral-sh/ty)'s own
defaults instead, leaving basedpython's diagnostics and analysis options off

## the default preset

```toml
[environment]
python-version = "3.13"
```

```py
def f[T]() -> T:
    raise NotImplementedError

reveal_type(f())  # revealed: Never
```

## ty-compatible turns basedpython's analysis options off

an unsolved type variable falls back to the gradual `Unknown`, the way ty solves it:

```toml
type-checking-preset = "ty-compatible"

[environment]
python-version = "3.13"
```

```py
def f[T]() -> T:
    raise NotImplementedError

reveal_type(f())  # revealed: Unknown
```

## a basedpython diagnostic under the default preset

```py
a: int = True  # error: [bool-as-int]
```

## ty-compatible turns basedpython's diagnostics off

`bool-as-int` is a basedpython diagnostic, so under this preset it doesn't exist:

```toml
type-checking-preset = "ty-compatible"

[environment]
python-version = "3.13"
```

```py
a: int = True
```

## a shared diagnostic still runs

a diagnostic ty has of its own is unaffected by the preset:

```toml
type-checking-preset = "ty-compatible"

[environment]
python-version = "3.13"
```

```py
# error: [unresolved-reference]
prin("hello")
```

## the analysis table beats the preset

```toml
type-checking-preset = "ty-compatible"

[environment]
python-version = "3.13"

[analysis]
precise-unsolved-typevars = true
```

```py
def f[T]() -> T:
    raise NotImplementedError

reveal_type(f())  # revealed: Never
```
