# basedpython: a name that is not bound where the program reads it

a basedpython lowering that writes a type into code that runs — a specialization the checker solved,
a soundness check, a type test — reads each name in it where that code runs. a binding made under
`if TYPE_CHECKING:` exists for the checker and never runs, which is how an import cycle is broken; a
binding made only on some paths, or only further down the module, is not there yet either. a
lowering leaves such a name out, and where the program needs the type at runtime to do what the
source asks, that is an error here rather than a `NameError` when the program runs

## A specialization the program only refines is left out

an empty `set()` declared as a `set[Decimal]` is a set whatever it is specialized as, so the
specialization the checker solved is left out rather than written with a name the program does not
bind

```by
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from decimal import Decimal

class Ledger:
    entries: set[Decimal]

    def __init__(self):
        self.entries = set()

totals: set[Decimal] = set()
assert Ledger().entries == totals
```

## `TYPE_CHECKING` under another name

the constant is the same constant whatever name it is imported under, and a block it guards is a
type-checking block all the same

```by
from typing import TYPE_CHECKING as CHECKING

if CHECKING:
    from decimal import Decimal

class Ledger:
    entries: set[Decimal]

    def __init__(self):
        self.entries = set()

assert Ledger().entries == set()
```

## A block on any condition that reads `TYPE_CHECKING`

the checker reads the constant as `True` and the program reads it as `False`, so a block whose
condition reads it may be taken by the checker and not by the program, whatever the condition does
with it. a binding made past such a condition is not one the program can be relied on to make, and
this does not ask which way the condition goes: `if not TYPE_CHECKING:` and
`if TYPE_CHECKING and ready:` count all the same

### A `match` on the constant

```by
from typing import TYPE_CHECKING

match TYPE_CHECKING:
    case True:
        from decimal import Decimal
    case _:
        pass

class Ledger:
    entries: set[Decimal]

    def __init__(self):
        self.entries = set()

assert Ledger().entries == set()
```

### A comparison with the constant

```by
from typing import TYPE_CHECKING

# error: [redundant-boolean-comparison]
if TYPE_CHECKING == True:
    from decimal import Decimal

class Ledger:
    entries: set[Decimal]

    def __init__(self):
        self.entries = set()

assert Ledger().entries == set()
```

### A call on the constant

```by
from typing import TYPE_CHECKING

if bool(TYPE_CHECKING):
    from decimal import Decimal

class Ledger:
    entries: set[Decimal]

    def __init__(self):
        self.entries = set()

assert Ledger().entries == set()
```

### The constant bound by a walrus

```by
from typing import TYPE_CHECKING

if checking := TYPE_CHECKING:
    from decimal import Decimal

class Ledger:
    entries: set[Decimal]

    def __init__(self):
        self.entries = set()

assert Ledger().entries == set()
```

### A `while` on the constant

```by
from typing import TYPE_CHECKING

while TYPE_CHECKING:
    from decimal import Decimal
    break

class Ledger:
    entries: set[Decimal]

    def __init__(self):
        self.entries = set()

assert Ledger().entries == set()
```

### A value chosen by the constant

a conditional expression on the constant binds one value for the checker and another for the
program. here the checker reads `Decimal` as the class, so a value declared as one gets a soundness
check, but the program binds `None`, which the check could not test against, so it is left out

```by
from typing import TYPE_CHECKING
from decimal import Decimal as RuntimeDecimal

Decimal = RuntimeDecimal if TYPE_CHECKING else None

def parse(text: str) -> dynamic:
    return RuntimeDecimal(text)

price: Decimal = parse("1.5")
assert price == RuntimeDecimal("1.5")
```

## An import made only on some paths

a name imported under a condition the program decides as it runs may not be bound when it is read

```by
import os

if os.environ.get("BASEDPYTHON_NEVER_SET") is not None:
    from decimal import Decimal

class Ledger:
    entries: set[Decimal]

    def __init__(self):
        self.entries = set()

assert Ledger().entries == set()
```

## A class declared further down the module

the module's own statements run in order, so a statement above a class's declaration runs before the
class exists. a method runs later, when the module has declared everything

```by
early: set[Later] = set()

class Later: ...

class Ledger:
    entries: set[Later]

    def __init__(self):
        self.entries = set()

assert early == Ledger().entries
```

## A name the module unbinds

a `del` unbinds a name, so by the time a later statement runs the name may be gone

```by
from decimal import Decimal

class Ledger:
    entries: set[Decimal]

ledger = Ledger()
del Decimal
ledger.entries = set()
assert ledger.entries == set()
```

## A name bound in one branch of a conditional expression

a binding inside a conditional expression is made only when that branch is taken

```by
import os
from decimal import Decimal as RuntimeDecimal

_ = (Decimal := RuntimeDecimal) if os.environ.get("BASEDPYTHON_NEVER_SET") is not None else None
entries: set[Decimal] = set()
assert entries == set()
```

## An instance made before a class its declaration names

a method runs when it is called, which can be before the module reaches a class it names: here
`Ledger()` runs above `class Later`

```by
class Ledger:
    later: set[Later]

    def __init__(self):
        self.later = set()

ledger = Ledger()

class Later: ...

assert ledger.later == set()
```

## A reified type argument the program does not bind

a reified type parameter is read at runtime, so a construction or a call that would pass it a class
the program does not bind is reported

```by
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from decimal import Decimal

class Box[reified T]:
    def kind(self) -> str:
        return T.__name__

def kind_of[reified T](value: T) -> str:
    return T.__name__

class Holder:
    box: Box[Decimal]

    def __init__(self):
        # error: [unspecialized-reified-generic]
        self.box = Box()

def describe(value: Decimal) -> str:
    # error: [unspecialized-reified-generic]
    return kind_of(value)
```

## A type test or checked cast against a class the program does not bind

`is` and `cast!` test the value against the class at runtime, so they cannot test against one the
program does not bind

```by
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from decimal import Decimal

def test(value: object) -> bool:
    # error: [erased-type-check] "`is Decimal` cannot be checked at runtime: `Decimal` is not bound when it runs"
    return value is Decimal

def check(value: object) -> Decimal:
    # error: [erased-type-check] "`Decimal` cannot be checked at runtime: `Decimal` is not bound when it runs"
    return value cast! Decimal
```

## A failed test against an unbound class narrows

a type test against a class the program does not bind is reported, and the checker reads the rest of
the function as it would if the test could run: the branch where the test fails rules the class out

```by
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from decimal import Decimal

def whole(value: int | Decimal) -> int:
    # error: [erased-type-check] "`is Decimal` cannot be checked at runtime: `Decimal` is not bound when it runs"
    if value is Decimal:
        return 0
    reveal_type(value)  # revealed: int
    return value
```

## A reified call that has a reason of its own

where a call's specialization cannot be written for a reason of its own, as for a lambda's type,
that reason is the one reported, even when another argument names a class the program does not bind

<!-- snapshot-diagnostics -->

```by
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from decimal import Decimal

def pair[reified A, reified B](a: A, b: B) -> str:
    return A.__name__ + B.__name__

def describe(value: Decimal) -> str:
    # error: [unspecialized-reified-generic]
    return pair(value, lambda: 1)
```

## A type test at the module's own level

a test written at the module's own level runs as the module reaches it. a class the module unbinds
further down still holds where the test runs, so the test reads the class by the name the source
wrote, and the statements after it, the `assert`s among them, decide nothing about it

```by
class Shown: ...

def make() -> object:
    return Shown()

value = make()

assert value is Shown
assert not (value is int)
assert isinstance(value, Shown)

del Shown
```

## A class the program binds differently from the checker

a module may import a class for the checker and bind the name some other way as it runs, in the
`else:` of the same block. the checker reads the block as always taken, so the class it resolves is
the `TYPE_CHECKING` one, and the name the program binds need not be that class. a test against it
would answer for whatever the program bound, so it is refused

```by
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from decimal import Decimal
else:
    Decimal = float

def test(value: object) -> bool:
    # error: [erased-type-check] "`is Decimal` cannot be checked at runtime: the `Decimal` bound when it runs need not be the class the checker resolved"
    return value is Decimal

def check(value: object) -> Decimal:
    # error: [erased-type-check] "`Decimal` cannot be checked at runtime: the `Decimal` bound when it runs need not be the class the checker resolved"
    return value cast! Decimal
```

## An unchecked cast to a class the program does not bind

a plain `cast` tests nothing, so it is fine: its type is written as a string, which python never
evaluates

```by
from typing import TYPE_CHECKING
from decimal import Decimal as RuntimeDecimal

if TYPE_CHECKING:
    from decimal import Decimal

def widen(value: Decimal) -> Decimal | int:
    return value cast Decimal | int

assert widen(RuntimeDecimal(1)) == RuntimeDecimal(1)
```
