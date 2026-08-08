# Django

Types come from the `django-stubs` PEP 561 package. The stubs are designed around their mypy plugin;
these tests record what works plugin-free, and what ty's dedicated django support re-derives (see
`docs/basedpython/frameworks/django.md`).

```toml
[environment]
python-version = "3.13"
python-platform = "linux"

[project]
dependencies = ["django==6.0.7", "django-stubs==6.0.7"]
```

## Model definition and manager

```py
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)

reveal_type(Author.objects)  # revealed: Manager[Author]
reveal_type(Author.objects.get(pk=1))  # revealed: Author
reveal_type(Author.objects.filter(name="x"))  # revealed: QuerySet[Author, Author]
reveal_type(Author.objects.first())  # revealed: Author | None

author = Author.objects.get(pk=1)
reveal_type(author.save())  # revealed: None
reveal_type(Author.DoesNotExist)  # revealed: type[ObjectDoesNotExist]
reveal_type(Author.MultipleObjectsReturned)  # revealed: type[MultipleObjectsReturned]
```

## Field descriptor reads

Unannotated field assignments get their descriptor specialization pinned at the constructor call
(from the stubs' own `_pyi_private_set_type` / `_pyi_private_get_type` markers), so instance reads
resolve through the ordinary descriptor protocol:

```py
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)
    age = models.IntegerField()
    birthday = models.DateField(null=True)

reveal_type(Author.name)  # revealed: _FieldDescriptor[CharField[str | int | Combinable, str]]

author = Author.objects.get(pk=1)
reveal_type(author.name)  # revealed: str
reveal_type(author.age)  # revealed: int
reveal_type(author.birthday)  # revealed: date | None
```

## Field descriptor writes

The pinned specialization also drives `__set__` checking:

```py
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)
    birthday = models.DateField(null=True)

author = Author.objects.get(pk=1)
author.name = "x"
author.name = 3  # coerced by django
author.birthday = None

# error: [invalid-assignment]
author.name = None
```

## Foreign keys

The `to=` model is substituted for the dynamic parts of the relation field's marker types:

```py
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)

class Book(models.Model):
    author = models.ForeignKey(Author, on_delete=models.CASCADE)
    editor = models.ForeignKey(Author, on_delete=models.CASCADE, null=True)
    contributors = models.ManyToManyField(Author)

book = Book.objects.get(pk=1)
reveal_type(book.author)  # revealed: Author
reveal_type(book.editor)  # revealed: Author | None
reveal_type(book.contributors)  # revealed: ManyRelatedManager[Author, Unknown]
reveal_type(book.contributors.all())  # revealed: QuerySet[Author, Author]

author = Author.objects.get(pk=1)
book.author = author

# error: [invalid-assignment]
book.author = 1
```

## Dynamic field facts degrade gracefully

A string `to=` (lazy reference) or a non-literal `null=` cannot be resolved statically; the field
keeps its unpinned `Unknown` specialization instead of guessing:

```py
from django.db import models

def flag() -> bool:
    return True

class Author(models.Model):
    name = models.CharField(max_length=100, null=flag())

class Book(models.Model):
    author = models.ForeignKey("Author", on_delete=models.CASCADE)

author = Author.objects.get(pk=1)
book = Book.objects.get(pk=1)
reveal_type(author.name)  # revealed: Unknown
reveal_type(book.author)  # revealed: Unknown
```

## Primary keys

The auto `id` primary key, the `pk` alias, and per-field attnames like `author_id` are added by the
model metaclass at runtime; ty synthesizes them from the model's field list:

```py
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)

class Book(models.Model):
    author = models.ForeignKey(Author, on_delete=models.CASCADE)
    editor = models.ForeignKey(Author, on_delete=models.CASCADE, null=True)

author = Author.objects.get(pk=1)
reveal_type(author.pk)  # revealed: int
reveal_type(author.id)  # revealed: int

book = Book.objects.get(pk=1)
reveal_type(book.author_id)  # revealed: int
reveal_type(book.editor_id)  # revealed: int | None
```

An explicit `primary_key=True` field replaces the auto `id`, and `pk` aliases its type:

```py
from django.db import models

class Sku(models.Model):
    code = models.CharField(max_length=10, primary_key=True)

sku = Sku.objects.get(pk="x")
reveal_type(sku.pk)  # revealed: str

# error: [unresolved-attribute] "Object of type `Sku` has no attribute `id`"
sku.id
```

## Constructor

The stubs declare `Model.__init__` as `(*args: Any, **kwargs: Any)`; ty synthesizes a precise
keyword-only signature from the field list instead. Every parameter is optional — requiredness is a
`full_clean`/`save` concern — and the `pk` alias and fk attnames are accepted alongside the field
names. Many-to-many values cannot be passed at construction.

```py
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)
    birthday = models.DateField(null=True)

class Book(models.Model):
    title = models.TextField()
    author = models.ForeignKey(Author, on_delete=models.CASCADE)
    contributors = models.ManyToManyField(Author)

# revealed: (self: Author, *, name: str | int | Combinable = ..., birthday: str | date | Combinable | None = ..., pk: int | None = ...) -> None
reveal_type(Author.__init__)

author = Author(name="x")
reveal_type(author)  # revealed: Author

book = Book(title="t", author=author, author_id=1)

# error: [unknown-argument] "Argument `nom` does not match any known parameter"
Author(nom="typo")

# error: [unknown-argument] "Argument `contributors` does not match any known parameter"
Book(contributors=[author])
```

## Reverse accessors

`author.book_set` is defined by `Book`, not `Author`: ty synthesizes reverse accessors from the
to-one relation fields of models in the *same module* (the standard `models.py` layout).
Cross-module relations degrade to unresolved attributes.

```py
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)

class Book(models.Model):
    author = models.ForeignKey(Author, on_delete=models.CASCADE)
    editor = models.ForeignKey(Author, on_delete=models.CASCADE, related_name="edited")
    proofreader = models.ForeignKey(Author, on_delete=models.CASCADE, related_name="+")

class Profile(models.Model):
    author = models.OneToOneField(Author, on_delete=models.CASCADE)

author = Author.objects.get(pk=1)
reveal_type(author.book_set)  # revealed: RelatedManager[Book]
reveal_type(author.book_set.all())  # revealed: QuerySet[Book, Book]
reveal_type(author.edited)  # revealed: RelatedManager[Book]
reveal_type(author.profile)  # revealed: Profile

# `related_name="+"` disables the reverse accessor
# error: [unresolved-attribute]
author.proofreader_set
```

## Lookup, create, and field-name validation

The queryset API accepts `**kwargs` in the stubs; ty validates lookup keys, `create()` keywords, and
literal field-name arguments against the model's fields — the plugin's biggest module, re-derived
statically. Unknown lookups, relations to unresolved models, and non-literal keys are skipped rather
than risk a false positive.

```py
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)
    age = models.IntegerField()

class Book(models.Model):
    title = models.CharField(max_length=100)
    author = models.ForeignKey(Author, on_delete=models.CASCADE)

# valid lookups
Author.objects.filter(name="x")
Author.objects.filter(name__startswith="x")
Author.objects.filter(age__gt=3)
Author.objects.filter(pk=1)
Author.objects.filter(name__isnull=True)
Author.objects.filter(name=None)
Book.objects.filter(author__name="x")
Book.objects.filter(author__isnull=True)
Author.objects.create(name="x", age=3)
Author.objects.order_by("name", "-age")
Book.objects.order_by("author__name")
Author.objects.values("name")

# error: [invalid-field-lookup] "Model `Author` has no field `nam`"
Author.objects.filter(nam="x")

# error: [invalid-field-lookup] "Value for `name__startswith` has type `Literal[1]`, but `Author` expects `str | None`"
Author.objects.filter(name__startswith=1)

# error: [invalid-field-lookup] "Model `Author` has no field `nom`"
Author.objects.create(nom="typo")

# error: [invalid-field-lookup] "Model `Author` has no field `bogus`"
Author.objects.order_by("bogus")

# error: [invalid-field-lookup] "Model `Author` has no field `nonfield` (in lookup `author__nonfield`)"
Book.objects.filter(author__nonfield="x")
```

## The `*_or_create` family's own keywords

`defaults` and `create_defaults` carry the values to apply on create; they are django's own
keywords, not lookups, so they name no field and are not resolved as one.

```py
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)
    age = models.IntegerField()

Author.objects.get_or_create(name="x", defaults={"age": 3})
Author.objects.update_or_create(name="x", defaults={"age": 3})
Author.objects.update_or_create(name="x", create_defaults={"age": 3}, defaults={})

async def create_one() -> None:
    await Author.objects.aget_or_create(name="x", defaults={"age": 3})

# the lookup keys beside them are still checked
# error: [invalid-field-lookup] "Model `Author` has no field `nam`"
Author.objects.get_or_create(nam="x", defaults={"age": 3})

# and `defaults` is not a keyword of the plain lookup methods
# error: [invalid-field-lookup] "Model `Author` has no field `defaults`"
Author.objects.filter(defaults={"age": 3})
```

## Model `Meta.ordering`

`Meta.ordering` holds `order_by` syntax — a leading `-` and the `?` sentinel are both legal. Django
resolves it at import time and reports a bad entry as `models.E015`; the stubs type it as plain
`list[str]`, so ty resolves each literal entry against the model declaring the `Meta`.

```py
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)

class Book(models.Model):
    title = models.CharField(max_length=100)
    published = models.DateField()
    author = models.ForeignKey(Author, on_delete=models.CASCADE)

    class Meta:
        ordering = ["-published", "title", "pk", "?", "author__name", "author_id", "author"]

class Bad(models.Model):
    title = models.CharField(max_length=100)

    class Meta:
        ordering = [
            "title",
            # error: [invalid-field-lookup] "Model `Bad` has no field `nope` (in `ordering`)"
            "nope",
            # error: [invalid-field-lookup] "Model `Bad` has no field `alsonope` (in `ordering`)"
            "-alsonope",
        ]
```

## `Meta.ordering` refusals

Nothing is reported when the list cannot be read exhaustively, when the `Meta` belongs to something
that is not a model, or when `ordering` is written outside a `Meta`.

```py
from django.db import models

NAMES = ["nope"]

class Book(models.Model):
    title = models.CharField(max_length=100)

    class Meta:
        # a name, not a literal list — its elements are unknown here
        ordering = NAMES

class Dynamic(models.Model):
    title = models.CharField(max_length=100)

    class Meta:
        # one non-literal element makes the whole list unverifiable
        ordering = ["nope", *NAMES]

class Annotated(models.Model):
    title = models.CharField(max_length=100)

    class Meta:
        # an annotation changes nothing about the entries
        # error: [invalid-field-lookup] "Model `Annotated` has no field `nope` (in `ordering`)"
        ordering: list[str] = ["title", "nope"]

class NotAMeta(models.Model):
    title = models.CharField(max_length=100)

    class Config:
        ordering = ["nope"]

class Plain:
    class Meta:
        ordering = ["nope"]

class AlsoNotChecked(models.Model):
    title = models.CharField(max_length=100)
    # `ordering` on the model itself is not django's `Meta.ordering`
    ordering = ["nope"]
```

## get_FOO_display for choice fields

A field declared with `choices=` gets a synthesized `get_<field>_display()` method; a field without
`choices=` does not.

```py
from django.db import models

class Article(models.Model):
    STATUS = [("d", "Draft"), ("p", "Published")]
    status = models.CharField(max_length=1, choices=STATUS)
    title = models.CharField(max_length=100)

article = Article.objects.get(pk=1)
reveal_type(article.get_status_display())  # revealed: str

# error: [unresolved-attribute] "Object of type `Article` has no attribute `get_title_display`"
article.get_title_display()
```

## Many-to-many reverse accessors

The reverse side of a `ManyToManyField` is synthesized like a foreign-key reverse accessor, but as a
`ManyRelatedManager`.

```py
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)

class Book(models.Model):
    title = models.CharField(max_length=100)
    contributors = models.ManyToManyField(Author)
    editors = models.ManyToManyField(Author, related_name="edited_books")

author = Author.objects.get(pk=1)
reveal_type(author.book_set)  # revealed: ManyRelatedManager[Book, Unknown]
reveal_type(author.book_set.all())  # revealed: QuerySet[Book, Book]
reveal_type(author.edited_books)  # revealed: ManyRelatedManager[Book, Unknown]
```

## values() / values_list() row types

The stubs type these as `QuerySet[Model, dict[str, Any]]` / `QuerySet[Model, Any]`; ty refines the
row type from the literal field arguments (a bare relation reads as its target pk, `flat=True`
unwraps a single field). `named=True`, `values_list()` with no fields, and any non-literal field
fall back to the stub type.

```py
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)
    age = models.IntegerField()

class Book(models.Model):
    title = models.CharField(max_length=100)
    author = models.ForeignKey(Author, on_delete=models.CASCADE)

reveal_type(Author.objects.values_list("name", "age"))  # revealed: QuerySet[Author, tuple[str, int]]
reveal_type(Author.objects.values_list("name", flat=True))  # revealed: QuerySet[Author, str]
reveal_type(Author.objects.values("name", "age"))  # revealed: QuerySet[Author, dict[str, str | int]]
reveal_type(Book.objects.values_list("author__name", flat=True))  # revealed: QuerySet[Book, str]
reveal_type(Book.objects.values_list("author", flat=True))  # revealed: QuerySet[Book, int]

# not refined — fall back to the stub row type
reveal_type(Author.objects.values_list("name", named=True))  # revealed: QuerySet[Author, Any]
reveal_type(Author.objects.values_list())  # revealed: QuerySet[Author, Any]
```

## Settings the project's settings module names

`django.conf.settings` answers every attribute through `__getattr__`, so the stubs can only say
`Any`. The project's settings module is ordinary python sitting in the project, and `manage.py` says
which module it is, so a setting the module assigns is read off it.

Only what django itself copies is read: a name is a setting when `name.isupper()`, which is the test
django applies to the module's contents.

`manage.py`:

```py
import os

os.environ.setdefault("DJANGO_SETTINGS_MODULE", "conf.settings")
```

`conf/__init__.py`:

```py
```

`conf/settings.py`:

```py
DEBUG = True
ROOT_URLCONF = "conf.urls"
SESSION_COOKIE_AGE = 1209600
MY_CUSTOM = "anything the project likes"
lowercase = "not a setting"
```

`app.py`:

```py
from django.conf import settings

reveal_type(settings.DEBUG)  # revealed: bool
reveal_type(settings.ROOT_URLCONF)  # revealed: str
reveal_type(settings.SESSION_COOKIE_AGE)  # revealed: int
reveal_type(settings.MY_CUSTOM)  # revealed: str

# nothing names it, so it stays what the stubs say
reveal_type(settings.NOT_A_SETTING_ANYWHERE)  # revealed: Any

# django copies only upper-case names off the module, so this is not a setting
reveal_type(settings.lowercase)  # revealed: Any

# what the stubs declare outright still wins
reveal_type(settings.SETTINGS_MODULE)  # revealed: str
```

## Settings whose value describes only one deployment

A container's inferred element types come from the literal this deployment happens to hold, and
django's contract is that anything may read and write keys the literal never mentions —
`settings.DATABASES[alias]["TEST"]["USER"]` is django's own code. Such a setting keeps the `Any` the
stubs give it rather than a type that is narrower than the setting really is.

`manage.py`:

```py
import os

os.environ.setdefault("DJANGO_SETTINGS_MODULE", "conf.settings")
```

`conf/__init__.py`:

```py
```

`conf/settings.py`:

```py
from pathlib import Path

BASE_DIR = Path(__file__).resolve().parent.parent
DATABASES = {"default": {"ENGINE": "django.db.backends.sqlite3", "NAME": ":memory:"}}
INSTALLED_APPS = ["django.contrib.auth"]
```

`app.py`:

```py
from django.conf import settings

reveal_type(settings.DATABASES)  # revealed: Any
reveal_type(settings.INSTALLED_APPS)  # revealed: Any

# a value whose type carries no arguments has no such gap
reveal_type(settings.BASE_DIR)  # revealed: Path
```

## No settings module named

A project may configure settings programmatically with `settings.configure()`, or run with
`DJANGO_SETTINGS_MODULE` set in its environment and written down nowhere. Nothing in the project
says what the settings are, so every setting stays `Any`.

`app.py`:

```py
from django.conf import settings

settings.configure(DEBUG=True)

reveal_type(settings.DEBUG)  # revealed: Any
```

## A settings module assembled from another

The split-settings layout — a per-environment module that starts `from .base import *` — is read
through the star import, since that is how the names reach the module django imports.

`manage.py`:

```py
import os

os.environ.setdefault("DJANGO_SETTINGS_MODULE", "conf.local")
```

`conf/__init__.py`:

```py
```

`conf/base.py`:

```py
ROOT_URLCONF = "conf.urls"
SECRET_KEY = "shared"
```

`conf/local.py`:

```py
from .base import *

DEBUG = True
```

`app.py`:

```py
from django.conf import settings

reveal_type(settings.DEBUG)  # revealed: bool
reveal_type(settings.ROOT_URLCONF)  # revealed: str
reveal_type(settings.SECRET_KEY)  # revealed: str
```

## Model fields in a `.by` file

A constructor call infers as `final CharField[…]` in basedpython, so everything a model's fields
drive has to read through the use-site modifier to see the class the call constructs.

```by
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)

author = Author.objects.get(pk=1)
reveal_type(author.name)  # revealed: str
Author.objects.filter(name="x")

# error: [invalid-field-lookup] "Model `Author` has no field `nam`"
Author.objects.filter(nam="x")
```

## Lookups written as expressions

basedpython spells the `__` lookup DSL as ordinary expressions. A name in the leading position of a
comparison that resolves nowhere in the lexical chain, and names a field of the model the queryset
is over, is a field path — `author.name` traverses the relation exactly as `author__name` does.

```by
from datetime import date
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)
    age = models.IntegerField()

class Book(models.Model):
    title = models.CharField(max_length=100)
    published = models.DateField()
    author = models.ForeignKey(Author, on_delete=models.CASCADE)

reveal_type(Book.objects.filter(title == "x"))  # revealed: QuerySet[Book, Book]
Book.objects.filter(published > date(1970, 1, 1))
Book.objects.filter(published >= date(1970, 1, 1))
Book.objects.filter(published < date(1970, 1, 1))
Book.objects.filter(published <= date(1970, 1, 1))
Book.objects.filter(title in ["a", "b"])
Book.objects.filter(author.name == "x")
Book.objects.filter(author.age > 3)
Book.objects.filter(pk == 1)
Book.objects.exclude(title == "x")
Book.objects.get(pk == 1)
```

## A lookup path types as the fields it names

The leading name is the field, and a relation traverses into the model it targets, so the segments
after it are ordinary member accesses — including the ones django reads as transforms.

```by
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)

class Book(models.Model):
    title = models.CharField(max_length=100)
    published = models.DateField()
    author = models.ForeignKey(Author, on_delete=models.CASCADE)

Book.objects.filter(author.name == "x")
Book.objects.filter(published.year == 1970)

# a segment after a concrete field is a lookup or a transform, and an unrecognized one is left
# alone rather than reported — exactly as `filter(published__nonmember=1970)` is today, since
# django lets a project register lookups of its own
Book.objects.filter(published.nonmember == 1970)
```

## An unknown segment is reported

```by
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)

class Book(models.Model):
    title = models.CharField(max_length=100)
    author = models.ForeignKey(Author, on_delete=models.CASCADE)

# error: [invalid-field-lookup] "Model `Author` has no field `nonfield` (in lookup `author__nonfield`)"
Book.objects.filter(author.nonfield == "x")
```

## A value the field cannot hold is reported

```by
from django.db import models

class Author(models.Model):
    age = models.IntegerField()

# error: [invalid-field-lookup] "Value for `age__gt` has type `b"x"`, but `Author` expects `int | float | str | Combinable | None`"
Author.objects.filter(age > b"x")
```

## Lexical scope wins

A name bound anywhere in the chain keeps its ordinary meaning, so nothing that resolves today
changes: the comparison is an ordinary `bool` the queryset method takes positionally.

```by
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)

def search(name: str) -> None:
    # `name` is the parameter, not the field
    reveal_type(Author.objects.filter(name == "x"))  # revealed: QuerySet[Author, Author]
```

## A builtin is a lexical binding too

`id` names the builtin, so it is never read as the model's primary key. `pk` is the spelling that
resolves.

```by
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)

# no lookup here — `id` is the builtin function
Author.objects.filter(id == 1)
Author.objects.filter(pk == 1)
```

## Operators with no lookup spelling

`!=` has no `__` form — django writes it as `.exclude(...)` or `~Q(...)`, both of which change the
method being called — so it stays an ordinary comparison and its name stays unresolved.

```by
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)

# error: [unresolved-reference] "Name `name` used when not defined"
Author.objects.filter(name != "x")
```

## Ordinary arguments pass through

```by
from django.db import models
from django.db.models import Q

class Author(models.Model):
    name = models.CharField(max_length=100)
    age = models.IntegerField()

def build(kwargs: dict[str, str], q: Q) -> None:
    Author.objects.filter(Q(name="a") | Q(name="b"))
    Author.objects.filter(q)
    Author.objects.filter(**kwargs)
    Author.objects.filter(name__startswith="x")
    Author.objects.filter(q, age > 3)
```

## A json key is a subscript

A `JSONField` holds arbitrary json, so its `__` segments are object keys rather than a closed set of
lookups. A subscript spells one: `data["key"]` is `data__key`, and what it reads back is json.

```by
from django.db import models

class Doc(models.Model):
    data = models.JSONField()

Doc.objects.filter(data["key"] == 1)
Doc.objects.filter(data["key"] == "x")
reveal_type(Doc.objects.filter(data["key"] == 1))  # revealed: QuerySet[Doc, Doc]
```

## Json keys nest, and the operators still apply

```by
from django.db import models

class Doc(models.Model):
    data = models.JSONField()

# data__a__b=1
Doc.objects.filter(data["a"]["b"] == 1)
# data__key__gt=1
Doc.objects.filter(data["key"] > 1)
Doc.objects.filter(data["key"] >= 1)
Doc.objects.filter(data["key"] < 1)
Doc.objects.filter(data["key"] <= 1)
Doc.objects.filter(data["key"] in [1, 2])
```

## An integer subscript is a json array index

```by
from django.db import models

class Doc(models.Model):
    data = models.JSONField()

# data__0=1
Doc.objects.filter(data[0] == 1)
# data__0__1=1
Doc.objects.filter(data[0][1] == 1)
# data__a__0__gt=1
Doc.objects.filter(data["a"][0] > 1)
```

## A json key on a relation's field

```by
from django.db import models

class Author(models.Model):
    data = models.JSONField()

class Book(models.Model):
    author = models.ForeignKey(Author, on_delete=models.CASCADE)

# author__data__key=1
Book.objects.filter(author.data["key"] == 1)
```

## An arbitrary json key is not an unknown field

Every string is a legal key, so nothing is reported for one — exactly as nothing is reported for the
keyword form's `filter(data__anything=1)`.

```by
from django.db import models

class Doc(models.Model):
    data = models.JSONField()

Doc.objects.filter(data["not_a_field"] == 1)
Doc.objects.filter(data__not_a_field=1)
```

## A subscript on a field with no key transform is left alone

`data["k"]` on a `CharField` is nonsense — django raises `FieldError` for `title__k`. The argument
keeps the meaning it has today, which leaves its leading name unresolved.

```by
from django.db import models

class Book(models.Model):
    title = models.CharField(max_length=100)

# error: [unresolved-reference] "Name `title` used when not defined"
Book.objects.filter(title["k"] == 1)
```

## A subscript on a relation is left alone

```by
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)

class Book(models.Model):
    author = models.ForeignKey(Author, on_delete=models.CASCADE)

# error: [unresolved-reference] "Name `author` used when not defined"
Book.objects.filter(author["k"] == 1)
```

## A json key django reads as a lookup is left alone

Django resolves the last segment of a keyword as a lookup when the field has one by that name, so
`data__gt=1` asks for `data > 1` rather than for the key `"gt"`. There is no keyword that spells
that key, so the subscript is refused rather than lowered to a different query.

```by
from django.db import models

class Doc(models.Model):
    data = models.JSONField()

# error: [unresolved-reference] "Name `data` used when not defined"
Doc.objects.filter(data["gt"] == 1)
```

## A json key with no keyword spelling is left alone

Django reads a segment that parses as an integer as an array index, so the string key `"0"` has no
`__` form of its own. Nor does a key that is not spellable inside an identifier, one holding the
`__` separator, or a negative index.

```by
from django.db import models

class Doc(models.Model):
    data = models.JSONField()

def refusals() -> None:
    # error: [unresolved-reference] "Name `data` used when not defined"
    Doc.objects.filter(data["0"] == 1)
    # error: [unresolved-reference] "Name `data` used when not defined"
    Doc.objects.filter(data["a b"] == 1)
    # error: [unresolved-reference] "Name `data` used when not defined"
    Doc.objects.filter(data["a__b"] == 1)
    # error: [unresolved-reference] "Name `data` used when not defined"
    Doc.objects.filter(data[-1] == 1)
```

## A subscript that is not a literal is left alone

A key held in a variable cannot be read here, and a slice is `ArrayField`'s spelling, which a json
field reads as an index.

```by
from django.db import models

class Doc(models.Model):
    data = models.JSONField()

def refusals(key: str) -> None:
    # error: [unresolved-reference] "Name `data` used when not defined"
    Doc.objects.filter(data[key] == 1)
    # error: [unresolved-reference] "Name `data` used when not defined"
    Doc.objects.filter(data[1:2] == 1)
```

## A dot after a json key is left alone

The dotted part of a path names the field; a dot after a subscript names nothing django spells.

```by
from django.db import models

class Doc(models.Model):
    data = models.JSONField()

# error: [unresolved-reference] "Name `data` used when not defined"
Doc.objects.filter(data["a"].b == 1)
```

## A path segment django would read back as two is left alone

Nothing escapes the `__` separator, so a dunder attribute spells more segments than the path has.

```by
from django.db import models

class Book(models.Model):
    published = models.DateField()

# error: [unresolved-reference] "Name `published` used when not defined"
Book.objects.filter(published.__class__ == 1)
```
