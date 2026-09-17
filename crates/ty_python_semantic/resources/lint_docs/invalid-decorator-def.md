## What it does

Checks for a `decorator def` whose parameters, or whose position, leave it with no decorator to
expand into.

## Why is this bad?

A `decorator def` is lowered to a pair of overloads and a runtime dispatcher, built from a first
parameter that receives the decorated function and options that each carry a default. A declaration
that does not have that shape has nothing to build from, and the lowering refuses it. Reported here,
the refusal arrives while the file is being checked rather than when it is transpiled.

## Examples

The first parameter is the one a decoration hands the function to, so there has to be one:

```by
# error: [invalid-decorator-def]
decorator def logged() -> int:
    return 1
```

Every shape the decorator is applied in supplies that function, so the parameter never falls back to
a default:

```by
def nothing() -> None: ...

# error: [invalid-decorator-def]
decorator def retry(fn: (...) -> object = nothing) -> int:
    return 1
```

The options are what `@traced` decorates without, so each of them needs a default:

```by
# error: [invalid-decorator-def]
decorator def traced(fn: (...) -> object, label: str) -> int:
    return 1
```

`decorator def` is module-scope only. A decorator written in a class body is a plain `def` that
returns a callable:

```by
class Registry:
    # error: [invalid-decorator-def]
    decorator def register(fn: (...) -> object) -> int:
        return 1
```

The form the lowering accepts is a decorated parameter followed by options that each have a default:

```by
decorator def traced(fn: (...) -> object, label: str = "") -> int:
    return len(label)
```
