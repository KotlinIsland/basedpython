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
| `compile`           | compile `.by` files to native CPython extension modules            |
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

the interpreter is the project's own: the environment `by check` resolves
imports against — the `environment.python` the project configures, else an
activated virtual environment, a conda environment, or a `.venv` beside the
project's `pyproject.toml`. `--python` overrides it for one run, and `PYTHON`
stands in only where the project has no environment of its own, above the bare
`python3` discovery falls back to. a third-party package lives in that
environment's `site-packages` and nowhere else, so running anywhere else fails
on an import the checker had no complaint about. all of that resolves against
the project root rather than the working directory, so `by run` in a
subdirectory is still this project

the emitted code targets that interpreter's version by default. an explicit
`--min-version` wins, but must not exceed the interpreter — `by run` refuses
rather than emit code the interpreter cannot parse. an interpreter *older* than
the version the project declares is refused too: the source may use syntax that
python has no lowering for, and the failure would land as a `SyntaxError` inside
generated code. passing `--min-version` for the interpreter's own version builds
for it instead

the program runs in the directory `by run` was invoked from, the way `python -m`
does. the transpiled tree lives in a temporary directory, which python puts at
the head of `sys.path` — so a relative path on the command line, and anything
the program reads or writes beside the project, resolve where they were written
to

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

artifacts and exit status answer different questions. everything that could be
emitted is emitted, sourcemap and package markers included; the exit status says
whether anything was *reported*. so a build that prints an error exits 1 while
still leaving a usable `out/`, and `by build && pytest out/tests` runs the tests
only against a tree the checker had nothing to say about

writes the transpiled python to `./out/` mirroring the *module* tree. a
src-layout project's `src/package_name/main.by` is the module
`package_name.main`, so it lands at `out/package_name/main.py` — `out/` is a
directory you can put on `sys.path` as it stands, and `run.main` names a module
the same way an import does. the `out/` directory is **not** considered
first-party source for `by check` or `by generate-api-file` — it is regenerated
on every build

alongside the python it writes `out/_by_sourcemap.py`, mapping each generated
line back to the `.by` line it came from, with a digest of both files so a tool
reading it can tell whether it still describes what is on disk — see
[sourcemaps](development/sourcemaps.md)

## `by compile`

> **status: early.** integer, float and bool arithmetic, control flow, and calls
> within a module compile natively. everything else falls back to the interpreted
> definition — see [native compilation](development/compilation/index.md)

```sh
by compile                      # every .by file under the project root → out/
by compile hot.by               # one file
by compile -o build hot.by      # a different output directory
by compile --verbose            # report every function left interpreted, and why
by compile --emit-c-only        # write the generated C without compiling it
by compile --no-any             # refuse to leave a gradual-typed function interpreted
by compile --require-native     # refuse to leave *any* function interpreted
```

the output directory mirrors the *module* tree, the way `by build`'s does: the
package member `pkg/sub/dup.py` lands at `out/pkg/sub/dup.cpython-313-darwin.so`,
and the package `pkg/sub/__init__.py` at
`out/pkg/sub/__init__.cpython-313-darwin.so` — so `out/` can go on `sys.path` as
it stands and every module imports under the dotted name it was compiled as

`--no-any` buys no speed on its own — it is a **predictability contract**. a
gradual type is the commonest reason a function silently stays interpreted, and a
decline is invisible unless you look for it, so a module that means to be fully
compiled can say so and be held to it:

```console
$ by compile app.by --no-any
error: could not compile app.by

Caused by:
    `no-any` is on and 1 function(s) could not be compiled because a type was gradual:
      loose: a gradual type has no known representation
```

`--require-native` asks a different, stricter question. `--no-any` asks *is this
module fully typed*; `--require-native` asks *does this module compile
entirely*, and so also fails on a type that is perfectly precise but that the
compiler does not represent yet:

```console
$ by compile app.by --require-native
error: could not compile app.by

Caused by:
    `require-native` is on and 1 function(s) were left interpreted:
      describe: `list[int]` has no native representation yet
```

a function the compiler cannot lower natively is **not** an error: the module's
transpiled python is embedded in the extension and executed at import, so
declined functions still exist and module-level code still runs. the natively
compiled functions are installed over the top

```console
$ by compile hot.by --verbose
hot.by -> out/hot.cpython-313-darwin.so
  declined describe: `list[int]` has no native representation yet

compiled 1 module(s)
1 function(s) left to the interpreted definition
```

the C toolchain and the cpython headers come from the same interpreter `by run`
uses, chosen the same way, and it must have development headers available. it
has to be the same one: an extension built against one abi is unimportable by
another, and `by run --compiled` imports what this wrote

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

a python source that declares its own encoding with a
[PEP 263](https://peps.python.org/pep-0263/) comment is decoded as it is read.
what is written back out is utf-8, so the declaration is rewritten to say `utf-8`
— left alone it would name an encoding the file no longer has. utf-8 and the
latin-1 family are decodable; a file declaring anything else is skipped and named
rather than guessed at
