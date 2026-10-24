# boolean conditions

a condition collapses a value to a single bit. that is fine when the bit answers the question the
code is asking, and a bug when it does not. the checks below watch the ways it goes wrong

## `None` and another falsy value

`truthiness-test-of-none-union` fires when a condition tests an optional value whose other members
can be falsy too: `None` and the empty string share the branch, so the test cannot tell an absent
value from an empty one

```python
def f(name: str | None):
    if not name:      # warning: `""` and `None` are both falsy
        ...
    if name is None:  # ok — the members are told apart
        ...
```

an instance of a class with no `__bool__` and no `__len__` is taken to be truthy, so `if not a`
over an optional instance is not reported

## overlapping conditions

`overlapping-condition` reports the same mistake for every other sentinel: the branch a condition
*selects* holds a value that is always there alongside one that is only sometimes there. inside
that branch the two are indistinguishable, so whatever the condition was meant to ask, the answer
it got back conflates them

```python
from typing import Literal

def f(result: list[int] | Literal[False]):
    if result:           # ok — only a non-empty list is truthy
        ...
    if not result:       # warning: does not distinguish between `False` and `[]`
        ...
    if result is False:  # ok — the members are told apart
        ...
```

only the selected branch is analysed. `if a` looks at the truthy members, `if not a` at the falsy
ones — and a chain of `not`s flips the polarity each time, so `not not a` is a truthy test again

a boolean operator is not one condition but one per operand: each operand's truthiness is tested on
its own at runtime, and the operator's *value* — the union of the operands — is not a value anything
is tested for. `if count > 0 or leftovers:` asks two questions, each with one answer

```by
def f(a: list[int] | False, name: str | False):
    if not a or not name:  # two warnings, one per operand
        ...
```

a class settles its own case with `__bool__`. an instance that always answers `True` is never a
falsy member, so both directions of the test are clean

```by
class A:
    def __bool__(self) -> True:
        return True

def f(a: A | False):
    if a:      # ok — `A`
        ...
    if not a:  # ok — `False`
        ...
```

without `__bool__` (or `__len__`), a subclass could still be falsy, so `A` joins `False` in the
falsy branch and `if not a` is reported. the `analysis.overlapping-condition-assume-truthy-instances`
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
def f(a: str | bytes, b: str | False):
    if a: ...      # ok — both members are only partly here
    if not b: ...  # warning: `False` is always here, `""` only sometimes
```

members of one class — or of two classes where one derives the other — are one kind of value, and a
condition was never going to tell them apart either. `Literal[1] | Literal[2]` is not an overlap,
and neither is `list[A] | list[B]`: type arguments are not something truthiness can see.
`analysis.overlapping-condition-exempt-types` extends that to classes of your choosing, so a project
that does not mind conflating an empty `str` with anything else can say so

```toml
[analysis]
overlapping-condition-exempt-types = ["str"]
```

entries are qualified class names (`decimal.Decimal`); a class in `builtins` may also be spelled
bare. an entry that is not spelled like a class name is a
configuration error; one that is well-formed but resolves to nothing simply never matches

## redundant conditions

a condition whose outcome the tested type already fixes leaves one of its branches dead.
`redundant-condition` reports it, and `redundant-condition-strict` (off by default) reports the
cases more likely to be deliberate, such as a value that is `True` or `False` itself

```python
def f(point: tuple[int]):
    if point:  # warning: a 1-element tuple is always truthy
        ...
```

constants that only the checker's model of the build fixes — `TYPE_CHECKING`, `sys.version_info`,
`sys.platform`, `os.name`, and anything defined from them — are not reported: selecting a branch
with one is the entire point of writing it

```py
import sys
from typing import TYPE_CHECKING

if TYPE_CHECKING:                    # ok
    from collections.abc import Sequence

if sys.version_info >= (3, 12):      # ok
    ...
```

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
