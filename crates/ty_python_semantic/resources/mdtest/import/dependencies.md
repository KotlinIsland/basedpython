# Declared dependencies

An import of installed third-party code is checked against what the project says it depends on.

The `[dependencies]` section of these tests stands in for a `pyproject.toml`, which mdtest has no
way to hand to the type checker.

## A transitive import is undeclared

`charset_normalizer` is installed because `requests` needs it, not because this project asked for
it.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = ["requests"]
```

`/.venv/<path-to-site-packages>/charset_normalizer/__init__.py`:

```py
def detect(): ...
```

`/.venv/<path-to-site-packages>/charset_normalizer-3.4.4.dist-info/RECORD`:

```text
charset_normalizer/__init__.py,sha256=a,1
```

`main.py`:

```py
# error: [undeclared-dependency] "This project does not directly depend on `charset_normalizer`"
import charset_normalizer
```

## A declared import is fine

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = ["requests"]
```

`/.venv/<path-to-site-packages>/requests/__init__.py`:

```py
def get(): ...
```

`/.venv/<path-to-site-packages>/requests-2.32.5.dist-info/RECORD`:

```text
requests/__init__.py,sha256=a,1
```

`main.py`:

```py
import requests
from requests import get
```

## The declared name does not have to be spelled like the module

`PyJWT` installs `jwt`.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = ["pyjwt"]
```

`/.venv/<path-to-site-packages>/jwt/__init__.py`:

```py
def encode(): ...
```

`/.venv/<path-to-site-packages>/PyJWT-2.10.1.dist-info/RECORD`:

```text
jwt/__init__.py,sha256=a,1
```

`main.py`:

```py
import jwt
```

## The dependency that pulled it in is named

`METADATA` records what each installed distribution requires, which is what says whose dependency
this is. `numpy` is installed here only because `pandas` asked for it.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = ["pandas"]
```

`/.venv/<path-to-site-packages>/pandas/__init__.py`:

```py
def read_csv(): ...
```

`/.venv/<path-to-site-packages>/pandas-2.3.3.dist-info/RECORD`:

```text
pandas/__init__.py,sha256=a,1
```

`/.venv/<path-to-site-packages>/pandas-2.3.3.dist-info/METADATA`:

```text
Name: pandas
Requires-Dist: numpy>=1.26.0
```

`/.venv/<path-to-site-packages>/numpy/__init__.py`:

```py
def array(): ...
```

`/.venv/<path-to-site-packages>/numpy-2.3.4.dist-info/RECORD`:

```text
numpy/__init__.py,sha256=a,1
```

`main.py`:

```py
# snapshot: undeclared-dependency
import numpy
```

```snapshot
warning[undeclared-dependency]: This project does not directly depend on `numpy`
 --> src/main.py:2:8
  |
2 | import numpy
  |        ^^^^^
info: It is installed because `pandas` requires it
info: Add `numpy` to the project's dependencies
```

## A dependency reached through another is traced back to what the project declared

The project declares `fastapi`, which requires `starlette`, which requires `anyio`. Naming `fastapi`
is what makes the install explicable; naming `starlette` is what makes it findable.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = ["fastapi"]
```

`/.venv/<path-to-site-packages>/fastapi/__init__.py`:

```py
def app(): ...
```

`/.venv/<path-to-site-packages>/fastapi-0.121.2.dist-info/RECORD`:

```text
fastapi/__init__.py,sha256=a,1
```

`/.venv/<path-to-site-packages>/fastapi-0.121.2.dist-info/METADATA`:

```text
Name: fastapi
Requires-Dist: starlette>=0.40.0
```

`/.venv/<path-to-site-packages>/starlette/__init__.py`:

```py
def route(): ...
```

`/.venv/<path-to-site-packages>/starlette-0.42.0.dist-info/RECORD`:

```text
starlette/__init__.py,sha256=a,1
```

`/.venv/<path-to-site-packages>/starlette-0.42.0.dist-info/METADATA`:

```text
Name: starlette
Requires-Dist: anyio>=3.6.2
```

`/.venv/<path-to-site-packages>/anyio/__init__.py`:

```py
def run(): ...
```

`/.venv/<path-to-site-packages>/anyio-4.11.0.dist-info/RECORD`:

```text
anyio/__init__.py,sha256=a,1
```

`main.py`:

```py
# snapshot: undeclared-dependency
import anyio
```

```snapshot
warning[undeclared-dependency]: This project does not directly depend on `anyio`
 --> src/main.py:2:8
  |
2 | import anyio
  |        ^^^^^
info: It is installed because `fastapi` requires it, through `starlette`
info: Add `anyio` to the project's dependencies
```

## A requirement that only an extra brings in explains nothing

`METADATA` records what an extra would require, not whether that extra was asked for. Nothing here
says the install of `pytest` is `pandas`'s doing, so nothing claims it is.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = ["pandas"]
```

`/.venv/<path-to-site-packages>/pandas/__init__.py`:

```py
def read_csv(): ...
```

`/.venv/<path-to-site-packages>/pandas-2.3.3.dist-info/RECORD`:

```text
pandas/__init__.py,sha256=a,1
```

`/.venv/<path-to-site-packages>/pandas-2.3.3.dist-info/METADATA`:

```text
Name: pandas
Requires-Dist: pytest>=7.3.2; extra == "test"
```

`/.venv/<path-to-site-packages>/pytest/__init__.py`:

```py
def fixture(): ...
```

`/.venv/<path-to-site-packages>/pytest-8.4.2.dist-info/RECORD`:

```text
pytest/__init__.py,sha256=a,1
```

`main.py`:

```py
# snapshot: undeclared-dependency
import pytest
```

```snapshot
warning[undeclared-dependency]: This project does not directly depend on `pytest`
 --> src/main.py:2:8
  |
2 | import pytest
  |        ^^^^^^
info: It is only installed because something else requires it
info: Add `pytest` to the project's dependencies
```

## A distribution the module is not named after is named outright

`PyJWT` installs `jwt`, so which distribution to declare is not something the import says.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = []
```

`/.venv/<path-to-site-packages>/jwt/__init__.py`:

```py
def encode(): ...
```

`/.venv/<path-to-site-packages>/PyJWT-2.10.1.dist-info/RECORD`:

```text
jwt/__init__.py,sha256=a,1
```

`main.py`:

```py
# error: [undeclared-dependency] "`jwt` comes from `PyJWT`, which this project does not directly depend on"
import jwt
```

## A dependency can hand out what it depends on

A library's interface can be partly made of another distribution's types — `pandas` hands out numpy
arrays — and it says so in the `by.typed` its package ships. A project that depends on `pandas` may
then import `numpy` without claiming it chose numpy itself.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = ["pandas"]
```

`/.venv/<path-to-site-packages>/pandas/__init__.py`:

```py
def read_csv(): ...
```

`/.venv/<path-to-site-packages>/pandas/by.typed`:

```text
exported-dependencies = ["numpy"]
```

`/.venv/<path-to-site-packages>/pandas-2.3.3.dist-info/RECORD`:

```text
pandas/__init__.py,sha256=a,1
pandas/by.typed,sha256=b,2
```

`/.venv/<path-to-site-packages>/pandas-2.3.3.dist-info/METADATA`:

```text
Name: pandas
Requires-Dist: numpy>=1.26.0
```

`/.venv/<path-to-site-packages>/numpy/__init__.py`:

```py
def array(): ...
```

`/.venv/<path-to-site-packages>/numpy-2.3.4.dist-info/RECORD`:

```text
numpy/__init__.py,sha256=a,1
```

`main.py`:

```py
import numpy
```

## An export is only what the distribution itself depends on

A marker naming something its own distribution does not require declares nothing. `pandas` cannot
make `requests` importable by saying so: an interface is made of what it was built with.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = ["pandas"]
```

`/.venv/<path-to-site-packages>/pandas/__init__.py`:

```py
def read_csv(): ...
```

`/.venv/<path-to-site-packages>/pandas/by.typed`:

```text
exported-dependencies = ["requests"]
```

`/.venv/<path-to-site-packages>/pandas-2.3.3.dist-info/RECORD`:

```text
pandas/__init__.py,sha256=a,1
pandas/by.typed,sha256=b,2
```

`/.venv/<path-to-site-packages>/pandas-2.3.3.dist-info/METADATA`:

```text
Name: pandas
```

`/.venv/<path-to-site-packages>/requests/__init__.py`:

```py
def get(): ...
```

`/.venv/<path-to-site-packages>/requests-2.32.5.dist-info/RECORD`:

```text
requests/__init__.py,sha256=a,1
```

`main.py`:

```py
# error: [undeclared-dependency] "This project does not directly depend on `requests`"
import requests
```

## An export travels only as far as every link declares it

`fastapi` exports `starlette`, so a project that depends on `fastapi` may import it. Whether
`starlette`'s own dependencies come with it is `starlette`'s to say, and here it says nothing: the
project reaches `starlette` but not `anyio`.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = ["fastapi"]
```

`/.venv/<path-to-site-packages>/fastapi/__init__.py`:

```py
def app(): ...
```

`/.venv/<path-to-site-packages>/fastapi/by.typed`:

```text
exported-dependencies = ["starlette"]
```

`/.venv/<path-to-site-packages>/fastapi-0.121.2.dist-info/RECORD`:

```text
fastapi/__init__.py,sha256=a,1
fastapi/by.typed,sha256=b,2
```

`/.venv/<path-to-site-packages>/fastapi-0.121.2.dist-info/METADATA`:

```text
Name: fastapi
Requires-Dist: starlette>=0.40.0
```

`/.venv/<path-to-site-packages>/starlette/__init__.py`:

```py
def route(): ...
```

`/.venv/<path-to-site-packages>/starlette-0.42.0.dist-info/RECORD`:

```text
starlette/__init__.py,sha256=a,1
```

`/.venv/<path-to-site-packages>/starlette-0.42.0.dist-info/METADATA`:

```text
Name: starlette
Requires-Dist: anyio>=3.6.2
```

`/.venv/<path-to-site-packages>/anyio/__init__.py`:

```py
def run(): ...
```

`/.venv/<path-to-site-packages>/anyio-4.11.0.dist-info/RECORD`:

```text
anyio/__init__.py,sha256=a,1
```

`main.py`:

```py
import starlette

# error: [undeclared-dependency] "This project does not directly depend on `anyio`"
import anyio
```

## A chain of exports carries all the way along it

The same project, with `starlette` exporting `anyio` in turn. Every link now says that what it hands
out is part of what it is, so `anyio` reaches the project through both.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = ["fastapi"]
```

`/.venv/<path-to-site-packages>/fastapi/__init__.py`:

```py
def app(): ...
```

`/.venv/<path-to-site-packages>/fastapi/by.typed`:

```text
exported-dependencies = ["starlette"]
```

`/.venv/<path-to-site-packages>/fastapi-0.121.2.dist-info/RECORD`:

```text
fastapi/__init__.py,sha256=a,1
fastapi/by.typed,sha256=b,2
```

`/.venv/<path-to-site-packages>/fastapi-0.121.2.dist-info/METADATA`:

```text
Name: fastapi
Requires-Dist: starlette>=0.40.0
```

`/.venv/<path-to-site-packages>/starlette/__init__.py`:

```py
def route(): ...
```

`/.venv/<path-to-site-packages>/starlette/by.typed`:

```text
exported-dependencies = ["anyio"]
```

`/.venv/<path-to-site-packages>/starlette-0.42.0.dist-info/RECORD`:

```text
starlette/__init__.py,sha256=a,1
starlette/by.typed,sha256=b,2
```

`/.venv/<path-to-site-packages>/starlette-0.42.0.dist-info/METADATA`:

```text
Name: starlette
Requires-Dist: anyio>=3.6.2
```

`/.venv/<path-to-site-packages>/anyio/__init__.py`:

```py
def run(): ...
```

`/.venv/<path-to-site-packages>/anyio-4.11.0.dist-info/RECORD`:

```text
anyio/__init__.py,sha256=a,1
```

`main.py`:

```py
import anyio
```

## Shipped code cannot reach an export of a dependency group

An export is only as available as the dependency that makes it. `pytest` is for working on the
project, so what it hands out is not the project's to ship either.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = []
groups = { dev = ["pytest"] }
```

`/.venv/<path-to-site-packages>/pytest/__init__.py`:

```py
def fixture(): ...
```

`/.venv/<path-to-site-packages>/pytest/by.typed`:

```text
exported-dependencies = ["pluggy"]
```

`/.venv/<path-to-site-packages>/pytest-8.4.2.dist-info/RECORD`:

```text
pytest/__init__.py,sha256=a,1
pytest/by.typed,sha256=b,2
```

`/.venv/<path-to-site-packages>/pytest-8.4.2.dist-info/METADATA`:

```text
Name: pytest
Requires-Dist: pluggy>=1.5
```

`/.venv/<path-to-site-packages>/pluggy/__init__.py`:

```py
def hookimpl(): ...
```

`/.venv/<path-to-site-packages>/pluggy-1.6.0.dist-info/RECORD`:

```text
pluggy/__init__.py,sha256=a,1
```

`my_lib/__init__.py`:

```py
# error: [undeclared-dependency] "This project does not directly depend on `pluggy`"
import pluggy
```

`tests/test_lib.py`:

```py
import pluggy
```

## Nothing is reported without a manifest

The same undeclared import, with the project saying nothing about its dependencies.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/charset_normalizer/__init__.py`:

```py
def detect(): ...
```

`/.venv/<path-to-site-packages>/charset_normalizer-3.4.4.dist-info/RECORD`:

```text
charset_normalizer/__init__.py,sha256=a,1
```

`main.py`:

```py
import charset_normalizer
```

## Nothing is reported without install metadata

An environment ty cannot attribute to distributions cannot answer the question at all.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = ["requests"]
```

`/.venv/<path-to-site-packages>/charset_normalizer/__init__.py`:

```py
def detect(): ...
```

`main.py`:

```py
import charset_normalizer
```

## Shipped code cannot import a dependency group

`my_lib` is what the project named `my-lib` ships.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = ["requests"]
groups = { dev = ["pytest"] }
```

`/.venv/<path-to-site-packages>/pytest/__init__.py`:

```py
def fixture(): ...
```

`/.venv/<path-to-site-packages>/pytest-8.4.2.dist-info/RECORD`:

```text
pytest/__init__.py,sha256=a,1
```

`my_lib/helpers.py`:

```py
# error: [misplaced-dependency] "`pytest` is declared in dependency group `dev`"
import pytest
```

## Code that is not shipped can import a dependency group

The same import, from a file outside what the project ships.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = ["requests"]
groups = { dev = ["pytest"] }
```

`/.venv/<path-to-site-packages>/pytest/__init__.py`:

```py
def fixture(): ...
```

`/.venv/<path-to-site-packages>/pytest-8.4.2.dist-info/RECORD`:

```text
pytest/__init__.py,sha256=a,1
```

`tests/test_helpers.py`:

```py
import pytest
```

## An extra may be imported from shipped code

An extra is installed for anyone who asks for it, so guarding the import is the project's business
rather than this check's.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
extras = { cli = ["click"] }
```

`/.venv/<path-to-site-packages>/click/__init__.py`:

```py
def command(): ...
```

`/.venv/<path-to-site-packages>/click-8.1.7.dist-info/RECORD`:

```text
click/__init__.py,sha256=a,1
```

`my_lib/cli.py`:

```py
import click
```

## A project that does not name itself ships nothing

Without a name there is no way to tell which files are shipped, so every group is available.

```toml
[environment]
python = "/.venv"

[dependencies]
groups = { dev = ["pytest"] }
```

`/.venv/<path-to-site-packages>/pytest/__init__.py`:

```py
def fixture(): ...
```

`/.venv/<path-to-site-packages>/pytest-8.4.2.dist-info/RECORD`:

```text
pytest/__init__.py,sha256=a,1
```

`my_lib/helpers.py`:

```py
import pytest
```

## The groups a file may use can be set outright

```toml
[environment]
python = "/.venv"

[analysis]
dependency-groups = ["project"]

[dependencies]
project = ["requests"]
groups = { dev = ["pytest"] }
```

`/.venv/<path-to-site-packages>/pytest/__init__.py`:

```py
def fixture(): ...
```

`/.venv/<path-to-site-packages>/pytest-8.4.2.dist-info/RECORD`:

```text
pytest/__init__.py,sha256=a,1
```

`tests/test_helpers.py`:

```py
# error: [misplaced-dependency]
import pytest
```

## The modules a project ships can be stated

```toml
[environment]
python = "/.venv"

[analysis]
shipped-modules = ["plugins"]

[dependencies]
name = "my-lib"
groups = { dev = ["pytest"] }
```

`/.venv/<path-to-site-packages>/pytest/__init__.py`:

```py
def fixture(): ...
```

`/.venv/<path-to-site-packages>/pytest-8.4.2.dist-info/RECORD`:

```text
pytest/__init__.py,sha256=a,1
```

`plugins/hook.py`:

```py
# error: [misplaced-dependency]
import pytest
```

## The standard library and first-party code are never reported

```toml
[dependencies]
name = "my-lib"
project = []
```

`my_lib/helper.py`:

```py
def help(): ...
```

`my_lib/main.py`:

```py
import os
import sys
from my_lib import helper
```

## A submodule is reported against its root distribution

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = []
```

`/.venv/<path-to-site-packages>/charset_normalizer/__init__.py`:

```py
def detect(): ...
```

`/.venv/<path-to-site-packages>/charset_normalizer/api.py`:

```py
def from_bytes(): ...
```

`/.venv/<path-to-site-packages>/charset_normalizer-3.4.4.dist-info/RECORD`:

```text
charset_normalizer/__init__.py,sha256=a,1
charset_normalizer/api.py,sha256=b,2
```

`main.py`:

```py
# error: [undeclared-dependency] "This project does not directly depend on `charset_normalizer`"
from charset_normalizer.api import from_bytes
```

## A namespace package is available when any owner is

`google` is installed by both, and only one of them is declared.

```toml
[environment]
python = "/.venv"

[dependencies]
name = "my-lib"
project = ["protobuf"]
```

`/.venv/<path-to-site-packages>/google/protobuf/__init__.py`:

```py
def message(): ...
```

`/.venv/<path-to-site-packages>/google/cloud/__init__.py`:

```py
def client(): ...
```

`/.venv/<path-to-site-packages>/protobuf-6.33.1.dist-info/RECORD`:

```text
google/protobuf/__init__.py,sha256=a,1
```

`/.venv/<path-to-site-packages>/google_cloud_core-2.4.1.dist-info/RECORD`:

```text
google/cloud/__init__.py,sha256=b,2
```

`main.py`:

```py
from google.protobuf import message
from google.cloud import client
```
