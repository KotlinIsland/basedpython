# pytest conformance

```toml
[environment]
python-version = "3.13"
python-platform = "linux"

[project]
dependencies = ["pytest==9.0.1"]
```

pytest's contract with transpiled output is *introspection*: it collects `test_*` functions and
fills their parameters by name from fixtures, matching signatures and unwrapping decorators. every
basedpython lowering must leave that introspection intact. each block below drives **real pytest**
over the transpiled output — it re-invokes `pytest.main` on itself under `__main__`, so a non-zero
exit (a test that failed to collect, a fixture that failed to inject) fails the divergence harness.

These blocks are skipped unless pytest is installed; run them locally with
`PYTHON=/path/to/venv/bin/python` to enforce the contract.

## a yield fixture returning a based enum injects

A `@pytest.fixture` written in `.by` — here yielding a based-enum value — collects and injects into
a test that requests it by name.

```by
import pytest
from typing import Iterator

enum class Color:
    case Red
    case Green

@pytest.fixture
def color() -> Iterator[Color]:
    chosen: Color = Color.Green
    yield chosen

def test_color_injected(color: Color) -> None:
    assert color == Color.Green

if __name__ == "__main__":
    import sys
    sys.exit(pytest.main([__file__, "-q"]))
```

## parametrize over based-enum variants

`@pytest.mark.parametrize` iterates based-enum variants — payload-less variants lower to real enum
members, so pytest receives concrete values.

```by
import pytest

enum class Color:
    case Red
    case Green
    case Blue

@pytest.mark.parametrize("c", [Color.Red, Color.Green, Color.Blue])
def test_variant(c: Color) -> None:
    assert c in (Color.Red, Color.Green, Color.Blue)

if __name__ == "__main__":
    import sys
    sys.exit(pytest.main([__file__, "-q"]))
```

## parameter names survive soundness lowering

Soundness guards insert statements into a test body but never touch its signature, so pytest still
matches the fixture parameter by name.

```by
import pytest

@pytest.fixture
def number() -> int:
    return 21

def test_number_injected(number: int) -> None:
    doubled: int = number * 2
    assert doubled == 42

if __name__ == "__main__":
    import sys
    sys.exit(pytest.main([__file__, "-q"]))
```
