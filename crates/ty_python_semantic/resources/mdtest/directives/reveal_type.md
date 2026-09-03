# `reveal_type`

`reveal_type` is used to inspect the type of an expression at a given point in the code. It is often
used for debugging and understanding how types are inferred by the type checker.

## Basic usage

```py
from typing_extensions import reveal_type

reveal_type(1)  # revealed: Literal[1]
```

This also works with the fully qualified name:

```py
import typing_extensions

typing_extensions.reveal_type(1)  # revealed: Literal[1]
```

The return type of `reveal_type` is the type of the argument:

```py
from typing_extensions import assert_type

def _(x: int):
    y = reveal_type(x)  # revealed: int
    assert_type(y, int)
```

## Reporting the declared type

A declared place holds anything its annotation allows, while a load of it reads only what reaches
that point. `reveal_type` answers with the narrower type, and reports the declaration alongside it
so the bound on the place is visible too.

```py
from typing_extensions import reveal_type

a: int = 1

# snapshot: revealed-type
reveal_type(a)
```

```snapshot
info[revealed-type]: Revealed type
 --> src/mdtest_snippet.py:6:13
  |
6 | reveal_type(a)
  |             ^ `Literal[1]`
info: Declared type: `int`
```

Narrowing a parameter tells the same story the other way round: the declaration is the type the
narrowing started from.

```py
def narrowed(value: int | str) -> None:
    if isinstance(value, int):
        # snapshot: revealed-type
        reveal_type(value)
```

```snapshot
info[revealed-type]: Revealed type
  --> src/mdtest_snippet.py:10:21
   |
10 |         reveal_type(value)
   |                     ^^^^^ `int`
info: Declared type: `int | str`
```

Nothing is reported for a place no annotation declares.

```py
b = 1

# snapshot: revealed-type
reveal_type(b)
```

```snapshot
info[revealed-type]: Revealed type
  --> src/mdtest_snippet.py:14:13
   |
14 | reveal_type(b)
   |             ^ `Literal[1]`
```

Nor for a place whose declared type is the one the call already read.

```py
def unnarrowed(value: int) -> None:
    # snapshot: revealed-type
    reveal_type(value)
```

```snapshot
info[revealed-type]: Revealed type
  --> src/mdtest_snippet.py:17:17
   |
17 |     reveal_type(value)
   |                 ^^^^^ `int`
```

## Without importing it

For convenience, we also allow `reveal_type` to be used without importing it, even if that would
fail at runtime:

```py
reveal_type(1)  # revealed: Literal[1]
```

## In type-checking blocks

An unimported `reveal_type` cannot fail at runtime inside a `TYPE_CHECKING` block because that code
is never executed at runtime.

Note that this test uses `# error: [revealed-type]` assertions instead of the more common
`# revealed` assertions that we use elsewhere for `reveal_type` calls. `# revealed` assertions
swallow `undefined-reveal` errors as well as asserting the revealed type, but
`# error: [revealed-type]` assertions do not also match `undefined-reveal`. This means that an
unexpected so an unexpected `undefined-reveal` warning would cause these tests to fail.

```py
from typing import TYPE_CHECKING
import typing

if TYPE_CHECKING:
    reveal_type(1)  # error: [revealed-type] "Literal[1]"

    def nested() -> None:
        reveal_type("nested")  # error: [revealed-type] "nested"

if typing.TYPE_CHECKING:
    reveal_type(True)  # error: [revealed-type] "Literal[True]"
```

## In stub files

An unimported `reveal_type` also cannot fail at runtime in a stub file because stub files are never
executed.

As in the previous section, this test uses `# error: [revealed-type]` rather than `revealed:`
assertions to ensure that an unexpected `undefined-reveal` warning is not silently matched.

```pyi
reveal_type(1)  # error: [revealed-type] "Literal[1]"
```

## In unreachable code

Make sure that `reveal_type` works even in unreachable code.

### When importing it

```py
from typing_extensions import reveal_type
import typing_extensions

if False:
    reveal_type(1)  # revealed: Literal[1]
    typing_extensions.reveal_type(1)  # revealed: Literal[1]

if 1 + 1 != 2:
    reveal_type(1)  # revealed: Literal[1]
    typing_extensions.reveal_type(1)  # revealed: Literal[1]
```

### Without importing it

```py
if False:
    reveal_type(1)  # revealed: Literal[1]

if 1 + 1 != 2:
    reveal_type(1)  # revealed: Literal[1]
```
