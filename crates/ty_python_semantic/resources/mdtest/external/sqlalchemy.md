# SQLAlchemy

```toml
[environment]
python-version = "3.13"
python-platform = "linux"

[project]
dependencies = ["SQLAlchemy==2.0.44"]
```

## ORM Model

A plain 2.0 declarative model inherits SQLAlchemy's runtime `__init__(self, **kw)`, which accepts
any mapped attribute as a keyword. ty synthesizes the truthful signature from the `Mapped[T]`
annotations: keyword-only, and every parameter optional (SQLAlchemy enforces non-nullable columns at
flush, not at construction).

```py
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column

class Base(DeclarativeBase):
    pass

class User(Base):
    __tablename__ = "user"

    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str] = mapped_column()

reveal_type(User.__init__)  # revealed: (self: User, *, id: int = ..., name: str = ...) -> None

user = User(name="John Doe")
reveal_type(user.id)  # revealed: int
reveal_type(user.name)  # revealed: str
```

Any subset of the mapped attributes constructs, including none of them:

```py
User()
User(id=1)
User(id=1, name="Alice")
```

A keyword that names no mapped attribute is now an error (previously the loose `**kw: Any` silently
accepted it):

```py
# error: [unknown-argument] "Argument `nam` does not match any known parameter"
User(nam="typo")
```

The parameter type is the `Mapped[T]` argument, so a value of the wrong type is rejected:

```py
# error: [invalid-argument-type]
User(name=123)
```

## Relationships are constructor keywords

`relationship()` fields are mapped attributes too, so they join the synthesized constructor:

```py
from __future__ import annotations

from sqlalchemy import ForeignKey
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column, relationship

class Base(DeclarativeBase):
    pass

class Address(Base):
    __tablename__ = "address"

    id: Mapped[int] = mapped_column(primary_key=True)
    email: Mapped[str]
    user_id: Mapped[int] = mapped_column(ForeignKey("user.id"))
    user: Mapped[User] = relationship(back_populates="addresses")

class User(Base):
    __tablename__ = "user"

    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str]
    addresses: Mapped[list[Address]] = relationship(back_populates="user")

# revealed: (self: User, *, id: int = ..., name: str = ..., addresses: list[Address] = ...) -> None
reveal_type(User.__init__)

alice = User(name="Alice", addresses=[Address(email="a@example.com")])
reveal_type(alice.addresses)  # revealed: list[Address]
```

## Mixin and abstract fields are inherited

Fields declared on an `__abstract__` model or an ordinary declarative mixin flow into a concrete
model's constructor through the MRO:

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
    name: Mapped[str]

# revealed: (self: Widget, *, created_at: datetime = ..., id: int = ..., name: str = ...) -> None
reveal_type(Widget.__init__)
```

## A user-defined constructor wins

An explicit `__init__` in the class body overrides synthesis:

```py
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column

class Base(DeclarativeBase):
    pass

class User(Base):
    __tablename__ = "user"

    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str]

    def __init__(self, name: str) -> None:
        self.name = name

reveal_type(User.__init__)  # revealed: def __init__(self, name: str) -> None
User("Alice")
# error: [missing-argument]
User()
```

## MappedAsDataclass is unchanged

`MappedAsDataclass` models are PEP 681 `dataclass_transform` classes and keep going through the
dataclass path — `init=False`, `default=`, and positional fields all honored:

```py
from sqlalchemy.orm import DeclarativeBase, MappedAsDataclass, Mapped, mapped_column

class Base(MappedAsDataclass, DeclarativeBase):
    pass

class User(Base):
    __tablename__ = "user"

    id: Mapped[int] = mapped_column(primary_key=True, init=False)
    name: Mapped[str] = mapped_column()

# id is init=False, so it is not a constructor parameter, and name is positional-or-keyword. The
# parameter type comes from `Mapped.__set__`, which the dataclass descriptor path reads (unlike the
# plain-declarative path, which uses the `Mapped[T]` argument directly).
# revealed: (self: User, name: SQLCoreOperations[str] | str) -> None
reveal_type(User.__init__)
User("Alice")
```

## Basic query example

First, set up a `Session`:

```py
from sqlalchemy import select, Integer, Text, Boolean
from sqlalchemy.orm import Session
from sqlalchemy.orm import DeclarativeBase
from sqlalchemy.orm import Mapped, mapped_column
from sqlalchemy import create_engine

engine = create_engine("sqlite://example.db")
session = Session(engine)
```

And define a simple model:

```py
class Base(DeclarativeBase):
    pass

class User(Base):
    __tablename__ = "users"

    id: Mapped[int] = mapped_column(Integer, primary_key=True)
    name: Mapped[str] = mapped_column(Text)
    is_admin: Mapped[bool] = mapped_column(Boolean, default=False)
```

Finally, we can execute queries:

```py
stmt = select(User)
reveal_type(stmt)  # revealed: Select[tuple[User]]

users = session.scalars(stmt).all()
reveal_type(users)  # revealed: Sequence[User]

for row in session.execute(stmt):
    reveal_type(row)  # revealed: Row[tuple[User]]

stmt = select(User).where(User.name == "Alice")
alice1 = session.scalars(stmt).first()
reveal_type(alice1)  # revealed: User | None

alice2 = session.scalar(stmt)
reveal_type(alice2)  # revealed: User | None

result = session.execute(stmt)
row = result.one_or_none()
assert row is not None
(alice3,) = row._tuple()
reveal_type(alice3)  # revealed: User
```

This also works with more complex queries:

```py
stmt = select(User).where(User.is_admin == True).order_by(User.name).limit(10)
admin_users = session.scalars(stmt).all()
reveal_type(admin_users)  # revealed: Sequence[User]
```

We can also specify particular columns to select:

```py
stmt = select(User.id, User.name)
reveal_type(stmt)  # revealed: Select[tuple[int, str]]

ids_and_names = session.execute(stmt).all()
reveal_type(ids_and_names)  # revealed: Sequence[Row[tuple[int, str]]]

for row in session.execute(stmt):
    reveal_type(row)  # revealed: Row[tuple[int, str]]

for user_id, name in session.execute(stmt).tuples():
    reveal_type(user_id)  # revealed: int
    reveal_type(name)  # revealed: str

result = session.execute(stmt)
row = result.one_or_none()
assert row is not None
user_id, name = row._tuple()
reveal_type(user_id)  # revealed: int
reveal_type(name)  # revealed: str

stmt = select(User.id).where(User.name == "Alice")

reveal_type(stmt)  # revealed: Select[tuple[int]]

alice_id = session.scalars(stmt).first()
reveal_type(alice_id)  # revealed: int | None

alice_id = session.scalar(stmt)
reveal_type(alice_id)  # revealed: int | None
```

Using the legacy `query` API also works:

```py
users_legacy = session.query(User).all()
reveal_type(users_legacy)  # revealed: list[User]

query = session.query(User)
reveal_type(query)  # revealed: Query[User]

reveal_type(query.all())  # revealed: list[User]

for row in query:
    reveal_type(row)  # revealed: User
```

And similarly when specifying particular columns:

```py
query = session.query(User.id, User.name)
reveal_type(query)  # revealed: RowReturningQuery[tuple[int, str]]

reveal_type(query.all())  # revealed: list[Row[tuple[int, str]]]

for row in query:
    reveal_type(row)  # revealed: Row[tuple[int, str]]
```

## Async API

The async API is supported as well:

```py
from sqlalchemy.ext.asyncio import AsyncSession
from sqlalchemy import select, Integer, Text
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column

class Base(DeclarativeBase):
    pass

class User(Base):
    __tablename__ = "users"

    id: Mapped[int] = mapped_column(Integer, primary_key=True)
    name: Mapped[str] = mapped_column(Text)

async def test_async(session: AsyncSession):
    stmt = select(User).where(User.name == "Alice")
    alice = await session.scalar(stmt)
    reveal_type(alice)  # revealed: User | None

    stmt = select(User.id, User.name)
    result = await session.execute(stmt)
    for user_id, name in result.tuples():
        reveal_type(user_id)  # revealed: int
        reveal_type(name)  # revealed: str
```
