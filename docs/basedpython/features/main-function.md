# main function

a module-level function named `main` is the program entry point. basedpython
appends a `__main__` guard that invokes it, so running the file as a script
executes `main`:

```by
def main():
    print("hello")
```

transpiles to:

```python
def main():
    print("hello")
if __name__ == "__main__":
    main()
```

## async

an `async def main` is driven through `asyncio.run`, and the import is added
for you:

```by
async def main():
    await serve()
```

```python
import asyncio
async def main():
    await serve()
if __name__ == "__main__":
    asyncio.run(main())
```

## command-line arguments

`main`'s parameters are the program's command-line interface. each one can be
given positionally, in declaration order, or by name:

```by
def main(name: str):
    print(name)
```

```sh
> by run main asdf
asdf
> by run main --name asdf
asdf
```

a parameter with a default is optional; one without is required. an underscore
in a name is also spelled with a dash on the command line, and both forms are
accepted (`--out-dir` and `--out_dir`). the docstring of `main` becomes the
`--help` description:

```by
from pathlib import Path

def main(name: str, count: int = 1, out_dir: Path = Path("."), verbose: bool = False):
    """greet someone repeatedly"""
```

```sh
> by run main bob 3 /tmp --verbose
> by run main --name bob --count 3 --out-dir /tmp --verbose
```

### types

a parameter is exposed on the command line when its annotation is one of
`str`, `int`, `float`, `bool`, or `Path`. the annotation is used as the
converter, so a bad value is reported before `main` runs:

```sh
> by run main bob notanint
main.py: error: argument count: invalid int value: 'notanint'
```

`bool` is a flag rather than a value: `--verbose` sets it `True` and
`--no-verbose` sets it `False`. a flag takes no positional slot, so the value
tokens on the command line still line up with the remaining parameters. a
`bool` without a default is required, meaning one of the two flags must be
given.

an [optional](wrapped-results.md) `T?` is spelled as its `T` — leaving the
argument out is what the `None` stands for — and so is a `T | None`

a union of literals becomes the set of values the argument accepts, written
either way round. argparse rejects anything else before `main` runs:

```by
def main(mode: "fast" | "slow" = "fast"): ...
```

```by
from typing import Literal

def main(mode: Literal["fast", "slow"] = "fast"): ...
```

```sh
> by run main --mode sideways
main: error: argument --mode: invalid choice: 'sideways'
```

a union of a named type and a literal (`int | "auto"`) is not exposed: no
argument could satisfy both halves, so there is nothing to offer

a parameter with any other annotation is not exposed. if it has a default it
simply keeps that default; if it is required, `main` cannot be called from the
command line at all and is left alone — the module gets no entry-point guard.
`**kwargs` is never exposed, and never blocks the guard.

### the arguments the interface does not claim

a program launched by another one is handed flags it never declared. a `*rest`
written *first* asks for them: everything after it is keyword-only, so the
arguments the interface does not recognise are the only positional ones and
they arrive in `rest`

```by
def main(*rest: str, games: int = 1):
    print(games, rest)
```

```sh
> by run main --games 3 --LadderServer 127.0.0.1
3 ('--LadderServer', '127.0.0.1')
```

they arrive as the strings the command line carried, so the vararg's own
annotation converts them the way a declared parameter's does — `*rest: int`
hands `main` integers. an annotation with no command-line spelling has nothing
to convert with, and the vararg goes back to being one nothing fills

a `*rest` written after an ordinary parameter cannot mean that either — the
parameter ahead of it would take the first unclaimed argument as its own — so
there it stays what python makes it

a flag the interface *does* declare is still matched, so `--games` reaches
`games`; anything else reaches `rest`, a misspelling of a declared flag
included

positional-only and keyword-only parameters keep their calling convention: a
positional-only parameter is still passed positionally to `main` even when the
command line named it with `--`, and a keyword-only one never takes a
positional slot.

## the project entry point

`main` makes a *module* runnable; `run.main` says which module that is for the
project as a whole:

```toml
[tool.basedpython.run]
main = "app.cli"
```

`by run` with no module then runs `app.cli`. see the
[cli reference](../cli-reference.md#by-run)

## scope

only a *top-level* function named `main` is recognized — a `main` method on a
class is just a method. when several top-level `main` definitions exist, the
last one wins, matching the binding `main` resolves to at runtime.

the guard is suppressed in three cases:

- the module already invokes `main` itself, either through a hand-written
    `if __name__ == "__main__":` guard or a bare top-level `main()` call. the
    entry point is never run twice
- `main` is marked `private` (it is renamed and is not a public entry point)
- `main` has a required parameter the command line can't supply, so calling it
    would raise `TypeError`

## completion

at module level, completing `main` writes the whole definition rather than the
name — there is only one thing a top-level `main` can be. once the module has
one, the name completes to it like any other

## why

most scripts end with the same boilerplate guard. naming the entry point
`main` and letting basedpython emit the guard keeps the source focused on the
program itself, the way a compiled language treats its `main`
