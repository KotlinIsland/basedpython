# native compilation

`by compile` builds your modules into native CPython extension modules, which import and answer
like the python they replace

given `hot.by`:

```by
def total(n: int) -> int:
    result = 0
    for i in range(n):
        result += i * i
    return result
```

compile it and import it:

```console
$ by compile hot.by
/app/hot.by -> /app/build/hot.cpython-313-darwin.so

compiled 1 module(s)
$ PYTHONPATH=build python -c "import hot; print(hot.__file__, hot.total(10))"
/app/build/hot.cpython-313-darwin.so 285
```

nothing about the source changes, and a function the compiler cannot lower still runs, so
compiling is a question of speed rather than of what the program does. how much speed depends on
the code: the [benchmark suite](../development/compilation/benchmarks.md) measures it

## what it writes

`by compile` writes everything [`by build`](../cli-reference.md#by-build) writes, and the extensions
as well. `build/`, in the project root, holds the whole project as importable python, with an
extension beside each module that was compiled. in a project that also has a `main.by`,
`by compile hot.by` leaves:

```text
build/
├── .by-manifest
├── _by_build.json
├── _by_sourcemap.py
├── by.h
├── hot.by
├── hot.c
├── hot.cpython-313-darwin.so
├── hot.py
├── main.by
├── main.py
└── pyproject.toml
```

the tree mirrors the module tree, so `pkg/sub/dup.by` lands at
`build/pkg/sub/dup.cpython-313-darwin.so`, and `build/` can go on `sys.path` as it stands. python's
finder prefers an extension to source, which is what makes `hot` native and `main` interpreted
here. `-o DIR` writes somewhere else

`build/` is a mirror of the last run: what that run did not write is taken back, so an extension
never outlives the module it was built from

## choosing what to compile

```sh
by compile                 # every .by and .py file in the project
by compile hot.by          # hot is native, every other module is interpreted
by compile a.by b.by       # a and b are native
```

the arguments are files, not module names, and a hand-written `.py` compiles as a `.by` does. naming
a file decides what is compiled, not what is written: the whole project is checked and transpiled
either way

each run describes itself and nothing before it, so `by compile a.by` followed by
`by compile b.by` leaves `b` native and `a` interpreted. name everything you want in one run

## running it

`by run --compiled` compiles what the program imports and runs it, without you managing a tree. with
this `main.by`:

```by
from hot import total

print(total(10))
```

```console
$ by run --compiled main
285
```

the entry module itself stays interpreted, since running something as `__main__` needs a code
object and an extension module has none. `--compiled` cannot be combined with `--coverage`: a native
module runs no python lines to measure

## code it cannot compile

a function the compiler cannot lower is left to its interpreted definition rather than failing the
build. the module's transpiled python is embedded in the extension and runs when it is imported, so
that function still exists and module-level code still runs. `--verbose` says which ones, and why.
add this to `hot.by`:

```by
def run_code(source: str) -> object:
    return eval(source)
```

```console
$ by compile hot.by --verbose
/app/hot.by -> /app/build/hot.cpython-313-darwin.so
info[declined]: `run_code` was left to its interpreted definition
 --> hot.by:7:1
  |
7 | / def run_code(source: str) -> object:
8 | |     return eval(source)
  | |_______________________^ `eval` with no namespace of its own runs in the calling frame's, and a compiled function pushes none

Found 1 diagnostic

compiled 1 module(s)
1 function(s) left to the interpreted definition
```

a decline is invisible unless you look for it, so a module that means to be fully compiled can say
so and be held to it. `--require-native` fails on any function left interpreted:

```console
$ by compile hot.by --require-native
by failed
  Cause: could not compile /app/hot.by
  Cause: `require-native` is on and 1 function(s) were left interpreted:
  run_code: `eval` with no namespace of its own runs in the calling frame's, and a compiled function pushes none
```

and `--no-any` fails on a gradual type in a compiled signature, the commonest reason code ends up
slower than it looks:

```by
def loose(x: dynamic) -> dynamic:
    return x
```

```console
$ by compile loose.by --no-any
by failed
  Cause: could not compile /app/loose.by
  Cause: `no-any` is on and 2 place(s) in this module have a gradual type:
  loose: `x` is gradual
  loose: `return` is gradual
```

either failure exits 2 and writes no extension for that module

a check *error* is a different matter. as with `by build`, everything that could be written is
written and the exit status says what was reported, so a project with an error still produces its
extensions and exits 1

## how a compiled module differs

compiled and interpreted code agree, and the differences that remain are the
[semantic deltas](../development/compilation/plan.md#semantic-deltas). the ones you are most likely
to meet:

- a `data class` constructor checks its arguments, since a compiled field is unboxed:
    `Point("a", 1)` raises `TypeError`
- assigning a method onto a class from outside it raises `TypeError`, since an emitted class is
    sealed
- `is` on a fixed-length tuple, a small `int` or an interned `str` is unspecified
- `f.__code__` of a compiled function belongs to the function that forwards to it, and
    `f.__wrapped__` is the interpreted definition, which is where `inspect` reads the signature

a traceback through a compiled function names its `.by` file and line. a debugger does not stop
inside one: see [debugging](debugger.md#compiled-modules)

## configuration

flags are in [the cli reference](../cli-reference.md#by-compile). two project options trade a
behaviour of python for speed, under `compile`:

```toml
[tool.basedpython.compile]
bind-functions-early = true
follow-recursion-limit = false
```

- `bind-functions-early`, off by default: a compiled call to a function of the same module goes
    straight to it, where it would otherwise ask whether the name still holds that function. with it
    on, `mock.patch("hot.helper")` no longer reaches `hot`'s own compiled calls
- `follow-recursion-limit`, on by default: a compiled recursion counts its calls against python's
    recursion limit. with it off, `sys.setrecursionlimit` does not reach compiled code, and a
    recursion runs until the stack is close to running out, where it raises `RecursionError` rather
    than crashing

## python versions

a native build needs python 3.11 or later. 3.11 through 3.14 are supported, as are the free-threaded
builds of 3.13 and 3.14

```console
$ PYTHON=python3.10 by compile hot.by
by failed
  Cause: a native build needs python 3.11 or later, and `python3.10` is python 3.10
```

the interpreter is the one `by run` would use, [chosen the same way](../cli-reference.md#by-run):
the project's environment first, and `PYTHON` only where the project has none. its development
headers have to be available, as does the C compiler it reports it was built with, which is the one
`by compile` uses

an extension is built for one interpreter. another interpreter does not see it, and imports the
`.py` beside it instead, which is correct and slower

## further reading

- [how the compiler is designed](../development/compilation/index.md)
- [what has shipped](../development/compilation/plan.md#what-has-shipped)
- [the runtime](../development/compilation/runtime.md)
