# basedpython: sqlalchemy declarative models

sqlalchemy 2.0 declarative support is dedicated (`external/sqlalchemy.md`, `sqlalchemy.md`): ty
synthesizes a truthful keyword-only constructor from the `Mapped[T]` annotations. this suite is the
basedpython half — models *written in `.by`* must both check precisely and, once transpiled, behave
under the real framework. every `.by` block here is checker-clean, so the runtime-divergence harness
(`crates/ty/tests/mdtest_divergence.rs`) also transpiles and executes it against an installed
sqlalchemy (in-memory sqlite).

```toml
[environment]
python-version = "3.13"
python-platform = "linux"

[project]
dependencies = ["SQLAlchemy==2.0.44"]
```

## synthesized constructor round-trips through sqlite

the synthesized `__init__` accepts any subset of the mapped columns as keywords; a model constructs,
persists, and reads back with the declared attribute types.

```by
from sqlalchemy import create_engine
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column, Session

class Base(DeclarativeBase):
    pass

class User(Base):
    __tablename__ = "user"

    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str]

reveal_type(User.__init__)  # revealed: (self: User, *, id: int = ..., name: str = ...) -> None

engine = create_engine("sqlite://")
Base.metadata.create_all(engine)

with Session(engine) as session:
    # the id column is autoincrement, so constructing without it is valid
    session.add(User(name="Alice"))
    session.commit()

    fetched = session.query(User).filter(User.name == "Alice").one()
    reveal_type(fetched.name)  # revealed: str
    assert fetched.name == "Alice"
    assert fetched.id == 1
```

## optional chaining across a nullable relationship

a nullable many-to-one relationship reads as `Address | None`, so `?.` short-circuits cleanly at
runtime on a transient object whose relationship is unset, and threads the value through when it is
set.

```by
from __future__ import annotations

from sqlalchemy import ForeignKey
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column, relationship

class Base(DeclarativeBase):
    pass

class Address(Base):
    __tablename__ = "address"

    id: Mapped[int] = mapped_column(primary_key=True)
    city: Mapped[str]
    user_id: Mapped[int | None] = mapped_column(ForeignKey("user.id"))

class User(Base):
    __tablename__ = "user"

    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str]
    address: Mapped[Address | None] = relationship()

without = User(name="nobody")
reveal_type(without?.address?.city)  # revealed: str | None
assert (without?.address?.city) is None

withaddr = User(name="somebody", address=Address(city="NYC"))
assert (withaddr?.address?.city) == "NYC"
```

## class-body column default is left untouched

the mutable-default transform rewrites *function* argument defaults; a class-body `mapped_column`
default is left verbatim, so sqlalchemy applies it at insert time.

```by
from sqlalchemy import create_engine
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column, Session

class Base(DeclarativeBase):
    pass

class Flag(Base):
    __tablename__ = "flag"

    id: Mapped[int] = mapped_column(primary_key=True)
    enabled: Mapped[bool] = mapped_column(default=True)

engine = create_engine("sqlite://")
Base.metadata.create_all(engine)

with Session(engine) as session:
    session.add(Flag())
    session.commit()
    row = session.query(Flag).one()
    assert row.enabled is True
```

## soundness guards inside a model method

a guard emitted inside a model method body does not disturb the class's instrumentation; the method
still runs and the mapped attributes still resolve.

```by
from sqlalchemy import create_engine
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column, Session

class Base(DeclarativeBase):
    pass

class User(Base):
    __tablename__ = "user"

    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str]

    def greeting(self, prefix: str) -> str:
        return prefix + self.name

engine = create_engine("sqlite://")
Base.metadata.create_all(engine)

with Session(engine) as session:
    session.add(User(name="Ada"))
    session.commit()
    user = session.query(User).one()
    assert user.greeting("hi ") == "hi Ada"
```
