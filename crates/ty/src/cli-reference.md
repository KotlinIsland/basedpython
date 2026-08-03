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
file in the project) to a temporary directory, then executes
`python -m main` from there

```sh
by run main --min-version 3.12
```

## `by build`

transpile every `.by` file in the project to `out/`, mirroring the source
layout:

```sh
by build
```

```text
main.by   -> out/main.py
utils.by  -> out/utils.py
```

generated `.py` files are ordinary Python — run them with any Python tool
(`python`, `pytest`, `mypy`, `ruff check`, etc.)

```sh
by build --min-version 3.10
```

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
