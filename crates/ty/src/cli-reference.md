<!-- TODO(taz): put this to `args.rs` -->

# `by` CLI reference

the `by` command is the basedpython transpiler and runner. all commands accept
`--min-version` to control the minimum Python version the output must run on
(default `3.10`)

## `by run <module>`

transpile and run a module:

```sh
by run main
```

resolves `main.by` in the project root, transpiles it (and every other `.by`
file in the project) to a temporary directory, and runs `main` out of that tree
— in the directory you invoked `by` from, the way `python -m` does, so a
relative path the program is given and anything it reads or writes beside the
project mean what they say

```sh
by run main --min-version 3.12
```

### `--in-build`

run in the staged tree instead of in the current directory:

```sh
by run --in-build pytest -v tests/test_calc.py
```

for a program whose arguments are the project's own files. the project is
python only in the staged tree — `tests/test_calc.by` is `tests/test_calc.py`
there, beside every file the build carries over unchanged, `pyproject.toml`
included — so a path naming one of its modules resolves, and a tool reports
paths relative to a tree laid out like the project.

## `by build`

transpile every `.by` file in the project to `build/`, mirroring the module
layout — a src-layout project's source root is stripped, so `build/` is
importable as it stands:

```sh
by build
```

```text
main.by                     -> build/main.py
utils.by                    -> build/utils.py
src/package_name/main.by    -> build/package_name/main.py
```

generated `.py` files are ordinary Python — run them with any Python tool
(`python`, `pytest`, `mypy`, `ruff check`, etc.)

```sh
by build --min-version 3.10
```

without the flag the target is the project's configured python version, so the
emitted code matches what `by check` assumed.

## `by transpile`

low-level single-file transpilation. reads a file (or stdin) and writes the
transpiled output to stdout:

```sh
by transpile hello.by
echo 'a = b ?? 1' | by transpile
```

### `--reverse`

run the [reverse transforms](development/reverse-transforms.md) pipeline,
converting Python source into basedpython idioms:

```sh
by transpile --reverse legacy.py
```

useful when migrating an existing Python module — the output will use
basedpython surface syntax (`?.`, `===`, modifier keywords, etc.) wherever the
reverse transforms can recognize the underlying pattern

### `--min-version`

```sh
by transpile main.by --min-version 3.11
```

## `--min-version`

the minimum Python version the transpiled output must run on. polyfills are
inserted as needed when the target is below the version that introduced a
feature. all polyfills are no-ops when targeting a version that has the feature
natively. see [polyfills](features/polyfills.md) for the per-feature
breakdown

| value  | accepted forms |
| ------ | -------------- |
| `3.10` | default        |
| `3.11` |                |
| `3.12` |                |
| `3.13` |                |
| `3.14` |                |

## type checking

`.by` files are type-checked by ty using the same surface syntax. point your
editor at the `.by` source — ty understands the basedpython sugars and reports
errors with line/column information from the original file
