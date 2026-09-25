# basedpython: calls to unannotated functions that never return

A statement-level call ends the flow when the function it calls cannot return, that is, when its
return type is `Never`. A function annotated `-> NoReturn` says so in its signature. With
`infer-unannotated-signatures` (which `sound-types` implies), a function that leaves its return type
out has it recovered from its body: when no path through the body reaches a `return` or the end of
the body without passing something that cannot complete — a `raise`, a loop that never exits, or
another call that cannot return — the function returns `Never`, and the code after a call to it is
unreachable.

```toml
[environment]
python-version = "3.13"

[analysis]
infer-unannotated-signatures = true
```

## a function that exits

`die` never returns, because `sys.exit` is declared `NoReturn`. The code after a call to it is
unreachable, so `x` is only ever `"test"` once the branches meet:

```py
import sys

def die():
    sys.exit(1)

def f(flag: bool):
    if flag:
        x = "terminal"
        die()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
```

A function that ends with such a call does not implicitly return `None`:

```py
def g() -> int:
    die()
```

## a chain of functions that exit

`a` calls `b`, which calls `sys.exit`, so neither returns:

```py
import sys

def b():
    sys.exit(1)

def a():
    b()

def f(flag: bool):
    if flag:
        x = "terminal"
        a()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]

def g() -> int:
    a()
```

## a function that sometimes returns

A function with a path that reaches the end of its body, or a `return`, can return, so the code
after a call to it stays reachable:

```py
import sys

def exit_if(flag: bool):
    if flag:
        sys.exit(1)

def exit_unless(flag: bool):
    if flag:
        return
    sys.exit(1)

def f(flag: int):
    if flag == 1:
        x = "first"
        exit_if(True)
    elif flag == 2:
        x = "second"
        exit_unless(False)
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["first", "second", "test"]

# error: [invalid-return-type]
def g() -> int:
    exit_if(True)
```

## `raise` in every branch

```py
def fail(flag: bool):
    if flag:
        raise ValueError
    else:
        raise TypeError

def f(flag: bool):
    if flag:
        x = "terminal"
        fail(flag)
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
```

## a loop that never exits

`while True:` without a `break` never falls through:

```py
def spin():
    while True:
        pass

def f(flag: bool):
    if flag:
        x = "terminal"
        spin()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
```

A `break` is a way out of the loop, and out of the function:

```py
def spin_until(flag: bool):
    while True:
        if flag:
            break

def g(flag: bool):
    if flag:
        x = "call"
        spin_until(flag)
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["call", "test"]
```

## `try` and `finally`

A `finally` suite runs on the way out, but the exception that `sys.exit` raises still propagates
after it. An exit in the `finally` suite itself ends every path through the `try`:

```py
import sys

def exits_in_try():
    try:
        sys.exit(1)
    finally:
        print("cleanup")

def exits_in_finally():
    try:
        print("work")
    finally:
        sys.exit(1)

def f(flag: int):
    if flag == 1:
        x = "try"
        exits_in_try()
    elif flag == 2:
        x = "finally"
        exits_in_finally()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
```

An `except` clause that catches the exception is a way out, whether the exception is raised in the
`try` suite itself or in a function it calls:

```py
def caught():
    try:
        sys.exit(1)
    except SystemExit:
        pass

def caught_from_a_call():
    try:
        exits_in_try()
    except SystemExit:
        pass

def g(flag: int):
    if flag == 1:
        x = "raised"
        caught()
    elif flag == 2:
        x = "called"
        caught_from_a_call()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["raised", "called", "test"]
```

A `return` in the `finally` suite swallows the exception, so a function whose `try` suite exits
still returns when its `finally` suite does. That holds whether the `try` suite exits itself or
calls a function that does:

```py
def returns_in_finally():
    try:
        sys.exit(1)
    finally:
        return

def returns_in_finally_after_a_call():
    try:
        exits_in_try()
    finally:
        return

def h(flag: int):
    if flag == 1:
        x = "exit"
        returns_in_finally()
    elif flag == 2:
        x = "call"
        returns_in_finally_after_a_call()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["exit", "call", "test"]
```

A `break` in the `finally` suite swallows the exception too, and the function goes on after the
loop. The flow after a `finally` suite is only followed from the state the `try` suite finished in,
so that way out is not seen yet:

```py
def breaks_in_finally():
    while True:
        try:
            sys.exit(1)
        finally:
            break

def k(flag: bool):
    if flag:
        x = "call"
        breaks_in_finally()
    else:
        x = "test"
    # TODO: `breaks_in_finally()` returns, so this should be `Literal["call", "test"]`
    reveal_type(x)  # revealed: Literal["test"]
```

## recursion

A function that calls itself unconditionally never returns: every call makes another, until
`RecursionError` is raised.

```py
def forever():
    forever()

def f(flag: bool):
    if flag:
        x = "terminal"
        forever()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
```

A recursive call behind a condition leaves the other path, which returns:

```py
def countdown(n: int):
    if n:
        countdown(n - 1)

def g(flag: bool):
    if flag:
        x = "call"
        countdown(3)
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["call", "test"]
```

Mutual recursion is read the same way. `ping` and `pong` only ever call each other, so neither
returns:

```py
def ping():
    pong()

def pong():
    ping()

def h(flag: bool):
    if flag:
        x = "terminal"
        ping()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
```

When one of them has a way out, both do:

```py
def tick(n: int):
    if n:
        tock(n - 1)

def tock(n: int):
    tick(n)

def k(flag: bool):
    if flag:
        x = "call"
        tock(3)
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["call", "test"]
```

## a body that only types can read

A body that exits only on a path that its types decide is read through those types, like any other
body.

`exit` is a builtin object rather than a function, and calling it never returns:

```py
import sys

def exits_through_a_builtin():
    exit(1)

def f(flag: bool):
    if flag:
        x = "terminal"
        exits_through_a_builtin()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
    reveal_type(exits_through_a_builtin())  # revealed: Never
```

A condition that is always true because of its type, rather than because it is a literal, is read
through that type:

```py
def exits_behind_isinstance():
    if isinstance(sys.argv, list):
        sys.exit(1)

def g(flag: bool):
    if flag:
        x = "terminal"
        exits_behind_isinstance()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
    reveal_type(exits_behind_isinstance())  # revealed: Never
```

A function that ends with such a call does not implicitly return `None`:

```py
def h() -> int:
    exits_behind_isinstance()
```

A method called on a module global is read through the global's type. `ArgumentParser.error` never
returns:

```py
import argparse

parser = argparse.ArgumentParser()

def usage(message: str):
    parser.error(message)

def k(flag: bool):
    if flag:
        x = "terminal"
        usage("bad")
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
```

A method called through `self` in a class with bases is read through the type of `self`:

```py
class Tool:
    def describe(self) -> str:
        return "tool"

class Runner(Tool):
    def die(self):
        sys.exit(1)

    def run(self):
        self.die()

def m(flag: bool, runner: Runner):
    if flag:
        x = "terminal"
        runner.run()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
```

A function called through another name bound to it is read through that name's type:

```py
def die():
    sys.exit(1)

alias = die

def through_an_alias():
    alias()

def n(flag: bool):
    if flag:
        x = "terminal"
        through_an_alias()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
```

## what a `return` hands back

A `return` that can never be reached hands nothing back, so it plays no part in the return type. A
function that exits before its only `return` returns `Never`, and a call to it ends the flow:

```py
import sys

def exits_before_returning():
    sys.exit(1)
    return 1

def f(flag: bool):
    if flag:
        x = "terminal"
        exits_before_returning()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
    reveal_type(exits_before_returning())  # revealed: Never
```

A `return` whose value never finishes evaluating returns `Never` too, but a call to it is still read
as one that reaches its `return`:

```py
def returns_an_exit():
    return sys.exit(1)

def g(flag: bool):
    if flag:
        x = "call"
        returns_an_exit()
    else:
        x = "test"
    # TODO: `returns_an_exit()` returns `Never`, so this should be `Literal["test"]`
    reveal_type(x)  # revealed: Literal["call", "test"]
    reveal_type(returns_an_exit())  # revealed: Never
```

## a callee that cannot be resolved

A call the checker cannot see through is treated as returning. A call to a parameter is one: which
function it calls is up to the caller.

```py
import sys

def call(callback):
    callback()

def exit_one():
    sys.exit(1)

def f(flag: bool):
    if flag:
        x = "call"
        call(exit_one)
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["call", "test"]
```

A name that is bound to one of two functions, depending on a condition, is another, even though
neither function returns:

```py
def exit_two():
    sys.exit(2)

def choose(flag: bool):
    if flag:
        chosen = exit_one
    else:
        chosen = exit_two
    chosen()

def g(flag: bool):
    if flag:
        x = "call"
        choose(True)
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["call", "test"]
```

A call to something that does not resolve at all returns too:

```py
from unknown_module import helper  # error: [unresolved-import]

def calls_unknown():
    helper()

def h(flag: bool):
    if flag:
        x = "call"
        calls_unknown()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["call", "test"]
```

## methods

A method and a `classmethod` are read the same way as a function, whether they are called directly
or through `self` and `cls`:

```py
import sys

class Exiter:
    def die(self):
        sys.exit(1)

    @classmethod
    def class_die(cls):
        sys.exit(1)

    def die_through_self(self):
        self.die()

    @classmethod
    def die_through_cls(cls):
        cls.class_die()

def f(flag: int, exiter: Exiter):
    if flag == 1:
        x = "method"
        exiter.die()
    elif flag == 2:
        x = "classmethod"
        Exiter.class_die()
    elif flag == 3:
        x = "self"
        exiter.die_through_self()
    elif flag == 4:
        x = "cls"
        Exiter.die_through_cls()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
```

A `staticmethod` is read the same way, whether it is called through the class directly or from the
body of another function:

```py
class Tools:
    @staticmethod
    def die():
        sys.exit(1)

def dies_through_the_class():
    Tools.die()

def h(flag: int):
    if flag == 1:
        x = "direct"
        Tools.die()
    elif flag == 2:
        x = "through a function"
        dies_through_the_class()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
```

Every class inherits what `object` defines, and all of it states its return type. A method that
overrides one of those, such as `__init__`, takes its return type from `object` rather than from its
body, so a call to it returns:

```py
class Resettable:
    def __init__(self):
        sys.exit(1)

    def reset(self):
        self.__init__()

def k(flag: bool, resettable: Resettable):
    if flag:
        x = "call"
        resettable.reset()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["call", "test"]
```

An override that leaves its return type out takes the one its base declares, so its body does not
decide whether a call to it returns:

```py
class Base:
    def run(self) -> int:
        return 1

class Child(Base):
    def run(self):
        sys.exit(1)

def g(flag: bool, child: Child):
    if flag:
        x = "call"
        child.run()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["call", "test"]
```

A base that leaves its own return type out supplies the one recovered from its body in turn, so an
override of a method that cannot return cannot return either:

```py
class Stopper:
    def stop(self):
        sys.exit(1)

class LoggingStopper(Stopper):
    def stop(self):
        print("stopping")

def m(flag: bool, stopper: LoggingStopper):
    if flag:
        x = "call"
        stopper.stop()
    else:
        x = "test"
    # TODO: `LoggingStopper.stop` can return, which the `Never` it takes from its base hides
    reveal_type(x)  # revealed: Literal["test"]
    reveal_type(stopper.stop())  # revealed: Never
```

A base whose return type cannot stand for an override's leaves the override's return type gradual,
and a call to it returns. Which type an overloaded base returns depends on the arguments, and a type
variable in the base's return type belongs to the base:

```py
from typing import overload

class Parser:
    @overload
    def parse(self, text: str) -> str: ...
    @overload
    def parse(self, text: bytes) -> bytes: ...
    def parse(self, text: str | bytes) -> str | bytes:
        return text

class StrictParser(Parser):
    def parse(self, text):
        sys.exit(1)

class Echo:
    def echo[T](self, value: T) -> T:
        return value

class SilentEcho(Echo):
    def echo(self, value):
        sys.exit(1)

def n(flag: int, parser: StrictParser, echo: SilentEcho):
    if flag == 1:
        x = "overloaded"
        parser.parse("text")
    elif flag == 2:
        x = "generic"
        echo.echo(1)
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["overloaded", "generic", "test"]
```

A narrowing return type is not handed to an override at all: it is a claim about what the base's
body tests, which the override's body need not test. The override's body decides instead:

```py
from typing_extensions import TypeIs

class Checker:
    def is_int(self, value: object) -> TypeIs[int]:
        return isinstance(value, int)

class FailingChecker(Checker):
    def is_int(self, value):
        sys.exit(1)

def p(flag: bool, checker: FailingChecker):
    if flag:
        x = "call"
        checker.is_int(1)
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
    reveal_type(checker.is_int(1))  # revealed: Never
```

## decorated functions

A decorator can replace the function it decorates, so a call to a decorated function returns
whatever the decorator's result does. Here that result is a function that returns, so neither call
ends the flow, though the body the decorator was handed exits:

```py
import sys
from typing import Callable

def replace(function) -> Callable[[], None]:
    return lambda: None

@replace
def exits():
    sys.exit(1)

def calls_decorated():
    exits()

def f(flag: int):
    if flag == 1:
        x = "direct"
        exits()
    elif flag == 2:
        x = "through a function"
        calls_decorated()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["direct", "through a function", "test"]
```

## callees in other modules

A call to a function in another module is read the same way, whichever form of import names the
function or its module:

`tools/exits.py`:

```py
import sys

def die():
    sys.exit(1)
```

`everything.py`:

```py
from tools.exits import *

def through_a_star_import():
    die()
```

`main.py`:

```py
import tools.exits as exits
from tools.exits import die
from everything import through_a_star_import

def through_an_alias():
    exits.die()

def through_the_name():
    die()

def f(flag: int):
    if flag == 1:
        x = "alias"
        through_an_alias()
    elif flag == 2:
        x = "name"
        through_the_name()
    elif flag == 3:
        x = "star import"
        through_a_star_import()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
```

A stub only exports what it re-exports, so a function it imports without re-exporting is not one of
its members, and a call that names it through the stub returns:

`lib/_impl.pyi`:

```pyi
from typing import NoReturn

def die() -> NoReturn: ...
def stop() -> NoReturn: ...
```

`lib/__init__.pyi`:

```pyi
from lib._impl import die as die
from lib._impl import stop
```

`uses_lib.py`:

```py
import lib

def dies():
    lib.die()

def stops():
    lib.stop()  # error: [unresolved-attribute]

def g(flag: int):
    if flag == 1:
        x = "re-exported"
        dies()
    elif flag == 2:
        x = "not re-exported"
        stops()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["not re-exported", "test"]
```

## generators and coroutines

Calling a generator function builds a generator without running the body, so the call returns even
when the body never does:

```py
import sys

def exits_later():
    sys.exit(1)
    yield 1

def f(flag: bool):
    if flag:
        x = "call"
        exits_later()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["call", "test"]
```

Calling an `async def` builds a coroutine, which is what returns; awaiting it runs the body:

```py
async def exits_when_awaited():
    sys.exit(1)

async def g(flag: bool):
    if flag:
        x = "created"
        exits_when_awaited()  # error: [unused-awaitable]
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["created", "test"]

    if flag:
        y = "awaited"
        await exits_when_awaited()
    else:
        y = "test"
    reveal_type(y)  # revealed: Literal["test"]
```

An `async def` that awaits one that never returns cannot return either once it is awaited:

```py
async def awaits_the_exit():
    await exits_when_awaited()

async def h(flag: bool):
    if flag:
        x = "awaited"
        await awaits_the_exit()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
```

## without the setting

Without `infer-unannotated-signatures`, a function that leaves its return type out returns
`Unknown`, and a call to it is treated as returning, whatever its body does:

```toml
[environment]
python-version = "3.13"

[analysis]
infer-unannotated-signatures = false
```

```py
import sys

def die():
    sys.exit(1)

def f(flag: bool):
    if flag:
        x = "call"
        die()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["call", "test"]
```

## `sound-types` implies the setting

`sound-types` turns `infer-unannotated-signatures` on, so a call to a function that cannot return
ends the flow even where the setting itself is off:

```toml
[environment]
python-version = "3.13"

[analysis]
infer-unannotated-signatures = false
sound-types = true
```

```py
import sys

def die():
    sys.exit(1)

def f(flag: bool):
    if flag:
        x = "terminal"
        die()
    else:
        x = "test"
    reveal_type(x)  # revealed: Literal["test"]
```

## a module-level call into a module that reads the package back

A package can call, at module level and behind a condition, a function in a submodule that reads the
package's own globals. Whether the rest of the package is reachable then depends on whether that
function returns, and reading its body must not need those globals to be known first:

`pkg/__init__.py`:

```py
import os

VALUE = os.environ.get("VALUE")

if "FLAG" in os.environ:
    import pkg.sub as sub

    sub.enable()
    AFTER = 2
```

`pkg/sub.py`:

```py
import sys

import pkg

def enable():
    check()

def check():
    if pkg.VALUE:
        sys.exit(1)
    sys.exit(2)
```

`main.py`:

```py
import pkg

reveal_type(pkg.VALUE)  # revealed: str | None
# error: [unresolved-attribute]
reveal_type(pkg.AFTER)  # revealed: Unknown
```

## a callee whose annotation depends on the call

A callee's return annotation can name a global of the module whose reachability the call decides.
Here `NoReturn = int` runs only once `enable()` has returned, and whether `enable()` returns depends
on what `pkg.NoReturn` is. Reading the call as one that does not return is consistent, and it is
what happens at run time: `stop` raises before `NoReturn = int` is reached, so `AFTER` is never
bound:

`pkg/__init__.py`:

```py
import os
from typing import NoReturn

if "FLAG" in os.environ:
    import pkg.sub as sub

    sub.enable()
    NoReturn = int
    AFTER = 1
```

`pkg/sub.py`:

```py
import pkg

def enable():
    stop()

def stop() -> pkg.NoReturn:
    raise SystemExit
```

`main.py`:

```py
from pkg import AFTER  # error: [unresolved-import]
```
