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
# error: [undeclared-dependency] "`charset_normalizer` comes from `charset_normalizer`, which this project does not depend on"
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
# error: [misplaced-dependency] "`pytest` comes from `pytest`, which is declared in dependency group `dev`"
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
# error: [undeclared-dependency] "`charset_normalizer` comes from `charset_normalizer`, which this project does not depend on"
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
