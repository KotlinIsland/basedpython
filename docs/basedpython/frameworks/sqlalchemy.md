# sqlalchemy support

basedpython supports sqlalchemy 2.0 declarative models. type checking is precise, and constructor calls check correctly against your schema.

## what works

### declarative models with `Mapped` fields

- models that inherit from `DeclarativeBase` check precisely
- field types declared with `Mapped[T]` annotations are understood: `name: Mapped[str]`, `age: Mapped[int]`
- constructor calls: `User(name="Alice", age=30)` checks that you're using the right field names and types
- relationships: `relationship()` fields are constructor keywords too, and their types check correctly
- abstract models and mixins: fields flow from base classes into subclasses
- `MappedAsDataclass` models work via the dataclass machinery

### transpilation compatibility

- `optional?.chaining` works across nullable relationships: `user?.address?.city`
- `checked cast` and `cast?` operators work in model methods
- soundness checks work correctly with sqlalchemy's descriptor machinery

### runtime queries

- `select()`, `filter()`, comparison operators on columns
- session and result generics: `session.execute()`, `result.scalars()`
- async API

## limitations

### `DynamicMapped` and `WriteOnlyMapped`

only `Mapped[T]` unwraps for constructor synthesis. dynamic or write-only collections degrade to less precise checking. you can still use them — they just won't contribute to the constructor signature:

```by
users: WriteOnlyMapped[list["User"]]  # doesn't appear in __init__
```

### columns without `Mapped`

sqlalchemy 1.x style `Column()` attributes without `Mapped` aren't recognized. they check as the stubs allow, without constructor synthesis. upgrade to sqlalchemy 2.0's `Mapped` syntax for precise checking.

### custom enum columns

using a basedpython enum as a column type requires a `TypeDecorator` to handle persistence. declare `Mapped[YourEnum]` to check, but provide your own column serialization logic at runtime.

## incompatible patterns

**`init` shorthand or `data class` modifier**

sqlalchemy uses metaclass instrumentation. declaring your own `__init__` or stacking `@dataclass` on a model conflicts with this and breaks at runtime. you'll get an error:

```by
class User(Base):
    id: Mapped[int]

    init(id: int):  # error: conflicts with sqlalchemy's __init__
        self.id = id
```

**use:** let the synthesized constructor handle initialization. if you need custom logic, use a method instead.

## required setup

sqlalchemy 2.0 has inline type stubs (`py.typed`), so there's no additional setup. just install sqlalchemy 2.0+.

## examples

basic declarative model:

```by
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column

class Base(DeclarativeBase)

class User(Base):
    __tablename__ = "user"
    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str]
    email: Mapped[str | None] = None

user = User(name="Alice", email="alice@example.com")  # checks correctly
```

relationships:

```by
class User(Base):
    __tablename__ = "user"
    id: Mapped[int] = mapped_column(primary_key=True)
    posts: Mapped[list["Post"]] = relationship(back_populates="author")

class Post(Base):
    __tablename__ = "post"
    id: Mapped[int] = mapped_column(primary_key=True)
    author_id: Mapped[int]
    author: Mapped[User] = relationship(back_populates="posts")

user = User(posts=[])  # posts is a constructor argument
```

optional chaining:

```by
def get_first_post_author_email(user: User) -> str | None:
    return user.posts[0]?.author?.email  # checks correctly
```

## see also

- [sqlalchemy 2.0 documentation](https://docs.sqlalchemy.org/en/20/)
- framework compatibility matrix in the [frameworks overview](index.md#basedpython-features-and-framework-compatibility)
