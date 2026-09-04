## What it does

Checks for a basedpython destructuring binder whose pattern may not match the value it destructures,
with nothing to handle the failure.

## Why is this bad?

A destructuring binder — a `let` statement, a `for` target, a `with` item, a parameter — binds its
captures unconditionally. A pattern that does not match leaves them unbound, which is a `NameError`
at the first use.

A `let` statement can handle the failure with an `else` block, but only if the block diverges:
control that falls out of it reaches the same unbound captures.

## Examples

```by
def f(value: int | str) -> int:
    let int(n) := value  # error: [refutable-destructuring]
    return n

def g(value: int | str) -> int:
    let int(n) := value else:  # error: [refutable-destructuring]
        print("not an int")
    return n  # error: [possibly-unresolved-reference]
```

Use a pattern that matches every value of the type, or an `else` block that diverges:

```by
def f(value: int | str) -> int:
    let int(n) := value else:
        return 0
    return n
```
