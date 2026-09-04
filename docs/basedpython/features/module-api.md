# module api enforcement

!!! warning "experimental"

    this feature is off unless the project asks for it:

    ```toml
    # basedpython.toml
    [experimental]
    module-api = true
    ```

    an `implements` declaration written without that is reported rather than
    quietly ignored. see [experimental features](../configuration.md#experimental-features)

`implements` says a module answers an interface, and has the type checker hold it
to that:

```by
# postgres.by
from .api import Backend

implements Backend

def connect(url: str) -> str:
    return url
```

a module that stops answering `Backend` is an error in *that module*, whether or
not anything ever assigns it to a `Backend`-typed place — which matters most for
a plugin that is only ever loaded by name, since nothing else would ever check it

## writing the interface

a module's members are unbound, so the interface spells them `static`:

```by
protocol Backend:
    name: str
    static def connect(url: str) -> str
```

a member like `name` is writable through the interface, so the module has to
declare its type — `name: str = "postgres"` rather than `name = "postgres"`, whose
type is the literal it was given

nothing else about the protocol changes. a class object answers a static-membered
protocol just as a module does, which is what lets a test substitute a fake:

```by
class FakeBackend:
    name: str = "fake"
    static def connect(url: str) -> str:
        return url

def run(backend: Backend) -> None: ...

run(FakeBackend)
```

## imposing an interface on a package

a `for` clause obliges other modules rather than the one it is written in. it
belongs in a package's `__init__`, and its patterns name what is inside that
package:

```by
# backends/__init__.by
from .api import Backend

implements Backend for ".*", "!.base"
```

every module in `backends` now answers `Backend` — including one added tomorrow
by someone who never read the interface, and including one nothing imports. the
obligation cannot be dropped by editing the module that fails it

the error is reported in the failing module, with the rule that imposed it
pointed at as well:

```text
error[unmet-module-api]: `backends.broken` does not answer `Backend`
 --> backends/broken.by:1:1
  |
1 | def connect(url: int) -> str:
  | ^
  |
 ::: backends/__init__.by:3:1
  |
3 | implements Backend for ".*"
  | ---------- required by this declaration
info: `connect` is `def connect(url: int) -> str`, but `Backend` declares it as `def connect(url: str) -> str`
```

### patterns

a pattern starts with a `.`, marking it relative to the package it is written in,
as a relative import does. `*` matches inside one name, `**` matches any number of
levels, and `!` excludes:

| pattern         | reaches                        |
| --------------- | ------------------------------ |
| `".*"`          | every submodule of the package |
| `".**"`         | the whole subtree              |
| `".pages.*"`    | the submodules of `pages`      |
| `".**.pages.*"` | a `pages` package at any depth |
| `".handler_*"`  | a submodule matched by name    |
| `"!.base"`      | carves one back out            |

a module with an `_`-prefixed component is not reached by a pattern containing a
wildcard — a private helper sitting among the plugins is not a plugin, and
neither is anything inside a private package. a pattern that names one outright,
with no wildcard in it at all (`"._named"`), still reaches it

a rule may only be written in a package's `__init__`, and its patterns may not
climb out of that package with `..`. a module finds the obligations imposed on it
by looking at the packages it is in, so a rule anywhere else would enforce
nothing, and a package cannot have requirements added to it from outside

## stubs

when a module has a `.byi` stub, the stub is what everything outside the module
reads, so the stub is what an obligation is about — and where its declarations
belong. an `implements` written in an implementation file that a stub shadows is
an error saying so, rather than a check of a surface nobody else can see

## what a module has to answer

only the members the interface names. everything else about the module is its own
business — a module may expose whatever else it likes, and `_`-prefixing and
`__all__` say what is not part of its surface, as they always did

a module whose `__getattr__` answers every name cannot be checked at all, so
attaching an obligation to one is an error rather than a check that always passes

## what it does not do

nothing is enforced at runtime, and nothing is hidden: `getattr` still reaches
whatever the module defines, and python that was never type-checked sees the
module it always saw. this is a type-level contract, exactly like a class's
private members
