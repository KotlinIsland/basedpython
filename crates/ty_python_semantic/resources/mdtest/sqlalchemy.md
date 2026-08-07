# SQLAlchemy (mock stubs)

Hermetic pins for the *mechanism* of ty's dedicated SQLAlchemy support, using minimal hand-written
stubs that mirror the shapes SQLAlchemy 2.0 ships inline: the `Mapped[T]` descriptor (class-level
`InstrumentedAttribute[T]`, instance-level `T`) and the `DeclarativeBase` / `MappedAsDataclass`
bases. The `external/sqlalchemy.md` suite checks the same behaviours against the real package.

## Declarative constructor synthesis

A plain declarative model synthesizes a keyword-only `__init__` from its `Mapped[T]` annotations,
with every parameter optional. The parameter type is the unwrapped `Mapped` argument, not the
descriptor. Mixin fields flow in through the MRO. A keyword that names no mapped attribute is an
error; an explicit `__init__` wins.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/sqlalchemy/__init__.pyi`:

```pyi
from typing import Any

class ForeignKey:
    def __init__(self, *args: Any, **kw: Any) -> None: ...
```

`/.venv/<path-to-site-packages>/sqlalchemy/orm/__init__.pyi`:

```pyi
from sqlalchemy.orm.decl_api import DeclarativeBase as DeclarativeBase
from sqlalchemy.orm.decl_api import MappedAsDataclass as MappedAsDataclass
from sqlalchemy.orm.base import Mapped as Mapped
from sqlalchemy.orm._orm_constructors import mapped_column as mapped_column
from sqlalchemy.orm._orm_constructors import relationship as relationship
```

`/.venv/<path-to-site-packages>/sqlalchemy/orm/base.pyi`:

```pyi
from typing import Any, Generic, TypeVar, overload

_T = TypeVar("_T")

class InstrumentedAttribute(Generic[_T]): ...

class Mapped(Generic[_T]):
    @overload
    def __get__(self, instance: None, owner: Any) -> InstrumentedAttribute[_T]: ...
    @overload
    def __get__(self, instance: object, owner: Any) -> _T: ...
    def __set__(self, instance: Any, value: _T) -> None: ...
```

`/.venv/<path-to-site-packages>/sqlalchemy/orm/decl_api.pyi`:

```pyi
from typing import Any

class DeclarativeBase:
    def __init__(self, **kw: Any) -> None: ...

class MappedAsDataclass: ...
```

`/.venv/<path-to-site-packages>/sqlalchemy/orm/_orm_constructors.pyi`:

```pyi
from typing import Any

def mapped_column(*args: Any, **kw: Any) -> Any: ...
def relationship(*args: Any, **kw: Any) -> Any: ...
```

The class/instance duality resolves through the descriptor, and the synthesized constructor is
keyword-only with optional parameters:

`main.py`:

```py
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column
from sqlalchemy.orm.base import InstrumentedAttribute

class Base(DeclarativeBase):
    pass

class User(Base):
    __tablename__ = "user"

    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str]

reveal_type(User.__init__)  # revealed: (self: User, *, id: int = ..., name: str = ...) -> None
reveal_type(User.id)  # revealed: InstrumentedAttribute[int]

user = User(name="Alice")
reveal_type(user.id)  # revealed: int
reveal_type(user.name)  # revealed: str

User()
User(id=1, name="Alice")
# error: [unknown-argument] "Argument `nam` does not match any known parameter"
User(nam="typo")
# error: [invalid-argument-type]
User(name=123)
```

A declarative mixin is an ordinary class; its `Mapped` fields still reach the model's constructor:

`mixin.py`:

```py
from datetime import datetime

from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column

class Base(DeclarativeBase):
    pass

class TimestampMixin:
    created_at: Mapped[datetime]

class Widget(TimestampMixin, Base):
    __tablename__ = "widget"

    id: Mapped[int] = mapped_column(primary_key=True)

# revealed: (self: Widget, *, created_at: datetime = ..., id: int = ...) -> None
reveal_type(Widget.__init__)
```

An explicit constructor in the class body overrides synthesis:

`explicit_init.py`:

```py
from sqlalchemy.orm import DeclarativeBase, Mapped

class Base(DeclarativeBase):
    pass

class Account(Base):
    __tablename__ = "account"

    name: Mapped[str]

    def __init__(self, name: str) -> None:
        self.name = name

reveal_type(Account.__init__)  # revealed: def __init__(self, name: str)
```

## Non-Mapped annotations are not fields

Only `Mapped[T]` annotations are mapped attributes. Plain annotations and `ClassVar`s are ignored by
the constructor synthesis.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/sqlalchemy/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/sqlalchemy/orm/__init__.pyi`:

```pyi
from sqlalchemy.orm.decl_api import DeclarativeBase as DeclarativeBase
from sqlalchemy.orm.base import Mapped as Mapped
```

`/.venv/<path-to-site-packages>/sqlalchemy/orm/base.pyi`:

```pyi
from typing import Any, Generic, TypeVar

_T = TypeVar("_T")

class Mapped(Generic[_T]):
    def __get__(self, instance: object, owner: Any) -> _T: ...
    def __set__(self, instance: Any, value: _T) -> None: ...
```

`/.venv/<path-to-site-packages>/sqlalchemy/orm/decl_api.pyi`:

```pyi
from typing import Any

class DeclarativeBase:
    def __init__(self, **kw: Any) -> None: ...
```

```py
from typing import ClassVar

from sqlalchemy.orm import DeclarativeBase, Mapped

class Base(DeclarativeBase):
    pass

class User(Base):
    __tablename__ = "user"

    id: Mapped[int]
    registry: ClassVar[str]
    not_a_column: int

reveal_type(User.__init__)  # revealed: (self: User, *, id: int = ...) -> None
```

## An unresolved base degrades to the runtime constructor

When any base is unresolvable the field list is incomplete, so synthesis is skipped and the model
keeps SQLAlchemy's runtime `__init__(self, **kw)` rather than an unsound closed signature.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/sqlalchemy/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/sqlalchemy/orm/__init__.pyi`:

```pyi
from sqlalchemy.orm.decl_api import DeclarativeBase as DeclarativeBase
from sqlalchemy.orm.base import Mapped as Mapped
```

`/.venv/<path-to-site-packages>/sqlalchemy/orm/base.pyi`:

```pyi
from typing import Any, Generic, TypeVar

_T = TypeVar("_T")

class Mapped(Generic[_T]):
    def __get__(self, instance: object, owner: Any) -> _T: ...
    def __set__(self, instance: Any, value: _T) -> None: ...
```

`/.venv/<path-to-site-packages>/sqlalchemy/orm/decl_api.pyi`:

```pyi
from typing import Any

class DeclarativeBase:
    def __init__(self, **kw: Any) -> None: ...
```

```py
from sqlalchemy.orm import DeclarativeBase, Mapped

# error: [unresolved-import]
from does_not_exist import Weird

class Model(DeclarativeBase, Weird):
    id: Mapped[int]

reveal_type(Model.__init__)  # revealed: def __init__(self, **kw: Any)
```
