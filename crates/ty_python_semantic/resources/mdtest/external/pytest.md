# pytest

```toml
[environment]
python-version = "3.13"
python-platform = "linux"

[project]
dependencies = ["pytest==9.0.1"]
```

## `pytest.fail`

Make sure that we recognize `pytest.fail` calls as terminal:

```py
import pytest

def some_runtime_condition() -> bool:
    return True

def test_something():
    if not some_runtime_condition():
        pytest.fail("Runtime condition failed")

        no_error_here_this_is_unreachable
```

## Builtin fixture types

A builtin fixture resolves through the real `_pytest` modules: `tmp_path` provides `pathlib.Path`,
so a parameter annotated otherwise is an error.

`test_builtins.py`:

```py
from pathlib import Path

def test_ok(tmp_path: Path) -> None:
    reveal_type(tmp_path)  # revealed: Path

def test_bad(tmp_path: int) -> None:  # error: [invalid-fixture-type]
    ...
```

## Fixture type mismatch across a conftest

A fixture defined in `conftest.py` resolves for a test in a sibling file; a parameter annotation the
fixture's type is not assignable to is an error.

`conftest.py`:

```py
import pytest

@pytest.fixture
def db() -> str:
    return "sqlite://"
```

`test_db.py`:

```py
def test_ok(db: str) -> None:
    reveal_type(db)  # revealed: str

def test_bad(db: int) -> None:  # error: [invalid-fixture-type]
    ...
```

## parametrize names and arity

`@pytest.mark.parametrize` names are checked against the function's parameters, and each value row's
length against the number of names.

`test_param.py`:

```py
import pytest

@pytest.mark.parametrize("a, b", [(1, 2), (3,)])  # error: [invalid-parametrize]
def test_add(a: int, b: int) -> None: ...
@pytest.mark.parametrize("a, missing", [(1, 2)])  # error: [invalid-parametrize]
def test_missing(a: int) -> None: ...
```
