# basedpython: forward references outside an annotation

an annotation is read as deferred, so it may name a class the module defines further down. the same
holds for the type arguments of a specialization written as a value, `list[F]()` or
`Alias = dict[str, F]`, and for the types a typing construct such as `TypeVar` takes as call
arguments: python evaluates these where they stand, so the transpiler writes such a name as the
quoted forward reference python reads lazily (`list["F"]()`)

```toml
[environment]
python-version = "3.12"
```

## a specialization's type arguments may name a later class

```by
from typing import Callable, Union

items = list[F]()
Alias = dict[str, F]
Maybe = Union[F, None]
Handler = Callable[[F], F]
nested = dict[str, list[F | None]]()

def use(alias: Alias, maybe: Maybe, handler: Handler):
    reveal_type(items)  # revealed: final list[F]
    reveal_type(alias)  # revealed: dict[str, F]
    reveal_type(maybe)  # revealed: F | None
    reveal_type(handler)  # revealed: (F, /) -> F
    reveal_type(nested)  # revealed: final dict[str, list[F | None]]

class F: ...
```

## a base's type arguments may name a later class

a base is the object the class is built on, so it is read where it is written, but its type
arguments are a specialization like any other

```by
class Base[T]:
    def get(self) -> T:
        raise NotImplementedError

class Bar(Base[F]): ...

def use(bar: Bar):
    reveal_type(bar.get())  # revealed: F

class F: ...
```

## a tuple type argument may name a later class

a type argument holding the reference is quoted whole, so the tuple type it lowers to is read lazily
with it

```by
from typing import TypeVar

class Foo[T]:
    def get(self) -> T:
        raise NotImplementedError

Alias = list[(F, int)]
pairs = dict[str, (F, int)]()
T = TypeVar("T", bound=(F, int))

class Bar(Foo[(F, int)]): ...

def use(alias: Alias, bar: Bar):
    reveal_type(alias)  # revealed: list[(F, int)]
    reveal_type(pairs)  # revealed: final dict[str, (F, int)]
    reveal_type(bar.get())  # revealed: (F, int)

class F: ...
```

## basedpython type syntax around a later class

an optional, an intersection or a callable type holding the reference is quoted whole, with what it
lowers to inside the quote

```by
class Foo[T]:
    def get(self) -> T:
        raise NotImplementedError

class G: ...

Maybe = list[F?]
Both = list[F & G]
Handler = list[(F) -> None]
Method = list[int.() -> F]
maybes = list[F?]()

class Bar(Foo[F?]): ...

def use(maybe: Maybe, both: Both, handler: Handler, method: Method, bar: Bar):
    reveal_type(maybe)  # revealed: list[F | None]
    reveal_type(both)  # revealed: list[F & G]
    reveal_type(handler)  # revealed: list[(F, /) -> None]
    reveal_type(method)  # revealed: list[(int, /) -> F]
    reveal_type(maybes)  # revealed: final list[F | None]
    reveal_type(bar.get())  # revealed: F | None

class F: ...
```

## a `Callable` parameter list written as a tuple may name a later class

each parameter of the list python's `Callable` takes is a type of its own

```by
from typing import Callable

Both = Callable[(F, int), F]
Returns = Callable[(int, str), F]

def use(both: Both, returns: Returns):
    reveal_type(both)  # revealed: (F, int, /) -> F
    reveal_type(returns)  # revealed: (int, str, /) -> F

class F: ...
```

## an anonymous named tuple may name a later class

its class is written ahead of everything the module binds, so a field naming a later class is a
forward reference there

```by
Fields = list[(first: F, count: int)]
fields: list[(first: F, count: int)] = []

def use(entries: Fields):
    reveal_type(entries[0].first)  # revealed: F

class F: ...
```

## a typing construct's type arguments may name a later class

```by
from typing import NamedTuple, NewType, TypedDict, TypeVar

T = TypeVar("T", bound=F)
Id = NewType("Id", F)
Pair = NamedTuple("Pair", [("first", F)])
Movie = TypedDict("Movie", {"lead": F})

def use(pair: Pair, movie: Movie, t: T) -> T:
    reveal_type(pair.first)  # revealed: F
    reveal_type(movie["lead"])  # revealed: F
    return t

class F: ...
```

## a name nothing binds is still unresolved

```by
items = list[Missing]()  # error: [unresolved-reference]
```

## a base itself is read where it is written

the class is built on the object, so a quote could not stand in for it, whether the class is generic
or not

```by
class Bar(F):  # error: [unresolved-reference]
    pass

class GenericBar[T](F):  # error: [unresolved-reference]
    pass

class Shaped((*: F)):  # error: [unresolved-reference]
    pass

class F: ...
```

## a keyword a class is built with is read where it is written

a metaclass, or a keyword `__init_subclass__` takes, is an object the class is built with

```by
class Tagged:
    def __init_subclass__(cls, tag: object = None):
        pass

class WithMeta(metaclass=Meta):  # error: [unresolved-reference]
    pass

class WithTag(Tagged, tag=F):  # error: [unresolved-reference]
    pass

class GenericWithMeta[T](metaclass=Meta):  # error: [unresolved-reference]
    pass

class Meta(type): ...
class F: ...
```

## a `TypedDict`'s `extra_items` may name a later class

`extra_items=` is the type of the keys a `TypedDict` does not list, so it is a type like a
specialization's arguments

```by
from typing_extensions import TypedDict

class Movie(TypedDict, extra_items=F): ...
class Pair[T](TypedDict, extra_items=list[F]): ...

class F: ...
```

## the bases of a class built by a call are read where they are written

`type()` builds the class on the base objects, as a `class` statement does

```by
import types

Built = type("Built", (F,), dict())  # error: [unresolved-reference]
NewBuilt = types.new_class("NewBuilt", (F,))  # error: [unresolved-reference]

class F: ...
```

## a reified class's type arguments are read where they are written

a reified class keeps its type arguments as runtime values, which a forward reference cannot stand
in for

```by
class Box[reified T]:
    pass

box = Box[F]()  # error: [unresolved-reference]
boxes = list[Box[F]]()  # error: [unresolved-reference]

class F: ...
```

## the type a test or a cast checks for is read where it is written

`is`, `cast!` and `cast?` check the value against the type when they run, and `cast` reads its type
as a type expression rather than a specialization

```by
from typing import cast

def make() -> object:
    return []

value = make()
is_list = value is list[F]  # error: [unresolved-reference]
checked = value cast? list[F]  # error: [unresolved-reference]
cast_list = cast(list[F], value)  # error: [unresolved-reference]

class F: ...
```

## a value inside a specialization is read where it is written

a `Literal`'s parameters and the metadata of an `Annotated` are values python evaluates while it
builds the specialization, even where the type around them is read as deferred

```by
from enum import Enum
from typing import Annotated, Literal

# error: [invalid-type-form]
# error: [unresolved-reference]
literals = list[Literal[E.A]]()
annotated = list[Annotated[int, F()]]()  # error: [unresolved-reference]

class E(Enum):
    A = 1

class F: ...
```
