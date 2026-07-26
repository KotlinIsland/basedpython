# extensions

an extension adds methods and computed properties to an existing type without
subclassing it or touching its definition (to make a type satisfy an *interface*
from the outside, see [implementations](implementations.md)):

```by
extension list:
    def second(self) -> Element:
        return self[1]
```

`xs.second()` is then available on any `list`, including the builtin one. the
extension reuses `list`'s own type parameter `Element` — it does not declare a
new one — so the return type tracks the element type with no extra ceremony

a method may reuse the extended type's parameters directly, by the names its
declaration gave them, and may also introduce its own fresh parameters:

```by
extension list:
    def foo[R](self, r: R) -> Element | R: ...
```

`Element` is not a new parameter and not a sigil — it is the name `list`'s
declaration bound (`class list[in out Element]` in basedpython's typeshed).
`[R]` is a fresh parameter the method declares normally

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

for a typeshed type the names follow basedpython's typeshed, which spells the
core containers `list[Element]`, `dict[Key, Value]`, `set[Element]`:

```by
extension dict:
    def invert(self) -> dict[Value, Key]: ...
```

referencing a name the extended type did not declare is an error — there is no
implicit free-parameter introduction, which is exactly what keeps the
"used before defined" case unreachable

## conditional extensions

a bound on a reused parameter narrows where the extension applies. it does not
re-declare the parameter — it constrains the receiver:

```by
extension list[Element: int]:
    def total(self) -> int:
        return sum(self)
```

`total` is visible on `list[int]` and `list[bool]`, not on `list[str]`. the
bound is spelled with basedpython's existing bracket-bound syntax so it reads
like every other bound in the language. parameters left out of the bracket stay
reused, unconstrained

constraint applicability is resolved by the type checker per call site, so an
extension can overlap a builtin or another extension and only apply to the
arm that satisfies its bound

## what an extension may add

extensions add behaviour, not state — methods, `class def`/`static def`
methods, and computed [properties](properties.md). they may not add stored
fields, because there is nowhere to store them on an already-constructed
instance of a builtin, and it keeps the feature implementable without touching
object layout

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
parameter is the receiver. annotations are dropped from the backing function
— they reference type parameters with no runtime binding at module level —
and a marker comment carries the member kind and the original header
(bounds included) as provenance for the reverse transpiler:

```by
extension list:
    def second(self) -> Element:
        return self[1]
```

→

```python
def _by_ext__list__second(self):  # basedpython: extension method list
    return self[1]
```

the name carries a single leading underscore deliberately: python applies
private-name mangling to any `__name` reference inside a class body, so a
double-underscore backing name would break an extension call written in one

when a module declares more than one extension of the same target, later
ones mangle with an ordinal (`_by_ext2__list__…`) so their members don't
collide — conditional extensions of the same method name coexist this way

call sites are rewritten by the type checker. ty already knows the receiver's
type and which extensions are in scope, so `xs.second()` resolves to the
backing function and lowers to a plain call:

```by
xs.second()
```

→

```python
_by_ext__list__second(xs)
```

computed properties lower the same way, minus the call parentheses:
`name.shouty` → `_by_ext__str__shouty(name)`

because the rewrite is type-directed, an extension call is never confused with a
real attribute. a method that happens to share a name with a real attribute
loses to the real attribute — extensions never shadow declared members

### implicit imports, lowered

when a call site uses an extension defined in another module, the lowering emits
the precise import of the backing function into the output, keyed off ty's
resolution of the call site:

```by
import textwrap

greeting.dedented()
```

→

```python
from textwrap import _by_ext__str__dedented

_by_ext__str__dedented(greeting)
```

so the surface stays `import textwrap`, and only the functions actually used are
imported — the implicit-import convenience costs nothing at runtime

## round-tripping

the reverse transpiler re-sugars both halves from the marker-comment
provenance: a backing function tagged `# basedpython: extension …` becomes an
`extension` block (consecutive same-header functions share one block), and a
call of a same-file backing function becomes receiver-method form —
`_by_ext__list__second(xs)` → `xs.second()`, property calls drop back to
bare attributes, `functools.partial(…, xs)` references to `xs.second`.
backing-shaped functions and calls written by hand without the marker are
left as ordinary python, and a call of a backing function *imported* from
another module conservatively stays as the explicit call

## current limitations

- an extension member cannot be reached through an optional chain
    (`xs?.second()`) yet — the transpiler reports an error rather than emit
    code that breaks the chain's short-circuit
- extension members resolve on a single receiver type, not across a union
- an unapplied method reference (`f = xs.second`) lowers to
    `functools.partial`, which binds the receiver eagerly like a bound method
    but is not one at runtime
