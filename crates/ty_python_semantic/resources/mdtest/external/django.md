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
