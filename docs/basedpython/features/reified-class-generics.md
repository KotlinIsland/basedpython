# reified class type parameters

a class's type parameter is erased in standard python: an `A[int]` and an
`A[str]` build instances of the same class, and nothing on the instance says
which one it was. basedpython keeps the type argument when the class actually
uses it as a value:

```by
class A[T]:
    def f(self):
        print(T)   # prints int

a: A[int] = A()
a.f()
```

the instance carries the argument it was constructed with, and every method
reads it back through its receiver

## it is the instance that knows

a [function's](reified-generics.md) type argument belongs to the call, so it
lives for the length of that call. a class's belongs to the *instance*: two
instances of one class can hold different arguments, and each method answers
for whichever receiver it was called on

```by
class Box[T]:
    def kind(self) -> type[T]:
        return T

Box[int]().kind()   # int
Box[str]().kind()   # str
```

that is also why the argument is there from the start — before `__init__`
runs, not after it — so a constructor can use it, which is most of the reason
to write a reified class at all:

```by
class Default[T]:
    value: T

    def __init__(self):
        self.value = T()

Default[int]().value   # 0
Default[str]().value   # ""
```

## a receiver has to be in scope

a read is answered through the receiver of the method it sits in. a function or
a class written *inside* a method closes over what that method read, so depth
costs nothing:

```by
class Box[T]:
    def kind(self):
        def inner():
            return T   # ok — closes over what `kind` read
        return inner()
```

but the class body itself, a method's decorators and its parameter defaults all
run while the class is still being built, and a `staticmethod` runs later but is
handed no receiver. a read in any of those has no instance to ask, and is an
error (`reified-without-receiver`) rather than a value invented from nothing:

```by
class Box[T]:
    kind = T                # error — the class body has no instance

    @staticmethod
    def make():
        return T()          # error — a static method has no receiver
```

a `classmethod` is fine: it is handed the class, and a specialization *is* a
class

```by
class Box[T]:
    @classmethod
    def kind(cls) -> type[T]:
        return T

Box[int].kind()   # int
```

## declaring it

reification is inferred from the body, which means it can also be declared.
`reified` ahead of a type parameter reifies it whether or not the class ever
reads it as a value:

```by
class Tagged[reified T]:
    pass
```

the specialization is required either way, so a class that promises a runtime
`T` keeps promising it when its methods stop printing one. it applies to a
variadic as well (`reified *Ts`)

what it does not take is a [variance](variance.md) keyword: reification has
already decided that, so `class C[reified out T]` is a contradiction rather than
a refinement and is reported (`invalid-variance-declaration`)

a keyword pack is the one parameter a class cannot reify: a class writes its
specialization as a subscript, and a subscript takes no keyword arguments, so
`class C[reified **Kwargs]` is an error (`invalid-reified-type-param`). nor can
a reified class define `__class_getitem__`, since that subscript is what records
the arguments an instance carries

## a construction says which specialization it builds

the argument has to be one the instance can carry, so the construction names
it. it usually doesn't have to be *written*: the transpiler injects the
specialization the type checker solved, from the declared type, from the
constructor's arguments, or from a [PEP 696] default

```by
class Box[T]:
    def kind(self) -> type[T]:
        return T

b: Box[int] = Box()   # from the declaration
Box[str]()            # written out
```

where nothing solves it, the bare construction is an error
(`unspecialized-reified-generic`) rather than an instance that would fail the
first time a method asked it a question

## a reified parameter is invariant

reading the argument back is something the program can do, so two
specializations of a reified class are interchangeable only when they were given
the same argument:

```by
class Box[T]:
    def kind(self) -> type[T]:
        return T

def take(box: Box[object]) -> None: ...

take(Box[int]())   # error — a Box[int] is not a Box[object]
```

an erased parameter keeps the variance it is
[inferred or declared](variance.md) with, so this is a property of reification
rather than of generic classes

## inheritance

a subclass supplies its base's argument the way it always has, and a generic
subclass passes its own along:

```by
class Box[T]:
    def kind(self) -> type[T]:
        return T

class IntBox(Box[int]):
    pass

class Wrapper[U](Box[U]):
    pass

IntBox().kind()        # int
Wrapper[str]().kind()  # str
```

a class that forwards its own parameter into a reified base is as reified as the
base is: `U` is invariant too, and its constructions are solved and checked the
same way

```by
w: Wrapper[int] = Wrapper()
w.kind()   # int
```

## desugaring

a class is reified when one of its type parameters is declared `reified` or is
read in a *value* position (anywhere other than a type annotation). the class is
decorated with the `generic_class` [polyfill](polyfills.md), and each method
that reads a parameter opens by binding it from its receiver:

```by
class A[T]:
    def f(self):
        print(T)

a: A[int] = A()
```

→

```python
@generic_class
class A[T]:
    def f(self):
        T = _type_argument(self, "T")
        print(T)

a: A[int] = A[int]()
```

`generic_class` replaces the class's `__class_getitem__`, so `A[int]` no longer
builds a `typing` alias but a memoized **subclass** of `A` recording the type
arguments. that is what makes them readable from `__new__` and `__init__`
onwards, where an `__orig_class__` stamp — which python applies only after
construction returns — is not yet there. being a real subclass is also what
keeps `isinstance(a, A)`, `class B(A[int])` and `__slots__` working, none of
which survive an alias standing in for a class. `__orig_class__` is carried on
the specialization as well, so anything that reads a runtime specialization —
[parametric type tests](parametric-type-tests.md) included — sees what it saw before

each specialization composes what it binds with what its bases already bound and
resolves the chain, which is how `class B[U](A[U])` specialized as `B[int]`
answers `T` with `int` rather than with `U`. binding the arguments onto the
parameter list is the same step a [reified function](reified-generics.md) takes,
so a variadic absorbs its run and a [PEP 696] default fills an omitted slot in
exactly the same way

an instance whose class was never specialized raises `TypeError` when a method
asks for an argument, rather than handing back the `TypeVar` the parameter would
otherwise still name — and so does one whose argument is a parameter a base was
never given, which is the same thing one level down

## what a specialization is not

`A[int]` is a class, where in python it would be a `typing` alias. that is what
makes the type argument available before `__init__`, and it is also the whole
cost of the feature:

- `typing.get_origin(A[int])` is `None` and `get_args` is empty — those read
    alias objects, and this is not one
- `pickle` cannot find `A[int]` by name, so instances of a specialization do not
    pickle
- a `@dataclass`'s generated `__eq__` compares `type`, so an `A[int]` and an
    `A[str]` with equal fields are no longer equal
- `A[int]` and `A[str]` are distinct classes, so anything keyed on `type(x)` sees
    two of them

a specialization is otherwise the class it specializes: it declares no `__slots__`
of its own, so a slotted class stays slotted, and `__init_subclass__` does not run
for it — it is the same class with its arguments fixed, not a subclass the program
wrote.

these costs are paid only by a class that reifies. worth knowing is that a class
opts in the moment one of its methods *reads* a type parameter, so adding a
`print(T)` to an existing generic class is what buys them; writing `reified` says
so on purpose.

## desugaring, continued

the lowered `class` keeps its native `[T]` syntax, because the specializer reads
the class's own `__type_params__` and the erased `Generic[T]` form has none.
that syntax is only available on python 3.12+, so reification requires
`min_version >= 3.12`; a defaulted parameter ([PEP 696]) raises the bar to
`min_version >= 3.13`. below either, a reified class is a transpile error rather
than code that cannot run

## round-tripping

the reverse transpiler recognizes the `@generic_class` wrapper by the marker
comment the forward transform writes on its line, and re-sugars it back to a
bare `class A[T]`, dropping the type-argument bindings its methods open with. a
hand-written `@generic_class` decoration without that marker is left untouched

[pep 696]: https://peps.python.org/pep-0696/
