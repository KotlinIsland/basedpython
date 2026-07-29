# pytest (mock stubs)

Hermetic pins for the *mechanism* of ty's dedicated pytest support, using minimal hand-written stubs
that mirror the shapes pytest ships inline: the `@pytest.fixture` decorator (defined in
`_pytest.fixtures`). The `external/pytest.md` suite checks the same behaviours — plus builtin
fixtures and `parametrize` — against the real package.

pytest fills a test or fixture parameter by *name* from a registry: the function's own module, then
the `conftest.py` chain, then builtin fixtures. A parameter annotation that disagrees with the
resolved fixture's type is an error; the fixture's provided type is its return annotation, with a
yield fixture's `Iterator[T]` / `Generator[T, ...]` unwrapped to `T`. An *unannotated* parameter
takes that provided type instead of the implicit `Unknown` an ordinary parameter would get.

## Fixture resolution and type checking

A fixture defined in the same module resolves for a same-module test. The provided type is the
fixture's return type (yield fixtures unwrap to the yielded element); `@pytest.fixture(name=...)`
renames it. An annotation that the provided type is not assignable to is an error.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/pytest/__init__.pyi`:

```pyi
from _pytest.fixtures import fixture as fixture
```

`/.venv/<path-to-site-packages>/_pytest/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/_pytest/fixtures.pyi`:

```pyi
from typing import Any, Callable, overload

class FixtureFunctionDefinition: ...

class FixtureFunctionMarker:
    def __call__(self, function: Callable[..., Any]) -> FixtureFunctionDefinition: ...

@overload
def fixture(function: Callable[..., Any], *, scope: str = ..., name: str | None = None) -> FixtureFunctionDefinition: ...
@overload
def fixture(function: None = ..., *, scope: str = ..., name: str | None = None) -> FixtureFunctionMarker: ...
```

`test_resolution.py`:

```py
import pytest
from typing import Iterator

@pytest.fixture
def number() -> int:
    return 1

@pytest.fixture(name="renamed")
def _make_text() -> str:
    return "x"

@pytest.fixture
def payload() -> Iterator[bytes]:
    yield b""

def test_ok(number: int, renamed: str, payload: bytes) -> None:
    reveal_type(number)  # revealed: int
    reveal_type(renamed)  # revealed: str
    reveal_type(payload)  # revealed: bytes

def test_bad(number: str) -> None:  # error: [invalid-fixture-type]
    ...
```

## conftest chain and shadowing

A fixture from a `conftest.py` above the test file resolves for it. A fixture the module defines
itself shadows a `conftest.py` fixture of the same name, and a nearer `conftest.py` shadows a
farther one — pytest's resolution order.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/pytest/__init__.pyi`:

```pyi
from _pytest.fixtures import fixture as fixture
```

`/.venv/<path-to-site-packages>/_pytest/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/_pytest/fixtures.pyi`:

```pyi
from typing import Any, Callable, overload

class FixtureFunctionDefinition: ...

class FixtureFunctionMarker:
    def __call__(self, function: Callable[..., Any]) -> FixtureFunctionDefinition: ...

@overload
def fixture(function: Callable[..., Any], *, scope: str = ..., name: str | None = None) -> FixtureFunctionDefinition: ...
@overload
def fixture(function: None = ..., *, scope: str = ..., name: str | None = None) -> FixtureFunctionMarker: ...
```

`conftest.py`:

```py
import pytest

@pytest.fixture
def shared() -> int:
    return 1

@pytest.fixture
def overridden() -> int:
    return 1
```

`pkg/conftest.py`:

```py
import pytest

@pytest.fixture
def overridden() -> str:
    return "nearer"
```

`pkg/test_chain.py`:

```py
import pytest

@pytest.fixture
def local() -> bytes:
    return b""

def test_it(shared: int, overridden: str, local: bytes) -> None:
    # `shared` from the root conftest, `overridden` from the nearer `pkg/conftest.py`
    reveal_type(shared)  # revealed: int
    reveal_type(overridden)  # revealed: str
    reveal_type(local)  # revealed: bytes
```

## Unknown fixture is opt-in

A parameter that resolves to no fixture is reported only when `unknown-fixture` is enabled; it ships
off by default because plugin-provided fixtures are not yet discovered.

```toml
[environment]
python = "/.venv"

[rules]
unknown-fixture = "error"
```

`/.venv/<path-to-site-packages>/pytest/__init__.pyi`:

```pyi
from _pytest.fixtures import fixture as fixture
```

`/.venv/<path-to-site-packages>/_pytest/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/_pytest/fixtures.pyi`:

```pyi
from typing import Any, Callable, overload

class FixtureFunctionDefinition: ...

class FixtureFunctionMarker:
    def __call__(self, function: Callable[..., Any]) -> FixtureFunctionDefinition: ...

@overload
def fixture(function: Callable[..., Any], *, scope: str = ..., name: str | None = None) -> FixtureFunctionDefinition: ...
@overload
def fixture(function: None = ..., *, scope: str = ..., name: str | None = None) -> FixtureFunctionMarker: ...
```

`test_unknown.py`:

```py
def test_it(missing) -> None:  # error: [unknown-fixture]
    ...
```

## Unannotated parameters take the fixture's type

An unannotated parameter is bound by pytest to the fixture of the same name, so the body sees that
fixture's provided type — including through a rename, a yield fixture's unwrapping, and the
`conftest.py` chain. A name that resolves to no fixture stays gradual, as does one whose fixture has
no derivable type.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/pytest/__init__.pyi`:

```pyi
from _pytest.fixtures import fixture as fixture
```

`/.venv/<path-to-site-packages>/_pytest/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/_pytest/fixtures.pyi`:

```pyi
from typing import Any, Callable, overload

class FixtureFunctionDefinition: ...

class FixtureFunctionMarker:
    def __call__(self, function: Callable[..., Any]) -> FixtureFunctionDefinition: ...

@overload
def fixture(function: Callable[..., Any], *, scope: str = ..., name: str | None = None) -> FixtureFunctionDefinition: ...
@overload
def fixture(function: None = ..., *, scope: str = ..., name: str | None = None) -> FixtureFunctionMarker: ...
```

`conftest.py`:

```py
import pytest

@pytest.fixture
def from_conftest() -> bytes:
    return b""
```

`test_unannotated.py`:

```py
import pytest
from typing import Iterator

@pytest.fixture
def number() -> int:
    return 1

@pytest.fixture(name="renamed")
def _make_text() -> str:
    return "x"

@pytest.fixture
def yielded() -> Iterator[bool]:
    yield True

@pytest.fixture
def untyped():
    return object()

def test_it(number, renamed, yielded, from_conftest, untyped, missing) -> None:  # error: [unknown-fixture]
    reveal_type(number)  # revealed: int
    reveal_type(renamed)  # revealed: str
    reveal_type(yielded)  # revealed: bool
    reveal_type(from_conftest)  # revealed: bytes
    reveal_type(untyped)  # revealed: Unknown
    reveal_type(missing)  # revealed: Unknown
```

## A fixture's own parameters take fixture types

Fixtures request fixtures the same way tests do, so an unannotated parameter of a `@pytest.fixture`
function is typed from the registry too — including when the fixture it requests is defined later in
the module.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/pytest/__init__.pyi`:

```pyi
from _pytest.fixtures import fixture as fixture
```

`/.venv/<path-to-site-packages>/_pytest/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/_pytest/fixtures.pyi`:

```pyi
from typing import Any, Callable, overload

class FixtureFunctionDefinition: ...

class FixtureFunctionMarker:
    def __call__(self, function: Callable[..., Any]) -> FixtureFunctionDefinition: ...

@overload
def fixture(function: Callable[..., Any], *, scope: str = ..., name: str | None = None) -> FixtureFunctionDefinition: ...
@overload
def fixture(function: None = ..., *, scope: str = ..., name: str | None = None) -> FixtureFunctionMarker: ...
```

`test_chained.py`:

```py
import pytest

@pytest.fixture
def outer(inner) -> str:
    reveal_type(inner)  # revealed: int
    return str(inner)

@pytest.fixture
def inner() -> int:
    return 1

def test_it(outer) -> None:
    reveal_type(outer)  # revealed: str
```

## Parametrized names are arguments, not fixtures

`@pytest.mark.parametrize` supplies a name from its value rows rather than from the fixture
registry, so a parametrized parameter is not fixture-typed even when a fixture of that name exists.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/pytest/__init__.pyi`:

```pyi
from _pytest.fixtures import fixture as fixture
from _pytest.mark.structures import MarkGenerator as MarkGenerator

mark: MarkGenerator
```

`/.venv/<path-to-site-packages>/_pytest/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/_pytest/mark/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/_pytest/mark/structures.pyi`:

```pyi
from typing import Any

class MarkDecorator:
    def __call__(self, *args: Any, **kwargs: Any) -> Any: ...

class MarkGenerator:
    def __getattr__(self, name: str) -> MarkDecorator: ...
```

`/.venv/<path-to-site-packages>/_pytest/fixtures.pyi`:

```pyi
from typing import Any, Callable, overload

class FixtureFunctionDefinition: ...

class FixtureFunctionMarker:
    def __call__(self, function: Callable[..., Any]) -> FixtureFunctionDefinition: ...

@overload
def fixture(function: Callable[..., Any], *, scope: str = ..., name: str | None = None) -> FixtureFunctionDefinition: ...
@overload
def fixture(function: None = ..., *, scope: str = ..., name: str | None = None) -> FixtureFunctionMarker: ...
```

`test_parametrized.py`:

```py
import pytest

@pytest.fixture
def value() -> int:
    return 1

@pytest.mark.parametrize("value", ["a", "b"])
def test_it(value) -> None:
    # supplied by the marker, so the same-named fixture does not apply
    reveal_type(value)  # revealed: Unknown

@pytest.mark.parametrize("other", ["a", "b"])
def test_mixed(other, value) -> None:
    reveal_type(other)  # revealed: Unknown
    reveal_type(value)  # revealed: int
```

## Ordinary functions are untouched

A function pytest does not manage keeps the ordinary rules for an unannotated parameter, even when a
fixture of that name is in scope.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/pytest/__init__.pyi`:

```pyi
from _pytest.fixtures import fixture as fixture
```

`/.venv/<path-to-site-packages>/_pytest/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/_pytest/fixtures.pyi`:

```pyi
from typing import Any, Callable, overload

class FixtureFunctionDefinition: ...

class FixtureFunctionMarker:
    def __call__(self, function: Callable[..., Any]) -> FixtureFunctionDefinition: ...

@overload
def fixture(function: Callable[..., Any], *, scope: str = ..., name: str | None = None) -> FixtureFunctionDefinition: ...
@overload
def fixture(function: None = ..., *, scope: str = ..., name: str | None = None) -> FixtureFunctionMarker: ...
```

`test_ordinary.py`:

```py
import pytest

@pytest.fixture
def number() -> int:
    return 1

# a helper, not a test: pytest never calls it, so `number` is an ordinary parameter
def helper(number) -> None:
    reveal_type(number)  # revealed: Unknown

def test_it() -> None:
    helper("anything")
```

## a `.by` test module is collected too

A basedpython module transpiles to a `.py` of the same stem, so pytest collects it under exactly the
same naming conventions — and a `conftest.by` provides fixtures to it just as a `conftest.py` does.
Recognising only `.py` left every `.by` test file uncollected, so no parameter was injected and no
fixture mismatch was reported.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/pytest/__init__.pyi`:

```pyi
from _pytest.fixtures import fixture as fixture
```

`/.venv/<path-to-site-packages>/_pytest/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/_pytest/fixtures.pyi`:

```pyi
from typing import Any, Callable, overload

class FixtureFunctionDefinition: ...

class FixtureFunctionMarker:
    def __call__(self, function: Callable[..., Any]) -> FixtureFunctionDefinition: ...

@overload
def fixture(function: Callable[..., Any], *, scope: str = ..., name: str | None = None) -> FixtureFunctionDefinition: ...
@overload
def fixture(function: None = ..., *, scope: str = ..., name: str | None = None) -> FixtureFunctionMarker: ...
```

`conftest.by`:

```by
import pytest

@pytest.fixture
def shared() -> bytes:
    return b""
```

`test_based.by`:

```by
import pytest

@pytest.fixture
def number() -> int:
    return 3

@pytest.fixture
def label() -> str:
    return "x"

def test_injects(number, label, shared) -> None:
    reveal_type(number)  # revealed: int
    reveal_type(label)  # revealed: str
    reveal_type(shared)  # revealed: bytes

# error: [invalid-fixture-type] "Fixture `label` provides `str`, but the parameter is annotated `int`"
def test_mismatch(label: int) -> None:
    pass
```
