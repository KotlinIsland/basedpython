# extensions

> planned for `0.0.1a4` — not yet implemented

an extension adds methods and computed properties to an existing type without
subclassing it or touching its definition:

```by
extension list:
    def second(self) -> _T:
        return self[1]
```

`xs.second()` is then available on any `list`, including the builtin one. the
extension reuses `list`'s own type parameter `_T` — it does not declare a new
one — so the return type tracks the element type with no extra ceremony

## why swift-style

three languages solve "where do the extended type's parameters come from"
differently. the choice matters because of one failure mode: a parameter that
is used before it is bound

kotlin re-declares the parameters on the receiver and again on the method:

```by
# kotlin-style, rejected
def list[T].foo[T: int](self) -> T: ...
```

`T` appears in `list[T]` before the `[T: int]` that declares it, and the two
`T`s are different parameters that happen to share a spelling. imports are
explicit — every extension must be named to be used

dart declares two separate parameter lists and binds one to the other:

```by
# dart-style, rejected
extension[T] on list[T]:
    def foo[R](self, r: R) -> T | R: ...
```

this is unambiguous but verbose, and the binding `[T] on list[T]` restates a
fact the declaration of `list` already records

swift reuses the extended type's parameters directly, by the names its
declaration gave them:

```by
extension list:
    def foo[R](self, r: R) -> _T | R: ...
```

`_T` is not a new parameter and not a sigil — it is the name `list`'s
declaration bound (`class list[_T]` in typeshed). because it refers to an
existing declaration, it can never be used before it is defined. a method may
still introduce its own fresh parameters (`[R]` here), declared normally on the
method. basedpython takes the swift model

## reusing declared parameters

inside an extension, the extended type's parameters are in scope under the
names its declaration used. for a first-party class the names are yours:

```by
class Stack[T]:
    _items: list[T]

extension Stack:
    def peek(self) -> T:
        return self._items[-1]
```

for a typeshed type the names follow typeshed convention (`_T`, `_KT`, `_VT`):

```by
extension dict:
    def invert(self) -> dict[_VT, _KT]: ...
```

referencing a name the extended type did not declare is an error — there is no
implicit free-parameter introduction, which is exactly what keeps the
"used before defined" case unreachable

## conditional extensions

a bound on a reused parameter narrows where the extension applies. it does not
re-declare the parameter — it constrains the receiver:

```by
extension list[_T: int]:
    def total(self) -> int:
        return sum(self)
```

`total` is visible on `list[int]` and `list[bool]`, not on `list[str]`. this is
swift's `where _T: int`, spelled with basedpython's existing bracket-bound
syntax so it reads like every other bound in the language. parameters left out
of the bracket stay reused, unconstrained

constraint applicability is resolved by the type checker per call site, so an
extension can overlap a builtin or another extension and only apply to the
arm that satisfies its bound

## what an extension may add

extensions add behaviour, not state — methods, `class def`/`static def`
methods, and computed [properties](properties.md). they may not add stored
fields, because there is nowhere to store them on an already-constructed
instance of a builtin. this is the same boundary swift draws, and it keeps the
feature implementable without touching object layout

```by
extension str:
    @property
    def shouty(self) -> str:
        return self.upper()
```

## implicit imports

importing a module makes its extensions applicable — there is no per-extension
import. a plain `import mod` is enough for the type checker to consider every
extension `mod` defines:

```by
import textwrap

# textwrap's extensions on `str` are now in scope
greeting.dedented()
```

the transpiler wires up the runtime side automatically (see below), so
`import mod` carries the extensions without `from mod import dedented` ever
being written by hand

## lowering

python has no extension methods, and builtin C types cannot be monkey-patched
at runtime, so extensions are resolved entirely at transpile time — no runtime
machinery, the same approach the rest of basedpython takes

each extension member lowers to a module-level free function whose first
parameter is the receiver:

```by
extension list:
    def second(self) -> _T:
        return self[1]
```

→

```python
def __by_ext__list__second(self):
    return self[1]
```

call sites are rewritten by the type checker. ty already knows the receiver's
type and which extensions are in scope, so `xs.second()` resolves to the
backing function and lowers to a plain call:

```by
xs.second()
```

→

```python
__by_ext__list__second(xs)
```

computed properties lower the same way, minus the call parentheses:
`name.shouty` → `__by_ext__str__shouty(name)`

because the rewrite is type-directed, an extension call is never confused with a
real attribute. a method that happens to share a name with a real attribute
loses to the real attribute — extensions never shadow declared members

### implicit imports, lowered

when a call site uses an extension defined in another module, the lowering emits
the precise import of the backing function into the output, keyed off the
provenance ty recorded:

```by
import textwrap

greeting.dedented()
```

→

```python
from textwrap import __by_ext__str__dedented

__by_ext__str__dedented(greeting)
```

so the surface stays `import textwrap`, and only the functions actually used are
imported — the implicit-import convenience costs nothing at runtime

## round-tripping

the reverse transpiler re-sugars both halves from `LoweringMap` provenance:
a free function tagged `ExtensionMethod` becomes an `extension` block, and a
call tagged `ExtensionCall` becomes receiver-method form. backing functions and
calls written by hand without that provenance are left as ordinary python
