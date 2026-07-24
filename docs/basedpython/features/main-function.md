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

a parameter with any other annotation is not exposed. if it has a default it
simply keeps that default; if it is required, `main` cannot be called from the
command line at all and is left alone — the module gets no entry-point guard.
`*args` / `**kwargs` are never exposed, and never block the guard.

positional-only and keyword-only parameters keep their calling convention: a
positional-only parameter is still passed positionally to `main` even when the
command line named it with `--`, and a keyword-only one never takes a
positional slot.

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

## why

most scripts end with the same boilerplate guard. naming the entry point
`main` and letting basedpython emit the guard keeps the source focused on the
program itself, the way a compiled language treats its `main`
