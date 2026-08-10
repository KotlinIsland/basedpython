# `by` CLI reference

basedpython ships with two executables: `by` and `buff`. `by` is the basedpython driver — an extension of `ty` and includes the
type-checker, the transpiler, and a few project-level commands

`buff` is the basedpython version of `ruff`

```text
by <command> [args...]
```

in addition to the cli provided by `ty`, `by` includes:

| command             | what it does                                                       |
| ------------------- | ------------------------------------------------------------------ |
| `run`               | transpile and run a module with `python -m <module>`               |
| `build`             | transpile every `.by`/`.byi` file and write to `out/`              |
| `generate-api-file` | write a public-api lockfile (see [api-lock](features/api-lock.md)) |
| `transpile`         | transpile a single file to stdout (reads stdin if no file)         |

## `by run`

```sh
by run MODULE [ARGS...]             # transpile + run with `python -m MODULE`
by run                              # run the configured entry point
by run MODULE --min-version 3.12    # target a specific runtime python version
```

equivalent to `by build && python -m MODULE`, but only transpiles the
modules required to import `MODULE`

the module can be left out when the project configures an entry point:

```toml
[tool.basedpython.run]
main = "app.cli"
```

`by run` then runs `app.cli`. a module named on the command line always wins,
so `by run other` still runs `other`. the first positional argument is always
the module — to reach the entry point's own arguments, name it: `by run app.cli --name asdf`

everything after `MODULE` is forwarded to the program as `sys.argv[1:]`,
including options — `by run main --name asdf` passes `--name asdf` on. the one
exception is a leading `-h` / `--help`, which prints `by run`'s own help; write
`by run main -- --help` to reach the program's. when the program's entry point
is a [`main` function](features/main-function.md), those arguments are parsed
into its parameters

the project is type-checked first, and a program with check *errors* is not
run — the checker's verdict and the runtime must not diverge. warnings don't
block; a rule can be downgraded in configuration where its error is unwanted

the interpreter comes from `PYTHON` (default `python3`), and by default the
emitted code targets that interpreter's version. an explicit `--min-version`
wins, but must not exceed the interpreter — `by run` refuses rather than emit
code the interpreter cannot parse

hidden directories (`.claude`, `.git`, `.venv`, …) and build outputs are never
treated as project source: they are neither checked nor transpiled, by `run`
and `build` alike

## `by build`

```sh
by build                            # transpile every .by/.byi the project claims
by build --min-version 3.12         # target a specific runtime python version
```

`by build` walks the project's *own* file set — the one `by check` walks — so
`src.exclude` and the ignore files it honours apply here too, and the two halves
of the toolchain never disagree about which files are in the project. hidden
directories (`.claude`, `.git`, `.venv`, …) and build outputs are skipped

without `--min-version` the emit target is the project's configured python
version (`environment.python-version`, else the `requires-python` lower bound),
so the checker and the emitter agree about which python this project targets

a file that fails to parse fails the build, but only for itself: every other
module is still written. a code generator and a test runner are exactly what you
reach for when one file is mid-edit

writes the transpiled python to `./out/` mirroring the *module* tree. a
src-layout project's `src/package_name/main.by` is the module
`package_name.main`, so it lands at `out/package_name/main.py` — `out/` is a
directory you can put on `sys.path` as it stands, and `run.main` names a module
the same way an import does. the `out/` directory is **not** considered
first-party source for `by check` or `by generate-api-file` — it is regenerated
on every build

## `by generate-api-file`

```sh
by generate-api-file                       # writes ./api.lock
by generate-api-file --stdout              # writes lockfile to stdout
by generate-api-file -o public.lock        # custom output path
by generate-api-file --python-version 3.10 # target a specific python version
```

see [api-lock](features/api-lock.md) for the lockfile format and workflow

## `by transpile`

```sh
by transpile FILE           # read FILE, write transpiled python to stdout
by transpile                # read from stdin, write to stdout
by transpile FILE --reverse         # convert python source into basedpython idioms
by transpile FILE --min-version 3.12 # target a specific runtime python version
echo 'x: int = 1' | by transpile
```

`by transpile` also accepts a directory, transpiling it in place (every `.by` →
`.py`, or with `--reverse` every `.py` → `.by`)

stops at the first transpile error and prints a diagnostic
