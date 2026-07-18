# basedpython: trailing lambda blocks

A statement-level expression followed by `:` and an indented suite calls that expression with the
suite as its last argument. The suite becomes a function taking the single implicit parameter `it`,
passed by keyword to the callee's last declared parameter (so earlier defaulted parameters keep
their defaults), or appended positionally when the callee's signature is not inspectable.

## the block is the call's last argument, `it` is context-typed

`it` takes the sole positional parameter type of the callable the callee's last parameter is
declared as.

```by
def f(x: int, a: (int) -> str):
    a(x)

f(2):
    reveal_type(it)  # revealed: int
```

## earlier defaulted parameters keep their defaults

A required parameter may follow a defaulted one in a `def` — the trailing block binds the last
parameter by keyword, so `x` keeps its default.

```by
def f(x: int = 1, a: (int) -> str):
    a(x)

f:
    print(it)

f(2):
    print(it)
```

## earlier required parameters still need arguments

```by
def g(x: int, a: (int) -> str):
    a(x)

g:  # error: [missing-argument]
    print(it)
```

## a non-callable target is an error

```by
x = 5

x:  # error: [call-non-callable]
    print(it)
```

## the trailing block counts toward the callee's arity

When the call already supplies every parameter, the appended block overfills the last parameter.

```by
def f(x: int, a: (int) -> str):
    a(x)

# error: [parameter-already-assigned]
f(1, lambda (n: int) -> str: str(n)):
    print(it)
```

## unknown callees degrade gracefully

The block is appended positionally and `it` is untyped.

```by
# error: [unresolved-import]
from nowhere import f

f(2):
    reveal_type(it)  # revealed: Unknown
```

## method callees

```by
class Runner:
    def run(self, a: (int) -> str):
        a(1)

Runner().run:
    reveal_type(it)  # revealed: int
```

## callable-typed values

A value of callable type can take a trailing block too; its last parameter's callable type gives
`it` its type.

```by
def call_with(consumer: ((int) -> None) -> None):
    consumer:
        reveal_type(it)  # revealed: int
```

## nested blocks

Each block's `it` is the innermost one.

```by
def f(a: (int) -> None):
    a(1)

f:
    print(it)
    f:
        reveal_type(it)  # revealed: int
```

## not valid in `.py` files

The shape parses exactly as it does in upstream python — an annotated assignment missing its
annotation — so `.py` diagnostics are unchanged.

```py
def f(a): ...

f:  # error: [invalid-syntax]
# error: [invalid-syntax] "Unexpected indentation"
# error: [unresolved-reference] "Name `it` used when not defined"
# error: [invalid-syntax] "Expected a statement"
    print(it)
```
