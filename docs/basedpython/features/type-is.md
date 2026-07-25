# type narrowing predicates

basedpython spells [PEP 742][pep-742] `TypeIs[T]` as `name is T` in the return
annotation, naming the place being narrowed:

```by
def is_str(x) -> x is str:
    return isinstance(x, str)
```

transpiles to:

```python
from typing_extensions import TypeIs

def is_str(x) -> TypeIs[str]:
    return isinstance(x, str)
```

## semantics

the runtime semantics are exactly PEP 742 — the function asserts that its
argument has type `T` when it returns `True`, and the checker narrows
accordingly at call sites. the name is lost in lowering (`TypeIs` doesn't
carry it) but is preserved in the source for readers

## naming the parameter

`TypeIs` always narrows a function's first parameter. the name says which
parameter is meant, so any of them can be the narrowed one:

```by
def is_str(first: object, second: object) -> second is str:
    return isinstance(second, str)

def f(a: object, b: object):
    if is_str(a, b):
        b  # str
        a  # object
```

the name is matched against the argument the call passes for that parameter,
so a keyword argument narrows the same place

## narrowing a member

a guard can name a member of what it narrows, which is how a method vouches
for its own state:

```by
class Holder:
    data: str | None = None

    def ensure(self) -> asserts self.data is not None:
        if self.data is None:
            raise ValueError

    def loaded(self) -> self.data is str:
        return self.data is not None

def f(h: Holder):
    h.ensure()
    h.data  # str
```

the member follows the receiver the call was made on — `o.holder.ensure()`
narrows `o.holder.data`, and nothing else. a guard on a parameter's member
(`def ensure(h: Holder) -> asserts h.data is not None`) narrows the argument's
member the same way. python has no spelling for either, so both lower to what
the function returns

## narrowing a place

a name that is not a parameter is a *place* — narrowed where the call is
written rather than at an argument:

```by
a: int | None

def f() -> a is int: ...

def m():
    if f():
        a  # int
```

this has no PEP 742 spelling, so it lowers to `bool`

the place is resolved by name in the calling scope. a guard declared in
another file names a place in *that* module, which is a different symbol from
a same-named place at the call site, so it narrows nothing. for the same
reason there is no definition-site check that the narrowed type fits the
place: the place's type is whatever the calling scope has, and narrowing
intersects with it

## assertion guards

`asserts` declares a function that narrows once it *returns*, rather than one
whose result is tested:

```by
def check(x: int | None) -> asserts x:
    if x is None:
        raise ValueError

def f(a: int | None):
    check(a)
    a  # int
```

the narrowing is truthiness, the same as `if a:`, and it holds for the rest of
the flow — a later assignment to `a` ends it. `asserts not x` narrows the
other way:

```by
def check_empty(x: str | None) -> asserts not x:
    if x:
        raise ValueError
```

an assertion guard returns `None` — it raises when the assertion doesn't hold
— so it lowers to `-> None`:

```python
def check(x: int | None) -> None:
    if x is None:
        raise ValueError
```

`and` asserts every place it names:

```by
def check(a: int | None, b: str | None) -> asserts a is int and b:
    if a is None or not b:
        raise ValueError
```

an assertion narrows when it is called as a *statement*, which is where an
assertion is written. its value is the `None` it returns, so testing that value
(`if check(x):`) or binding it (`ok = check(x)`) is an error — it gets no
narrowing, and the test is always false. a call whose arguments are unpacked
(`check(*args)`) doesn't say which argument reached the parameter, so it
narrows nothing

## asserting a type

`asserts x is T` narrows by a type instead of by truthiness, and `is not`
removes one:

```by
def check(x: int | str | None) -> asserts x is int:
    if x is not int:
        raise ValueError

def require(x: int | None) -> asserts x is not None:
    if x is None:
        raise ValueError

def f(a: int | str | None, b: int | None):
    check(a)
    a  # int
    require(b)
    b  # int
```

the type is an ordinary type expression, so `asserts x is None` narrows to
`None`. as with a predicate, the asserted type has to fit the parameter it
narrows — `def check(x: int) -> asserts x is str` is an error, since no `int`
is ever a `str`. removing a type constrains nothing, so `is not` is
unrestricted

## naming nothing

a guard whose name is neither a parameter nor a place it can see narrows
nothing at every call site, which is almost always a typo:

```by
def check(value: int | None) -> asserts values:  # error: `values` is nothing
    if value is None:
        raise ValueError
```

## scope

the `place is T` rewrite fires only where the return annotation is a single
`is` comparison whose left side is a name or an attribute chain rooted at one.
this disambiguates from identity checks elsewhere in the function:

- in the return annotation: `x is str` → `TypeIs[str]`
- anywhere else: `x is y` follows the [identity-swap rules](identity-swap.md)
    and lowers to `isinstance(x, y)`

chained comparisons (`a is int is str`) and other left operands — a subscript,
a call — are ignored

[pep-742]: https://peps.python.org/pep-0742/
