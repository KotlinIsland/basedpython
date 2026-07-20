# pytest support

basedpython understands pytest's fixture system. fixture parameters check correctly against their types, and tests using basedpython features work at runtime.

## what works

### fixtures and dependency injection

- fixture parameters resolve correctly by name
- fixture type mismatches are caught: if a test requests `db: Database` but the fixture provides `Connection`, you'll get a type error
- builtin fixtures: `tmp_path`, `monkeypatch`, `capsys`, `tmp_path_factory`, and others from pytest's stubs all have their correct types
- yield fixtures: `Iterator[T]` and `Generator[T, ...]` unwrap to `T`
- async fixtures: `AsyncIterator[T]` unwraps to `T`
- fixture shadowing: fixtures in the current module take precedence over conftest
- fixture name overrides: `@pytest.fixture(name="custom_name")`

### test parametrization

- `@pytest.mark.parametrize` checks arity and can validate literal parameter values
- basedpython enum variants work as parametrize values

### transpilation compatibility

- fixture functions written in `.by` work correctly: decorators survive, parameter names survive, type injection works at runtime
- `yield` fixtures transpose correctly
- `?:` and other basedpython features work inside test bodies and fixtures

## limitations and workarounds

### plugin-provided fixtures

third-party pytest plugins that inject fixtures via entry points aren't discovered. builtin fixtures work, but plugin fixtures like `django_db` or `flask_app` won't be recognized by the type checker.

**workaround:** annotate the parameter explicitly:

```by
# from pytest-django, not recognized by the checker
def test_user(db: DjangoDatabase):
    user = User.objects.create(name="Alice")
    assert user.id is not None
```

### dynamic parametrize arguments

when parametrize uses dynamic values (not literal strings or values), the checker can't validate them. static arity and value checks only apply to literal arguments.

## required setup

pytest has inline type stubs, so there's no additional setup. just install pytest.

## examples

basic fixture:

```by
import pytest
from pathlib import Path

@pytest.fixture
def temp_file(tmp_path: Path) -> Path:
    file = tmp_path / "test.txt"
    file.write_text("hello")
    return file

def test_read_file(temp_file: Path) -> None:
    content = temp_file.read_text()
    assert content == "hello"
```

fixture with setup and teardown:

```by
@pytest.fixture
def database() -> Iterator[Database]:
    db = Database(":memory:")
    db.connect()
    yield db
    db.close()

def test_query(database: Database) -> None:
    result = database.query("SELECT 1")
    assert result is not None
```

parametrization:

```by
@pytest.mark.parametrize("x,y,expected", [
    (2, 3, 5),
    (0, 0, 0),
    (-1, 1, 0),
])
def test_add(x: int, y: int, expected: int) -> None:
    assert x + y == expected
```

conftest with multiple fixtures:

```by
# conftest.py
import pytest

@pytest.fixture
def api_client() -> ApiClient:
    return ApiClient(base_url="http://localhost:8000")

@pytest.fixture
def authenticated_client(api_client: ApiClient) -> ApiClient:
    api_client.authenticate("token")
    return api_client

# test_api.py
def test_list_users(authenticated_client: ApiClient) -> None:
    users = authenticated_client.get("/users")
    assert len(users) > 0
```

## collection conventions

basedpython recognizes the default pytest conventions:

- test files: `test_*.py` or `*_test.py`
- test functions: `test_*` at module scope

custom collection configs in `pytest.ini` or `pyproject.toml` aren't read yet, so stick to the defaults for full checking.

## running tests

`.by` test files transpile to `.py`, so run tests on the transpiled output:

```sh
by build
pytest out/
```

## see also

- [pytest documentation](https://docs.pytest.org/)
- framework compatibility matrix in the [frameworks overview](index.md#basedpython-features-and-framework-compatibility)
