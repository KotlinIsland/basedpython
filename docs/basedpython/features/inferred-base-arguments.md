# inferred base type arguments

a generic base written without type arguments takes them from the members the
class overrides:

```by
class Box[T]:
    item: T

    def get(self) -> T:
        return self.item

class IntBox(Box):
    item = 1

reveal_type(IntBox().get())  # int
```

transpiles to:

```python
class Box[T]:
    item: T

    def get(self) -> T:
        return self.item

class IntBox(Box[int]):
    item: int = 1

reveal_type(IntBox().get())
```

every member a class writes overrides the one its base declares, so it has to
fit the base's declaration. the type arguments the base is inherited at are the
ones that make every override fit, solved the way a call solves a generic
function's type parameters from its arguments

## what counts

an attribute counts by its annotation, or by the type its value declares —
`item = 1` declares `item: int`. an attribute that can be assigned through the
base has to match its declaration exactly, since a write through the base could
otherwise store a value the subclass rules out

a method counts by the types it writes itself, its annotated parameters and its
annotated return type:

```by
class Store[K, V]:
    def get(self, key: K) -> V:
        raise NotImplementedError

class Names(Store):  # Store[int, str]
    override def get(self, key: int) -> str:
        return ""
```

what a method leaves out says nothing about the base: an unannotated parameter
takes its type from the method it overrides, and a return type it does not
write is read off its body, which is typed in terms of the base being solved.
a decorated method, a constructor, a name-mangled `__name` and a member under a
visibility keyword do not count either

a type parameter with a default takes what the overrides solve it to, and its
default when none of them names it

## when the arguments are not inferred

the base needs its type arguments written, and `missing-type-argument` says so,
when:

- a type parameter without a default is named by no override

- no one specialization fits every override:

    ```by
    class Mixed(Box):  # error: missing type argument
        item = 1

        override def get(self) -> str:  # `item` asks for `Box[int]`
            return ""
    ```

- an override is typed by something the class header cannot name — a method's
    `Self`, a method's own type parameter, an anonymous `some` parameter — or by
    the class being defined, whose base is what is being solved

- the base cannot be subscripted as the class is made: a stub can call a class
    generic that is not, like `map`

a class's own type parameters can be named, so `class H[U](Box): item: U`
inherits `Box[U]`. type arguments written in the header are used as written

## editor support

the inferred arguments are shown as an inlay hint after the base, under the
`inferredBaseTypeArguments` setting
