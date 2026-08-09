# polyfills

a polyfill takes plain python source and rewrites it into equivalent code that runs on an older target — python in, older python out:

```python
class Map[K, V]: ...
```

transpiles to:

```python
from typing import TypeVar, Generic

_K = TypeVar("_K")
_V = TypeVar("_V")

class Map(Generic[_K, _V]): ...
```

basedpython backfills modern python syntax and stdlib features to older supported versions this way, at transpile time. minimum supported runtime is **python 3.10**

scope of this page: rewrites that apply to **plain python source** (forms a user could type into a `.py` file). basedpython-specific surface syntax has its own feature page

polyfills fall into a few categories:

- **syntax rewrites** — new grammar that basedpython desugars into equivalent 3.10 code
- **import redirects** — `typing` names that are not yet in 3.10/3.11/3.12 stdlib, transparently redirected to [`typing_extensions`](https://pypi.org/project/typing-extensions/)
- **expression rewrites** — simple attribute or call expressions with a direct 3.10 equivalent
- **stdlib shims** — functions or classes not yet in the stdlib, injected as pure-python implementations

______________________________________________________________________

## Python 3.14

### bracketless `except` (PEP 758)

`except` clauses without parentheses are rewritten to use them:

```python
# python source
except TimeoutError, ConnectionRefusedError:
    ...
```

```python
# generated Python
except (TimeoutError, ConnectionRefusedError):
    ...
```

### template strings / t-strings (PEP 750)

`t'...'` literals produce `string.templatelib.Template` objects. below 3.14 there is no runtime
t-string, so a [custom string tag](string-tags.md) — the only construct that currently emits one —
has its template rewritten to an explicit `Template(...)` constructor over a polyfill with the same
`strings` / `interpolations` shape. lowering of a bare standalone `t'...'` literal is planned for a
future release

### `operator.is_none` / `operator.is_not_none`

rewritten to lambda equivalents or inline expressions:

```python
# python source
filter(operator.is_none, items)
filter(operator.is_not_none, items)
```

```python
# generated Python
filter(lambda x: x is None, items)
filter(lambda x: x is not None, items)
```

### `heapq` max-heap functions

`heapq.heapify_max`, `heapq.heappush_max`, `heapq.heappop_max`, `heapq.heapreplace_max`, and `heapq.heappushpop_max` are injected as pure-python shims when used

### `datetime.date.strptime` / `datetime.time.strptime`

rewritten to the existing `datetime.datetime.strptime` with appropriate extraction:

```python
# python source
datetime.date.strptime("2024-01-15", "%Y-%m-%d")
datetime.time.strptime("14:30:00", "%H:%M:%S")
```

```python
# generated Python
datetime.datetime.strptime("2024-01-15", "%Y-%m-%d").date()
datetime.datetime.strptime("14:30:00", "%H:%M:%S").time()
```

______________________________________________________________________

## Python 3.13

### generic type parameter defaults (PEP 696)

`TypeVar` with a `default=` argument requires Python 3.13+. basedpython imports `TypeVar` from `typing_extensions` instead (which supports `default=`).
this applies when using PEP 695 generic syntax with a default (see the [generics polyfill](#generic-classes-and-functions-pep-695) below)

the `[T = int]` header syntax is itself 3.13+: on a 3.12 target, a declaration with a defaulted type parameter is desugared by the generics polyfill while declarations without defaults keep the native syntax. a [reified](reified-generics.md) function can't be desugared, so a defaulted reified function on a 3.12 target is a transpile error

### `typing.TypeIs` (PEP 742)

redirected to `typing_extensions.TypeIs`:

```python
# python source
from typing import TypeIs
```

```python
# generated Python
from typing_extensions import TypeIs
```

### `typing.ReadOnly` (PEP 705)

redirected to `typing_extensions.ReadOnly`

### `warnings.deprecated` (PEP 702)

redirected to `typing_extensions.deprecated`

### `copy.replace()`

injected as a pure-python shim that calls `obj.__replace__(**changes)`:

```python
# python source
from copy import replace
new = replace(obj, x=1)
```

```python
# generated Python
def _replace(obj, **changes):
    return obj.__replace__(**changes)
new = _replace(obj, x=1)
```

### `base64.z85encode` / `base64.z85decode`

injected as pure-python shims (the Z85 alphabet and algorithm are fully specifiable in Python)

______________________________________________________________________

## Python 3.12

### generic classes and functions (PEP 695)

the `[T]` type parameter syntax and `type` alias statement are desugared to `TypeVar`, `Generic`, and `TypeAlias`. see the detailed examples in the section below

### `typing.override` (PEP 698)

redirected to `typing_extensions.override` on 3.10–3.11

### `typing.TypedDict` with `Unpack` / `**kwargs` (PEP 692)

redirected to `typing_extensions.Unpack` on 3.10–3.11

### `itertools.batched()`

injected as a pure-python shim on 3.10–3.11:

```python
def _batched(iterable, n, *, strict=False):
    it = iter(iterable)
    while batch := tuple(itertools.islice(it, n)):
        if strict and len(batch) < n:
            raise ValueError("batched(): incomplete batch")
        yield batch
```

### `math.sumprod(x, y)`

injected as `sum(a * b for a, b in zip(x, y))`

### `pathlib.Path.walk()`

injected as a wrapper around `os.walk()`

### `random.binomialvariate(n, p)`

injected as a pure-python shim

______________________________________________________________________

## Python 3.11

### `typing.Self` (PEP 673)

redirected to `typing_extensions.Self`

### `typing.Never` / `typing.assert_never`

redirected to `typing_extensions.Never` / `typing_extensions.assert_never`

### `typing.LiteralString` (PEP 675)

redirected to `typing_extensions.LiteralString`

### `typing.Required` / `typing.NotRequired` (PEP 655)

redirected to `typing_extensions.Required` / `typing_extensions.NotRequired`

### `typing.TypeVarTuple` / `typing.Unpack` (PEP 646)

redirected to `typing_extensions.TypeVarTuple` / `typing_extensions.Unpack`

### `typing.dataclass_transform` (PEP 681)

redirected to `typing_extensions.dataclass_transform`

### `typing.reveal_type` / `typing.assert_type`

redirected to `typing_extensions.reveal_type` / `typing_extensions.assert_type`

### `datetime.UTC`

rewritten to `datetime.timezone.utc`:

```python
# python source
from datetime import UTC
```

```python
# generated Python
from datetime import timezone as UTC
```

or inline:

```python
# python source
datetime.UTC
```

```python
# generated Python
datetime.timezone.utc
```

### `sys.exception()`

rewritten to `sys.exc_info()[1]`:

```python
# python source
err = sys.exception()
```

```python
# generated Python
err = sys.exc_info()[1]
```

### `math.exp2(x)`

rewritten to `2 ** x`

### `math.cbrt(x)`

injected as a shim: `x ** (1 / 3)` for positive values, with sign handling for negative values

### `enum.StrEnum`

injected as a pure-python shim:

```python
class StrEnum(str, enum.Enum):
    pass
```

### `BaseException.add_note()`

injected as a monkey-patch on 3.10 when used:

```python
# python source
e.add_note("context")
```

```python
# generated Python
if not hasattr(e, "__notes__"):
    e.__notes__ = []
e.__notes__.append("context")
```

### `tomllib`

rewritten to fall back to `tomli` (the third-party backport):

```python
# python source
import tomllib
```

```python
# generated Python
try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib
```

______________________________________________________________________

## generic classes and functions (PEP 695)

python 3.12 introduced compact generic syntax. basedpython rewrites it using `typing.TypeVar` and `typing.Generic`

| basedpython                        | Python output                                   |
| ---------------------------------- | ----------------------------------------------- |
| `class A[T]: ...`                  | `class A(Generic[T]): ...`                      |
| `class A[T=int]: ...`              | `class A(Generic[T]): ...` (with `default=int`) |
| `class A[T: int]: ...`             | `class A(Generic[T]): ...` (with `bound=int`)   |
| `def f[T](x: T) -> T: ...`         | `def f(x: T) -> T: ...`                         |
| `type Point = tuple[float, float]` | `Point: TypeAlias = tuple[float, float]`        |

each type parameter becomes a module-level `TypeVar` with a mangled name (`_T`, `_K`, etc)

```python
# python source
class A[T=int]: ...
```

```python
# generated Python
from typing import TypeVar, Generic

_T = TypeVar("_T", default=int)  # from typing_extensions

class A(Generic[_T]): ...
```

multiple parameters:

```python
# python source
class Map[K, V]: ...
```

```python
# generated Python
from typing import TypeVar, Generic

_K = TypeVar("_K")
_V = TypeVar("_V")

class Map(Generic[_K, _V]): ...
```

existing base classes are preserved:

```python
# python source
class SortedMap[K, V](dict): ...
```

```python
# generated Python
class SortedMap(dict, Generic[_K, _V]): ...
```

generic functions:

```python
# python source
def identity[T](x: T) -> T:
    return x
```

```python
# generated Python
from typing import TypeVar

_T = TypeVar("_T")

def identity(x: T) -> T:
    return x
```

a bound (`T: Foo`) maps to `TypeVar("_T", bound=Foo)`. a [type mapping](type-mappings.md) (`T in (Foo, Bar)`) maps to `TypeVar("_T", Foo, Bar)`

`type` aliases:

```python
# python source
type Point = tuple[float, float]
type Grid[T] = list[list[T]]
```

```python
# generated Python
from typing_extensions import TypeAliasType

Point = TypeAliasType("Point", tuple[float, float])
Grid = TypeAliasType("Grid", list[list[T]])
```

______________________________________________________________________

## import redirect summary

when basedpython detects one of these names imported from `typing` on a runtime older than the version that added it,
it silently redirects to `typing_extensions`

| Name                         | Added in | Redirect source     |
| ---------------------------- | -------- | ------------------- |
| `Self`                       | 3.11     | `typing_extensions` |
| `Never`, `assert_never`      | 3.11     | `typing_extensions` |
| `LiteralString`              | 3.11     | `typing_extensions` |
| `Required`, `NotRequired`    | 3.11     | `typing_extensions` |
| `TypeVarTuple`, `Unpack`     | 3.11     | `typing_extensions` |
| `dataclass_transform`        | 3.11     | `typing_extensions` |
| `reveal_type`, `assert_type` | 3.11     | `typing_extensions` |
| `override`                   | 3.12     | `typing_extensions` |
| `TypeVar(default=...)`       | 3.13     | `typing_extensions` |
| `TypeIs`                     | 3.13     | `typing_extensions` |
| `ReadOnly`                   | 3.13     | `typing_extensions` |
| `deprecated` (warnings)      | 3.13     | `typing_extensions` |
