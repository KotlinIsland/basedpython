# boolean conditions

a condition collapses a value to a single bit. that is fine when the bit answers the question the
code is asking, and a bug when it does not. three checks watch the ways it goes wrong

## overlapping conditions

`overlapping-condition` fires when the branch a condition *selects* holds a value that is always
there alongside one that is only sometimes there. inside that branch the two are
indistinguishable, so whatever the condition was meant to ask, the answer it got back conflates
them

```python
def f(a: bool | None):
    if a:      # ok — only `True` is truthy
        ...
    if not a:  # warning: does not distinguish between `False` and `None`
        ...
```

only the selected branch is analysed. `if a` looks at the truthy members, `if not a` at the falsy
ones — and a chain of `not`s flips the polarity each time, so `not not a` is a truthy test again

a boolean operator is not one condition but one per operand: each operand's truthiness is tested on
its own at runtime, and the operator's *value* — the union of the operands — is not a value anything
is tested for. `if count > 0 or leftovers:` asks two questions, each with one answer

```by
def f(a: bool?, name: str?):
    if not a or not name:  # two warnings, one per operand
        ...
```

the classic instance is an optional string, where the empty string and the absent value share a
branch

```python
def f(name: str | None):
    if not name:      # warning: `""` and `None` are both falsy
        ...
    if name is None:  # ok — the members are told apart
        ...
```

a class settles its own case with `__bool__`. an instance that always answers `True` is never a
falsy member, so both directions of the test are clean

```by
class A:
    def __bool__(self) -> True:
        return True

def f(a: A | None):
    if a:      # ok — `A`
        ...
    if not a:  # ok — `None`
        ...
```

without `__bool__` (or `__len__`), a subclass could still be falsy, so `A` joins `None` in the falsy
branch and `if not a` is reported. the `analysis.overlapping-condition-assume-truthy-instances`
option takes such a class at face value instead

```toml
[analysis]
overlapping-condition-assume-truthy-instances = true
```

two members that are *each* only partly in the branch are not reported. `if x:` over a `str | bytes`
puts a non-empty `str` next to a non-empty `bytes`, and neither of them was ever going to be
somewhere else — the union already conflated them and the condition added nothing. what the check is
looking for is the asymmetry: one member unconditionally here, another only in the corner of itself
that answers this way

```by
def f(a: str | bytes, b: str?):
    if a: ...      # ok — both members are only partly here
    if not b: ...  # warning: `None` is always here, `""` only sometimes
```

members of one class — or of two classes where one derives the other — are one kind of value, and a
condition was never going to tell them apart either. `Literal[1] | Literal[2]` is not an overlap,
and neither is `list[A] | list[B]`: type arguments are not something truthiness can see.
`analysis.overlapping-condition-exempt-types` extends that to classes of your choosing, so a project
that does not mind conflating a falsy `int` with anything else can say so

```toml
[analysis]
overlapping-condition-exempt-types = ["int"]
```

entries are qualified class names (`decimal.Decimal`); a class in `builtins` may also be spelled
bare, and `None` stands for the type of `None`. an entry that is not spelled like a class name is a
configuration error; one that is well-formed but resolves to nothing simply never matches

## redundant conditions

`redundant-condition` fires when the tested value's own type fixes the outcome, so one of the two
branches is dead

```by
def f(a: True):
    if a:  # warning: always true
        ...
```

the check applies to a value *read* — a name, an attribute, a subscript. a comparison or a call
computes a fresh value, and ty folding *that* one is the statically-known-branch machinery doing its
job: `elif isinstance(x, B):` closing an exhaustive chain is deliberate, and so is `while True`

what a read is worth differs by what ty knows about the place, so the same flag written two ways
does not report the same way. a *name* is read at its narrowed type, so a module-level constant is
one — its single assignment fixes the outcome and a branch guarded by it is dead. an attribute or a
subscript is read at its *declared* type, which for a `bool`-valued flag is `bool`, so the same
constant written as a class attribute is not reported

```by
DEBUG = False

if DEBUG:  # warning: always false — the branch is dead

class Settings:
    ENABLED: bool = False

    def run(self) -> None:
        if self.ENABLED:  # ok — an attribute reads as its declared `bool`
            ...
```

a constant flag whose dead branch is deliberate is what `# ty: ignore[redundant-condition]` is for,
or turn the rule off for the module. the [artificial](#artificial-truthiness) exemption below is not
it: that covers constants ty *manufactures*, not ones the program sets

## artificial truthiness

some constants are constant only because of how ty models the build it is checking for.
`TYPE_CHECKING`, `sys.version_info`, `sys.platform` and `os.name` all carry a fixed value that the
program itself never computes, and selecting a branch with one is the entire point of writing it.
their truthiness is *artificial*, and a redundant-condition report on it would be wrong

```py
import sys
from typing import TYPE_CHECKING

if TYPE_CHECKING:                    # ok
    from collections.abc import Sequence

if sys.version_info >= (3, 12):      # ok
    ...
```

`sys.version_info` has a type of its own and is recognised wherever it is bound. the others are
ordinary literals by the time they have a type, so they are recognised by the module they are read
off (`sys.platform`) or by their name (`TYPE_CHECKING`) — an import that renames them is missed

## redundant boolean comparisons

`redundant-boolean-comparison` fires on a comparison of a `bool` against `True` or `False`. the
operand already is the value the comparison produces

```python
def f(a: bool):
    if a == True:   # warning: redundant
        ...
    if a is False:  # warning: redundant — write `not a`
        ...
    if a:           # ok
        ...
```

an operand that is not a `bool` is left alone. `x == True` really does tell `True` apart from
`False` and `None` when `x` is a `bool?`, and it is a value comparison when `x` is an `int`. a
chained comparison (`a == True == b`) is left alone too — it is two comparisons over one literal,
and neither the operand nor its negation replaces the chain
