# basedpython: module api enforcement

a module is a structural value, so it can answer a protocol through its public surface. an
`implements` declaration attaches that obligation to the module permanently, so a break is reported
in the module that broke rather than wherever something happens to assign it — or nowhere at all,
when nothing does.

the feature is experimental, so a project asks for it by name:

```toml
[experimental]
module-api = true
```

## a module answers the interface it declares

the interface spells its members `static`, because a module's members are unbound.

`api.by`:

```by
protocol Backend:
    name: str
    static def connect(url: str) -> str
```

`postgres.by`:

```by
from api import Backend

implements Backend

name: str = "postgres"

def connect(url: str) -> str:
    return url
```

## a class object stands in for a module

a static-membered protocol is answered by a class object just as it is by a module, so a test can
substitute a fake without a module of its own.

```by
protocol Backend:
    name: str
    static def connect(url: str) -> str

class FakeBackend:
    name: str = "fake"
    static def connect(url: str) -> str:
        return url

def run(backend: Backend) -> None: ...

run(FakeBackend)
```

## a member the module does not have

the declaration is what the diagnostic points at, because that is what the module promised.

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`postgres.by`:

```by
from api import Backend

# error: [unmet-module-api] "does not answer `Backend`"
implements Backend
```

## a member whose shape is wrong

<!-- snapshot-diagnostics -->

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`postgres.by`:

```by
from api import Backend

# error: [unmet-module-api] "`postgres` does not answer `Backend`"
implements Backend

def connect(url: int) -> str:
    return str(url)
```

## a package imposes an interface on its submodules

a `for` clause in a package's `__init__` obliges the modules its patterns name. the patterns are
relative to that package, spelled with a leading `.` as a relative import is.

<!-- snapshot-diagnostics -->

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`backends/__init__.by`:

```by
from api import Backend

implements Backend for ".*"
```

`backends/good.by`:

```by
def connect(url: str) -> str:
    return url
```

`backends/bad.by`:

```by
x = 1  # error: [unmet-module-api] "`backends.bad` does not answer `Backend`"
```

## a package that imports its own submodules

the shape a plugin package actually has: the `__init__` re-exports from the very modules its rule
obliges.

`plug/api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`plug/__init__.by`:

```by
from .api import Backend
from .good import connect

implements Backend for ".*", "!.api"

__all__ = ["connect"]
```

`plug/good.by`:

```by
def connect(url: str) -> str:
    return url
```

`plug/bad.by`:

```by
x = 1  # error: [unmet-module-api] "`plug.bad` does not answer `Backend`"
```

## a rule does not oblige the package that wrote it

patterns name what is *inside* the package, so `__init__` itself is never one of them.

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`backends/__init__.by`:

```by
from api import Backend

implements Backend for ".**"
```

`backends/good.by`:

```by
def connect(url: str) -> str:
    return url
```

## a pattern may carve a module back out

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`backends/__init__.by`:

```by
from api import Backend

implements Backend for ".*", "!.registry"
```

`backends/good.by`:

```by
def connect(url: str) -> str:
    return url
```

`backends/registry.by`:

```by
names: list[str] = []
```

## a private submodule is reached only by name

a leading underscore already means "not part of the surface", so `.*` passes a private helper by.
naming one exactly still reaches it.

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`backends/__init__.by`:

```by
from api import Backend

implements Backend for ".*", "._named"
```

`backends/_helper.by`:

```by
def helper() -> int:
    return 1
```

`backends/_named.by`:

```by
y = 2  # error: [unmet-module-api] "`backends._named` does not answer `Backend`"
```

## an interface has to be a protocol

a module answers an interface through its public surface, which is what a protocol describes. a
concrete class promises state a module has no way to carry.

```by
class Backend:
    def connect(self) -> None: ...

# error: [invalid-module-api] "`Backend` is not a protocol"
implements Backend
```

## a rule outside a package's `__init__`

a module finds the rules imposed on it by walking the packages it is in, so a rule written anywhere
else would reach nothing.

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`loose.by`:

```by
from api import Backend

# error: [invalid-module-api] "may only be written in a package's `__init__`"
implements Backend for ".*"
```

## a pattern that is not relative to its package

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`backends/__init__.by`:

```by
from api import Backend

# error: [invalid-module-api] "is not a pattern relative to `backends`"
implements Backend for "backends.*"
```

## a rule that reaches nothing

a pattern that matches no module enforces nothing, silently, which is worse than no rule at all.

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`backends/__init__.by`:

```by
from api import Backend

# error: [invalid-module-api] "reaches no module"
implements Backend for ".nothing_*"
```

`backends/postgres.by`:

```by
def connect(url: str) -> str:
    return url
```

## a declaration inside a body

an obligation is about a module's surface, so there is nothing for one written inside a body to
attach to.

```by
protocol Backend:
    static def connect(url: str) -> str

def f() -> None:
    # error: [invalid-module-api] "belongs at module level"
    implements Backend
```

## a module whose `__getattr__` answers everything

every requirement would be met without anything being defined, so the obligation cannot be checked
at all.

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`dynamic_module.by`:

```by
from typing import Any

from api import Backend

# error: [invalid-module-api] "cannot be checked against `Backend`"
implements Backend

def __getattr__(name: str) -> Any: ...
```

## a stub is the module's api

when a module has a stub, the stub is what everything outside it reads, so the stub is what the
obligation is about.

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`postgres.byi`:

```byi
from api import Backend

implements Backend

def connect(url: str) -> str: ...
```

`postgres.by`:

```by
def connect(url: str) -> str:
    return url
```

## a declaration in a file its stub shadows

nothing outside the module reads that file's surface, so a declaration there would be about a
surface nobody sees — and would leave the stub, which *is* the api, unchecked.

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`postgres.byi`:

```byi
def connect(url: str) -> str: ...
```

`postgres.by`:

```by
from api import Backend

# error: [invalid-module-api] "this module's api is its stub"
implements Backend

def connect(url: str) -> str:
    return url
```

## one interface, however many declarations ask for it

a module inside a package that imposes an interface, which also declares it itself, has one thing
left to do about it — so it is told once.

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`backends/__init__.by`:

```by
from api import Backend

implements Backend for ".*"
implements Backend for ".**"
```

`backends/bad.by`:

```by
from api import Backend

# error: [unmet-module-api] "`backends.bad` does not answer `Backend`"
implements Backend
```

## a rule may name several interfaces

each is checked on its own, so answering one says nothing about the other.

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str

protocol Named:
    name: str
```

`backends/__init__.by`:

```by
import api

implements api.Backend, api.Named for ".*"
```

`backends/bad.by`:

```by
name: str = "bad"  # error: [unmet-module-api] "`backends.bad` does not answer `Backend`"
```

## a pattern that cannot be resolved does not disable the rest of the rule

the malformed pattern is reported; the ones beside it go on meaning what they say.

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`backends/__init__.by`:

```by
from api import Backend

# error: [invalid-module-api] "is not a pattern relative to `backends`"
implements Backend for ".*", "backends.other"
```

`backends/bad.by`:

```by
x = 1  # error: [unmet-module-api] "`backends.bad` does not answer `Backend`"
```

## a wildcard does not reach inside a private package

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`backends/__init__.by`:

```by
from api import Backend

implements Backend for ".**"
```

`backends/good.by`:

```by
def connect(url: str) -> str:
    return url
```

`backends/_private/__init__.by`:

```by
x = 1
```

`backends/_private/helper.by`:

```by
y = 2
```

## a python submodule is obliged like any other

a rule names modules, not files, so a `.py` module in the package answers for itself.

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`backends/__init__.by`:

```by
from api import Backend

implements Backend for ".*"
```

`backends/legacy.py`:

```py
x = 1  # error: [unmet-module-api] "`backends.legacy` does not answer `Backend`"
```

## an interface and the module obliged to answer it may import each other

resolving the interface a rule names infers the declaring module's code, which is how a package that
re-exports from the modules it obliges reaches back into them.

`plug/api.by`:

```by
from plug.good import connect as connect

protocol Backend:
    static def connect(url: str) -> str
```

`plug/__init__.by`:

```by
from .api import Backend

implements Backend for ".*", "!.api"
```

`plug/good.by`:

```by
def connect(url: str) -> str:
    return url
```

## a subscripted interface

a specialization would be dropped silently, so it is a syntax error rather than an obligation the
author did not write.

```by
protocol Backend[T]:
    static def connect(url: T) -> str

# error: [invalid-syntax] "an `implements` declaration takes interface names"
# error: [unmet-module-api] "does not answer `Backend[Unknown]`"
implements Backend[int]
```

## a comma with no interface after it

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`backends/__init__.by`:

```by
from api import Backend

# error: [invalid-syntax] "takes an interface name after `,`"
implements Backend, for ".*"
```

## a namespace package between the rule and the module

a directory with no `__init__` carries no rules of its own, and the walk continues past it to the
package that does.

`api.by`:

```by
protocol Backend:
    static def connect(url: str) -> str
```

`backends/__init__.by`:

```by
from api import Backend

implements Backend for ".**"
```

`backends/space/deep.by`:

```by
x = 1  # error: [unmet-module-api] "`backends.space.deep` does not answer `Backend`"
```

## the feature is off unless the project asks for it

a declaration written while the feature is off is reported rather than ignored — an obligation
nothing checks is the failure this feature exists to remove.

```toml
[experimental]
module-api = false
```

```by
protocol Backend:
    static def connect(url: str) -> str

# error: [invalid-module-api] "`implements` is an experimental feature, and is off"
implements Backend
```
