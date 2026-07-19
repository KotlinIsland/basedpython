# Django (mock stubs)

Hermetic pins for the *mechanisms* of ty's dedicated django support, using minimal hand-written
stubs that mirror the shapes django-stubs relies on: `Field[_ST, _GT]` descriptor generics whose
specialization is left to per-class `_pyi_private_set_type` / `_pyi_private_get_type` markers,
relation fields whose target comes from the `to=` argument, and the `ModelBase` runtime member
synthesis. The `external/django.md` suite checks the same behaviours against the real django-stubs
package.

Each test uses this environment:

## Field descriptor specialization pinning

The constructor call pins `_ST`/`_GT` from the class's marker declarations; `null=True` unions
`None` onto both sides.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/django-stubs/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/__init__.pyi`:

```pyi
from django.db.models.base import Model as Model
from django.db.models.fields import Field as Field, IntegerField as IntegerField
from django.db.models.fields.related import ForeignKey as ForeignKey
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/base.pyi`:

```pyi
from typing import Any

class Model:
    pk: Any
    def __init__(self, *args: Any, **kwargs: Any) -> None: ...
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/fields/__init__.pyi`:

```pyi
from typing import Any, Generic, TypeVar

_ST = TypeVar("_ST", contravariant=True)
_GT = TypeVar("_GT", covariant=True)

class Field(Generic[_ST, _GT]):
    _pyi_private_set_type: Any
    _pyi_private_get_type: Any
    def __init__(
        self,
        *args: Any,
        primary_key: bool = False,
        null: bool = False,
        **kwargs: Any,
    ) -> None: ...
    def __set__(self, instance: Any, value: _ST) -> None: ...
    def __get__(self, instance: Any, owner: Any) -> _GT: ...

class IntegerField(Field[_ST, _GT]):
    _pyi_private_set_type: int | str
    _pyi_private_get_type: int
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/fields/related.pyi`:

```pyi
from typing import Any

from django.db.models.fields import Field, _ST, _GT

class ForeignKey(Field[_ST, _GT]):
    _pyi_private_set_type: Any
    _pyi_private_get_type: Any
    def __init__(
        self,
        to: Any,
        *args: Any,
        related_name: str | None = None,
        primary_key: bool = False,
        null: bool = False,
        **kwargs: Any,
    ) -> None: ...
```

`main.py`:

```py
from django.db import models

class Author(models.Model):
    age = models.IntegerField()
    score = models.IntegerField(null=True)

reveal_type(models.IntegerField())  # revealed: IntegerField[int | str, int]
reveal_type(models.IntegerField(null=True))  # revealed: IntegerField[int | str | None, int | None]

author = Author()
reveal_type(author.age)  # revealed: int
reveal_type(author.score)  # revealed: int | None

author.age = "3"

# error: [invalid-assignment]
author.age = None
```

## A field class without marker declarations degrades to no pinning

The base `Field` declares its markers as `Any`; a custom field that does not redeclare them keeps
the unpinned specialization rather than guessing.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/django-stubs/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/__init__.pyi`:

```pyi
from django.db.models.base import Model as Model
from django.db.models.fields import Field as Field
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/base.pyi`:

```pyi
from typing import Any

class Model:
    pk: Any
    def __init__(self, *args: Any, **kwargs: Any) -> None: ...
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/fields/__init__.pyi`:

```pyi
from typing import Any, Generic, TypeVar

_ST = TypeVar("_ST", contravariant=True)
_GT = TypeVar("_GT", covariant=True)

class Field(Generic[_ST, _GT]):
    _pyi_private_set_type: Any
    _pyi_private_get_type: Any
    def __init__(self, *args: Any, null: bool = False, **kwargs: Any) -> None: ...
    def __set__(self, instance: Any, value: _ST) -> None: ...
    def __get__(self, instance: Any, owner: Any) -> _GT: ...

class OpaqueField(Field): ...
```

`main.py`:

```py
from django.db.models.fields import OpaqueField
from django.db import models

class Author(models.Model):
    blob = OpaqueField()

reveal_type(Author().blob)  # revealed: Unknown
```

## Relation fields, attnames, and member synthesis

The `to=` model is substituted for the marker's dynamic parts; `ModelBase`'s runtime members — the
auto `id`, the `pk` alias, and `<field>_id` attnames — are synthesized from the field list.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/django-stubs/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/__init__.pyi`:

```pyi
from django.db.models.base import Model as Model
from django.db.models.fields import Field as Field, IntegerField as IntegerField, CharField as CharField
from django.db.models.fields.related import ForeignKey as ForeignKey
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/base.pyi`:

```pyi
from typing import Any

class Model:
    pk: Any
    def __init__(self, *args: Any, **kwargs: Any) -> None: ...
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/fields/__init__.pyi`:

```pyi
from typing import Any, Generic, TypeVar

_ST = TypeVar("_ST", contravariant=True)
_GT = TypeVar("_GT", covariant=True)

class Field(Generic[_ST, _GT]):
    _pyi_private_set_type: Any
    _pyi_private_get_type: Any
    def __init__(
        self,
        *args: Any,
        primary_key: bool = False,
        null: bool = False,
        **kwargs: Any,
    ) -> None: ...
    def __set__(self, instance: Any, value: _ST) -> None: ...
    def __get__(self, instance: Any, owner: Any) -> _GT: ...

class IntegerField(Field[_ST, _GT]):
    _pyi_private_set_type: int | str
    _pyi_private_get_type: int

class CharField(Field[_ST, _GT]):
    _pyi_private_set_type: str | int
    _pyi_private_get_type: str
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/fields/related.pyi`:

```pyi
from typing import Any

from django.db.models.fields import Field, _ST, _GT

class ForeignKey(Field[_ST, _GT]):
    _pyi_private_set_type: Any
    _pyi_private_get_type: Any
    def __init__(
        self,
        to: Any,
        *args: Any,
        related_name: str | None = None,
        primary_key: bool = False,
        null: bool = False,
        **kwargs: Any,
    ) -> None: ...
```

`main.py`:

```py
from django.db import models

class Author(models.Model):
    name = models.CharField()

class Book(models.Model):
    author = models.ForeignKey(Author)
    editor = models.ForeignKey(Author, null=True)

book = Book()
reveal_type(book.author)  # revealed: Author
reveal_type(book.editor)  # revealed: Author | None
reveal_type(book.id)  # revealed: int
reveal_type(book.pk)  # revealed: int
reveal_type(book.author_id)  # revealed: int
reveal_type(book.editor_id)  # revealed: int | None

# a string (lazy) target degrades to no pinning
class Chapter(models.Model):
    book = models.ForeignKey("Book")

reveal_type(Chapter().book)  # revealed: Unknown
```

## An explicit primary key replaces the auto `id`

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/django-stubs/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/__init__.pyi`:

```pyi
from django.db.models.base import Model as Model
from django.db.models.fields import Field as Field, CharField as CharField
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/base.pyi`:

```pyi
from typing import Any

class Model:
    pk: Any
    def __init__(self, *args: Any, **kwargs: Any) -> None: ...
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/fields/__init__.pyi`:

```pyi
from typing import Any, Generic, TypeVar

_ST = TypeVar("_ST", contravariant=True)
_GT = TypeVar("_GT", covariant=True)

class Field(Generic[_ST, _GT]):
    _pyi_private_set_type: Any
    _pyi_private_get_type: Any
    def __init__(
        self,
        *args: Any,
        primary_key: bool = False,
        null: bool = False,
        **kwargs: Any,
    ) -> None: ...
    def __set__(self, instance: Any, value: _ST) -> None: ...
    def __get__(self, instance: Any, owner: Any) -> _GT: ...

class CharField(Field[_ST, _GT]):
    _pyi_private_set_type: str | int
    _pyi_private_get_type: str
```

`main.py`:

```py
from django.db import models

class Sku(models.Model):
    code = models.CharField(primary_key=True)

sku = Sku()
reveal_type(sku.pk)  # revealed: str

# error: [unresolved-attribute] "Object of type `Sku` has no attribute `id`"
sku.id
```

## Constructor synthesis and abstract field inheritance

The synthesized `__init__` is keyword-only with every parameter optional. Abstract models contribute
fields to concrete subclasses through the ordinary MRO walk, but get no synthesized members
themselves.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/django-stubs/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/__init__.pyi`:

```pyi
from django.db.models.base import Model as Model
from django.db.models.fields import Field as Field, CharField as CharField
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/base.pyi`:

```pyi
from typing import Any

class Model:
    pk: Any
    def __init__(self, *args: Any, **kwargs: Any) -> None: ...
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/fields/__init__.pyi`:

```pyi
from typing import Any, Generic, TypeVar

_ST = TypeVar("_ST", contravariant=True)
_GT = TypeVar("_GT", covariant=True)

class Field(Generic[_ST, _GT]):
    _pyi_private_set_type: Any
    _pyi_private_get_type: Any
    def __init__(
        self,
        *args: Any,
        primary_key: bool = False,
        null: bool = False,
        **kwargs: Any,
    ) -> None: ...
    def __set__(self, instance: Any, value: _ST) -> None: ...
    def __get__(self, instance: Any, owner: Any) -> _GT: ...

class CharField(Field[_ST, _GT]):
    _pyi_private_set_type: str | int
    _pyi_private_get_type: str
```

`main.py`:

```py
from django.db import models

class Timestamped(models.Model):
    label = models.CharField()

    class Meta:
        abstract = True

class Article(Timestamped):
    title = models.CharField()

# revealed: (self: Article, *, label: str | int = ..., title: str | int = ..., pk: int | None = ...) -> None
reveal_type(Article.__init__)

article = Article(title="t")
reveal_type(article.id)  # revealed: int

# the abstract model keeps the stubs' gradual fallbacks
reveal_type(Timestamped.__init__)  # revealed: (self: Timestamped, *, label: str | int = ..., pk: int | None = ...) -> None
reveal_type(Timestamped().pk)  # revealed: Any

# error: [unknown-argument]
Article(unknown="x")
```

## Missing stubs diagnostic

Django ships no inline type annotations. When it resolves from its untyped runtime package because
the `django-stubs` PEP 561 package is not installed, the import gets a `missing-framework-stubs`
warning naming the install command.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/django/__init__.py`:

```py
```

`/.venv/<path-to-site-packages>/django/db/__init__.py`:

```py
```

`/.venv/<path-to-site-packages>/django/db/models/__init__.py`:

```py
class Model: ...
```

`main.py`:

```py
# error: [missing-framework-stubs] "Types for `django` are incomplete without the `django-stubs` package"
from django.db import models

# error: [missing-framework-stubs]
import django.db.models
```

## A first-party module named like the framework

A first-party module that happens to be called `django` is not the framework, and gets no warning:

`django/__init__.py`:

```py
```

`django/db.py`:

```py
value: int = 1
```

`main.py`:

```py
from django.db import value

reveal_type(value)  # revealed: int
```

## Reverse accessors (same module)

Reverse accessors come from the to-one relation fields of models in the same module: the literal
`related_name`, or `<source>_set` (bare `<source>` for one-to-one).

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/django-stubs/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/__init__.pyi`:

```pyi
from django.db.models.base import Model as Model
from django.db.models.fields import Field as Field, IntegerField as IntegerField
from django.db.models.fields.related import ForeignKey as ForeignKey, OneToOneField as OneToOneField
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/base.pyi`:

```pyi
from typing import Any

class Model:
    pk: Any
    def __init__(self, *args: Any, **kwargs: Any) -> None: ...
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/fields/__init__.pyi`:

```pyi
from typing import Any, Generic, TypeVar

_ST = TypeVar("_ST", contravariant=True)
_GT = TypeVar("_GT", covariant=True)

class Field(Generic[_ST, _GT]):
    _pyi_private_set_type: Any
    _pyi_private_get_type: Any
    def __init__(
        self,
        *args: Any,
        primary_key: bool = False,
        null: bool = False,
        **kwargs: Any,
    ) -> None: ...
    def __set__(self, instance: Any, value: _ST) -> None: ...
    def __get__(self, instance: Any, owner: Any) -> _GT: ...

class IntegerField(Field[_ST, _GT]):
    _pyi_private_set_type: int | str
    _pyi_private_get_type: int
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/fields/related.pyi`:

```pyi
from typing import Any

from django.db.models.fields import Field, _ST, _GT

class ForeignKey(Field[_ST, _GT]):
    _pyi_private_set_type: Any
    _pyi_private_get_type: Any
    def __init__(
        self,
        to: Any,
        *args: Any,
        related_name: str | None = None,
        primary_key: bool = False,
        null: bool = False,
        **kwargs: Any,
    ) -> None: ...

class OneToOneField(ForeignKey[_ST, _GT]): ...
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/fields/related_descriptors.pyi`:

```pyi
from typing import Generic, TypeVar

_To = TypeVar("_To")

class RelatedManager(Generic[_To]):
    def most_recent(self) -> _To: ...
```

`main.py`:

```py
from django.db import models

class Author(models.Model):
    name = models.IntegerField()

class Book(models.Model):
    author = models.ForeignKey(Author)
    editor = models.ForeignKey(Author, related_name="edited")
    proofreader = models.ForeignKey(Author, related_name="+")

class Profile(models.Model):
    author = models.OneToOneField(Author)

author = Author()
reveal_type(author.book_set)  # revealed: RelatedManager[Book]
reveal_type(author.book_set.most_recent())  # revealed: Book
reveal_type(author.edited)  # revealed: RelatedManager[Book]
reveal_type(author.profile)  # revealed: Profile

# error: [unresolved-attribute]
author.proofreader_set
```

## Lookup / create / field-name validation

The queryset DSL is validated against the field list: unknown fields error, known lookups check
their operand type, and unrecognized lookups/relations are skipped conservatively.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/django-stubs/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/__init__.pyi`:

```pyi
from django.db.models.base import Model as Model
from django.db.models.fields import Field as Field, IntegerField as IntegerField, CharField as CharField
from django.db.models.fields.related import ForeignKey as ForeignKey
from django.db.models.manager import Manager as Manager
from django.db.models.query import QuerySet as QuerySet
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/base.pyi`:

```pyi
from typing import Any, ClassVar
from typing_extensions import Self

from django.db.models.manager import Manager

class Model:
    pk: Any
    objects: ClassVar[Manager[Self]]
    def __init__(self, *args: Any, **kwargs: Any) -> None: ...
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/fields/__init__.pyi`:

```pyi
from typing import Any, Generic, TypeVar

_ST = TypeVar("_ST", contravariant=True)
_GT = TypeVar("_GT", covariant=True)

class Field(Generic[_ST, _GT]):
    _pyi_private_set_type: Any
    _pyi_private_get_type: Any
    def __init__(self, *args: Any, primary_key: bool = False, null: bool = False, choices: Any = None, **kwargs: Any) -> None: ...
    def __set__(self, instance: Any, value: _ST) -> None: ...
    def __get__(self, instance: Any, owner: Any) -> _GT: ...

class IntegerField(Field[_ST, _GT]):
    _pyi_private_set_type: int | str
    _pyi_private_get_type: int

class CharField(Field[_ST, _GT]):
    _pyi_private_set_type: str | int
    _pyi_private_get_type: str
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/fields/related.pyi`:

```pyi
from typing import Any
from django.db.models.fields import Field, _ST, _GT

class ForeignKey(Field[_ST, _GT]):
    _pyi_private_set_type: Any
    _pyi_private_get_type: Any
    def __init__(self, to: Any, *args: Any, null: bool = False, **kwargs: Any) -> None: ...
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/manager.pyi`:

```pyi
from typing import Generic, TypeVar
from django.db.models.query import QuerySet

_T = TypeVar("_T")

class Manager(Generic[_T]):
    def filter(self, *args: object, **kwargs: object) -> QuerySet[_T, _T]: ...
    def get(self, *args: object, **kwargs: object) -> _T: ...
    def create(self, **kwargs: object) -> _T: ...
    def order_by(self, *field_names: str) -> QuerySet[_T, _T]: ...
    def values(self, *fields: str, **expressions: object) -> QuerySet[_T, dict[str, object]]: ...
    def values_list(self, *fields: str, flat: bool = False, named: bool = False) -> QuerySet[_T, object]: ...
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/query.pyi`:

```pyi
from typing import Generic, TypeVar

_Model = TypeVar("_Model")
_Row = TypeVar("_Row")

class QuerySet(Generic[_Model, _Row]):
    def filter(self, *args: object, **kwargs: object) -> QuerySet[_Model, _Row]: ...
    def get(self, *args: object, **kwargs: object) -> _Model: ...
    def order_by(self, *field_names: str) -> QuerySet[_Model, _Row]: ...
```

`main.py`:

```py
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)
    age = models.IntegerField()
    status = models.CharField(choices=[("a", "A")])

class Book(models.Model):
    author = models.ForeignKey(Author)

# valid
Author.objects.filter(name="x")
Author.objects.filter(age__gt=3)
Author.objects.filter(pk=1)
Author.objects.filter(name=None)
Book.objects.filter(author__name="x")
Author.objects.create(name="x", age=3)
Author.objects.order_by("-age")

author = Author.objects.get(pk=1)
reveal_type(author.get_status_display())  # revealed: str

# values() / values_list() row types refined from the literal fields
reveal_type(Author.objects.values_list("name", "age"))  # revealed: QuerySet[Author, tuple[str, int]]
reveal_type(Author.objects.values_list("age", flat=True))  # revealed: QuerySet[Author, int]
reveal_type(Author.objects.values("name", "age"))  # revealed: QuerySet[Author, dict[str, str | int]]
reveal_type(Book.objects.values_list("author", flat=True))  # revealed: QuerySet[Book, int]

# error: [invalid-field-lookup] "Model `Author` has no field `nam`"
Author.objects.filter(nam="x")

# error: [invalid-field-lookup] "Value for `name__startswith` has type `Literal[1]`, but `Author` expects `str | None`"
Author.objects.filter(name__startswith=1)

# error: [invalid-field-lookup] "Model `Author` has no field `nope`"
Author.objects.create(nope="x")

# error: [invalid-field-lookup] "Model `Author` has no field `bogus`"
Author.objects.order_by("bogus")

# error: [invalid-field-lookup] "Model `Author` has no field `missing` (in lookup `author__missing`)"
Book.objects.filter(author__missing="x")
```

## Many-to-many reverse accessors

The reverse of a `ManyToManyField` is a `ManyRelatedManager` on the target model, honoring
`related_name`.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/django-stubs/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/__init__.pyi`:

```pyi
from django.db.models.base import Model as Model
from django.db.models.fields import Field as Field, CharField as CharField
from django.db.models.fields.related import ManyToManyField as ManyToManyField
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/base.pyi`:

```pyi
from typing import Any

class Model:
    pk: Any
    def __init__(self, *args: Any, **kwargs: Any) -> None: ...
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/fields/__init__.pyi`:

```pyi
from typing import Any, Generic, TypeVar

_ST = TypeVar("_ST", contravariant=True)
_GT = TypeVar("_GT", covariant=True)

class Field(Generic[_ST, _GT]):
    _pyi_private_set_type: Any
    _pyi_private_get_type: Any
    def __init__(self, *args: Any, null: bool = False, **kwargs: Any) -> None: ...
    def __set__(self, instance: Any, value: _ST) -> None: ...
    def __get__(self, instance: Any, owner: Any) -> _GT: ...

class CharField(Field[_ST, _GT]):
    _pyi_private_set_type: str | int
    _pyi_private_get_type: str
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/fields/related.pyi`:

```pyi
from typing import Any, Generic, TypeVar
from django.db.models.fields import Field
from django.db.models.fields.related_descriptors import ManyRelatedManager

_To = TypeVar("_To")
_Through = TypeVar("_Through")

class ManyToManyField(Field[Any, Any], Generic[_To, _Through]):
    def __init__(self, to: Any, *args: Any, related_name: str | None = None, **kwargs: Any) -> None: ...
    def __get__(self, instance: Any, owner: Any) -> ManyRelatedManager[_To, _Through]: ...
```

`/.venv/<path-to-site-packages>/django-stubs/db/models/fields/related_descriptors.pyi`:

```pyi
from typing import Generic, TypeVar

_To = TypeVar("_To")
_Through = TypeVar("_Through")

class ManyRelatedManager(Generic[_To, _Through]):
    def count(self) -> int: ...
```

`main.py`:

```py
from django.db import models

class Author(models.Model):
    name = models.CharField()

class Book(models.Model):
    contributors = models.ManyToManyField(Author)
    editors = models.ManyToManyField(Author, related_name="edited")

author = Author()
reveal_type(author.book_set)  # revealed: ManyRelatedManager[Book, Unknown]
reveal_type(author.book_set.count())  # revealed: int
reveal_type(author.edited)  # revealed: ManyRelatedManager[Book, Unknown]
```
