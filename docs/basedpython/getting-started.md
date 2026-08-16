# getting started

install `by`, write a `.by` file, and run it. everything below takes about five
minutes

## installation

basedpython ships as the `basedpython` package, which installs two executables:
`by`, the type checker and transpiler, and `buff`, the linter and formatter

=== "uv"

    ```sh
    uv add --dev basedpython
    ```

verify it works:

```sh
by --help
```

## your first file

basedpython source files use the `.by` extension. create `main.by`:

```by
message = "hello"
print(message)
```

run it directly:

```sh
by run main
```

`by run main` finds `main.by` in the current directory, transpiles it (and all
other `.by` files in the project) to a temporary directory, then executes
`python -m main` from there

!!! note "`by run` takes a module, not a path"

    the argument is what you would pass to `python -m`, so it is `main`, not
    `main.by` — and a nested entry point is `pkg.main`

naming the module every time gets old once a project has an entry point. configure one and `by run` alone is enough:

```toml
[tool.basedpython.run]
main = "main"
```

that goes in the `[tool.basedpython]` section of `pyproject.toml`, or in a
`basedpython.toml` beside it, which holds the same options at the top level:

```toml
[run]
main = "main"
```

see [configuration](configuration.md) for everything that can go in there

## building

`by build` writes the project to `out/` as python:

```sh
by build
```

```text
main.by -> out/main.py
utils.by -> out/utils.py

build complete (2 files)
```

that is the whole project, not only its `.by` files — a hand-written `.py`
module, a `py.typed`, a data file the program reads are all carried across to
the same place, so `out/` runs the way the source does

the generated `.py` files are ordinary python. run them with any python tool:

```sh
python out/main.py
pytest out/
mypy out/
ruff check out/
```

to ship the project rather than run it, build a wheel — see
[packaging](packaging.md):

```sh
uv build
```

## CI integration

```yaml
- name: Build
  run: |
    uv add --dev basedpython
    by build

- name: Test
  run: pytest out/
```

## converting python to basedpython

you don't have to start from an empty file. `by transpile --reverse` runs the
transpiler backwards, rewriting python source into the basedpython idiom it
would have lowered from:

```sh
by transpile --reverse legacy.py
```

```py
from typing import Callable, Optional


class Node:
    children: list["Node"]

    def __eq__(self, other: object) -> bool:
        if other is self:
            return True
        return isinstance(other, Node) and other.children == self.children

    def find(self, key: str) -> Optional["Node"]: ...


on_visit: Callable[[Node], None]
```

comes back as:

```by
from typing import Optional


class Node:
    children: list[Node]

    def __eq__(self, other: object) -> bool:
        if other === self:
            return True
        return other is Node and other.children == self.children

    def find(self, key: str) -> Optional[Node]


on_visit: (Node) -> None
```

the identity fast path became `===` and the `isinstance` became
[`is`](features/identity-swap.md), the quotes came off the self-references,
`Callable[[Node], None]` became an [arrow type](features/callable.md), the
`: ...` body became an [empty declaration](features/empty-declarations.md), and
the now-unused `Callable` import was pruned

point it at a directory to convert a whole tree in place, every `.py` to a
`.by`:

```sh
by transpile --reverse src/
```

each reverse transform mirrors a forward one, so a converted file transpiles
back to the program you started with

!!! warning "read the diff"

    reversing converts the constructs that have a reverse transform and leaves
    the rest alone — `__init__` does not become
    [`init(...)`](features/init-method.md), `Optional[T]` does not become `T?`.
    it is a head start, not a port. run `by check` on the result, and read
    [differences from python](features/differences-from-python.md) before you
    commit it

## low-level: single file transpilation

`by transpile` is the low-level command for single-file transforms. it reads a
file (or stdin) and writes the transpiled python to stdout:

```sh
by transpile hello.by
echo 'a = b ?? 1' | by transpile
# → a = b if b is not None else 1
```

output goes to stdout - redirect it to a file if you want to keep it
(`by transpile hello.by > hello.py`). use `by build` to transpile a whole
project into `out/`

## forward references

basedpython has no manual forward-reference syntax — a string in an
annotation is a string-literal type, not a deferred name. so when a class
refers to itself before its definition finishes, the transpiler quotes the
reference for you:

```by
class Node:
    def next(self) -> Node: ...   # → def next(self) -> "Node": ...
```

quoting only happens when it's needed. on python 3.14+ annotations are
evaluated lazily (PEP 649), and if you target an older runtime but want
every annotation deferred anyway you can opt into a blanket
`from __future__ import annotations` — in either case the reference is left
as-is

## next

<div class="grid cards" markdown>

- :lucide-book-open:{ .lg .middle } **[the feature reference](features/index.md)**

    ______________________________________________________________________

    every piece of syntax basedpython adds, one page at a time

- :lucide-package:{ .lg .middle } **[framework support](frameworks/index.md)**

    ______________________________________________________________________

    what changes when pydantic, sqlalchemy, pytest or django is in the project

- :lucide-terminal:{ .lg .middle } **[`by` CLI reference](cli-reference.md)**

    ______________________________________________________________________

    every command and flag, including the ones inherited from `ty`

- :lucide-arrow-right-left:{ .lg .middle } **[how transpilation works](development/how-transpilation-works.md)**

    ______________________________________________________________________

    what happens between the `.by` file and the python that runs

</div>
