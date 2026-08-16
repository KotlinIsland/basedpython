"""basedpython — a python-like language that builds into python wheels.

The tools themselves are the `by` and `buff` executables this distribution
installs; there is no python api. What lives here is the build backend, so that
a basedpython project can name it in `[build-system]`:

    [build-system]
    requires = ["basedpython"]
    build-backend = "basedpython.build"
"""

from __future__ import annotations
