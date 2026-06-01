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

## scope

only a *top-level* function named `main` is recognized — a `main` method on a
class is just a method. when several top-level `main` definitions exist, the
last one wins, matching the binding `main` resolves to at runtime.

the guard is suppressed in three cases:

- the module already invokes `main` itself, either through a hand-written
    `if __name__ == "__main__":` guard or a bare top-level `main()` call. the
    entry point is never run twice
- `main` is marked `private` (it is renamed and is not a public entry point)
- `main` cannot be called with no arguments. `main` is wired up only when it
    takes no required parameters; forwarding command-line arguments is a planned
    extension, so a `main` that requires an argument is left alone for now

## why

most scripts end with the same boilerplate guard. naming the entry point
`main` and letting basedpython emit the guard keeps the source focused on the
program itself, the way a compiled language treats its `main`
