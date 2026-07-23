# unsafe unions

`UnsafeUnion[A, B]` is a gradual type whose materializations are exactly `A` and
`B`. it fuses a union with an intersection: a union on the way *in*, an
intersection on the way *out*

```by
from ty_extensions import UnsafeUnion

def f(a: UnsafeUnion[int, str]):
    a.imag    # ok — `int` has it
    a.upper() # ok — `str` has it

f(1)   # ok
f("s") # ok
```

it is `Any` restricted to a finite menu. because the menu is finite, the type
still rejects things: `UnsafeUnion[int, str]` is not assignable to `bytes`, and a
member that neither `int` nor `str` has is still an error

```by
from ty_extensions import UnsafeUnion

def takes_bytes(x: bytes): ...

def f(a: UnsafeUnion[int, str]):
    takes_bytes(a)   # error — neither materialization is a `bytes`
    a.nonexistent    # error — no materialization has it
```

## the two faces

- **in** (target position) it behaves as `A | B`: every materialization is a
    valid thing to store, so it accepts whatever the plain union accepts
- **out** (source position) it behaves as an intersection: the value *is* one of
    its materializations, so it goes wherever any single materialization can go,
    and offers the members of all of them

neither face is a subtype relation. like every gradual type, an unsafe union is a
subtype only of `object`. its top materialization is `A | B` and its bottom
materialization is `A & B`

it is disjoint from a type only when *every* materialization is:
`UnsafeUnion[int, str]` overlaps `int`, and is disjoint from `None`

## where it comes from

writing `UnsafeUnion[...]` by hand is rare. the type exists mainly because ty
infers it for an overload call that stays ambiguous because an argument is
[`dynamic`](dynamic.md)

```by
from typing import overload

@overload
def f(a: int) -> int: ...
@overload
def f(a: str) -> str: ...
def f(a: int | str) -> int | str:
    return a

def m(a: dynamic):
    reveal_type(f(a))  # UnsafeUnion[int, str]
```

[step 5 of the overload call evaluation algorithm][overloads] says such a call
evaluates to `Any`, throwing away everything we know. the call can only return an
`int` or a `str`, and that is exactly what an unsafe union describes — so the
result stays usable as either, while a `bytes` is still rejected

the same applies to an overloaded constructor whose `__new__` can return
something other than an instance of the class, and to a metaclass `__call__`

## simplification

a menu of one is not a choice, and nested menus flatten:

```by
UnsafeUnion[int, int]                    # int
UnsafeUnion[int, UnsafeUnion[str, bytes]] # UnsafeUnion[int, str, bytes]
```

the order of the menu does not matter: `UnsafeUnion[int, str]` and
`UnsafeUnion[str, int]` are the same type

a materialization that admits every type swallows the menu, so
`UnsafeUnion[int, Any]` is just `Any`. `Never` is uninhabited and contributes no
values to choose from, so `UnsafeUnion[int, Never]` is `int`

[overloads]: https://typing.python.org/en/latest/spec/overload.html#overload-call-evaluation
