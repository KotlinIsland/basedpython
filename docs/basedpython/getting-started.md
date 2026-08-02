# getting started

## installation

```sh
uv add --dev basedpython
```

this installs the `by` CLI. verify it works:

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

`by run main` finds `main.by` in the current directory, transpiles it (and all other `.by` files in the project) to a temporary directory, then executes `python -m main` from there

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

## project layout

a typical basedpython project looks like:

```text
myproject/
├── main.by
├── utils.by
├── out/          # transpiled .py output — gitignore this
└── pyproject.toml
```

add the output directory to `.gitignore`:

```text
out/
```

## building

`by build` transpiles all `.by` files in the project and writes the output to `out/`, mirroring the source structure:

```sh
by build
```

```text
main.by -> out/main.py
utils.by -> out/utils.py

build complete (2 files)
```

the generated `.py` files are ordinary python. run them with any python tool:

```sh
python out/main.py
pytest out/
mypy out/
ruff check out/
```

type checkers and linters operate on the generated Python. if your editor shows type errors in `.by` files, point it at the corresponding `.py` output instead

## CI integration

```yaml
- name: Build
  run: |
    uv add --dev basedpython
    by build

- name: Test
  run: pytest out/
```

## low-level: single file transpilation

`by transpile` is the low-level command for single-file transforms. it reads a file (or stdin) and writes the transpiled Python to stdout:

```sh
by transpile hello.by
echo 'x[(a, b)]' | by transpile
# → x[(a, b),]
```

output always goes to stdout — redirect it to a file if you want to keep it
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
