# pytest (mock stubs)

Hermetic pins for the *mechanism* of ty's dedicated pytest support, using minimal hand-written stubs
that mirror the shapes pytest ships inline: the `@pytest.fixture` decorator (defined in
`_pytest.fixtures`). The `external/pytest.md` suite checks the same behaviours — plus builtin
fixtures and `parametrize` — against the real package.

pytest fills a test or fixture parameter by *name* from a registry: the function's own module, then
the `conftest.py` chain, then builtin fixtures. A parameter annotation that disagrees with the
resolved fixture's type is an error; the fixture's provided type is its return annotation, with a
yield fixture's `Iterator[T]` / `Generator[T, ...]` unwrapped to `T`.

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
