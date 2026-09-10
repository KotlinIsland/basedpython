## What it does

Checks for a `def` written with no body at all, in a position that needs an implementation.

## Why is this bad?

A `def` with no body declares a signature. The lowering fills in `: ...`, so the function exists and
returns `None`. That is what a declaration means in a stub file; anywhere else it is an
implementation that was never written, silently stood in for by one that does nothing.

A body may be left out where a declaration is what the position asks for:

- in a stub file
- in an `if TYPE_CHECKING` block
- as a member of a protocol class
- as an `abstract def`, or an `@abstractmethod`-decorated method
- as an overload declaration, written `@overload` or as a run of same-name `def`s

An `init(...)` may also be written without a body: what it does is store the attribute parameters it
declares, and that body is built for it.

## Examples

```by
def parse(s: str) -> int  # ok: the run below makes this an overload declaration
def parse(s: bytes) -> int
def parse(s):
    return int(s)

# error: [missing-function-body]
def lookup() -> int
```

A function that is meant to do nothing says so with a body of its own:

```by
def ignore(event: str): ...
```
