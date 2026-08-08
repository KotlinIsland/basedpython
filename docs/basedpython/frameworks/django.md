# django support

basedpython supports django models. the type checker understands django's unannotated field syntax and synthesizes precise constructor and member types.

## what works

### model field types

- unannotated field assignments like `name = CharField(max_length=100)` are understood: the field type is inferred from the descriptor
- `null=True` correctly widens field types to include `None`
- field access on instances: `author.name` → `str`, `author.birthday` → `date | None`
- field access on the class: `Author.name` → the descriptor type

### model construction

- `Author(name="Alice", birthday=None)` constructor checks correctly
- all constructor parameters are optional (django enforces constraints at save time, not construction)
- foreign key fields and foreign key attnames are all constructor keywords: `Book(author_id=1)` and `Book(author=author_instance)`
- reverse accessors: `author.book_set` resolves to `RelatedManager[Book]`
- primary keys: `id` is auto-synthesized; `pk` is an alias

### queries

- queryset API: `filter()`, `get()`, `exclude()`, `create()`, `all()` are typed
- lookup kwargs are validated: `filter(name__startswith="x")` checks that `name` is a field and `startswith` is a valid lookup for strings
- relation traversal in lookups: `filter(author__birthday__lt=date(2000, 1, 1))` follows foreign keys and checks field types

### class-body field-name lists

several django and drf constructs spell model field paths as a class-body list of plain strings, which the stubs type as `list[str]`. each literal entry is resolved against the model the declaring class names:

- a model's `Meta.ordering` — `order_by` syntax, so a leading `-` and `?` are legal. django reports a bad entry itself, as `models.E015`
- a drf `ModelSerializer`'s / a `ModelForm`'s `Meta.fields` and `Meta.exclude`, against `Meta.model`. drf builds a read-only property field for a non-field attribute, so a model method, a property, `pk` and an `<fk>_id` attname are all legal, as is a field the serializer declares itself. `"__all__"` is a sentinel
- a drf view's `ordering_fields`, `search_fields` and `filterset_fields`, against the model its own `queryset` is a queryset of. a `search_fields` entry may carry one of `SearchFilter`'s `^`, `=`, `@`, `$` prefixes

nothing is reported unless the model is certain. a serializer without a `Meta.model`, a view that builds its queryset in `get_queryset`, and a list holding any non-literal element are all left alone

`source="author.name"` on a serializer field is deliberately **not** checked: drf resolves it with `getattr` at serialization time against whatever object it is handed, which is routinely an annotated row or a dict rather than an instance of `Meta.model`

### transpilation compatibility

- `optional?.chaining` works across nullable relations: `author?.birthday?.year`
- lazy imports work correctly with model registration
- soundness checks work in model methods

## required setup

django has no type annotations, so you need `django-stubs`. install it as a dev dependency:

```sh
uv add django-stubs
```

basedpython will notify you if django is installed without django-stubs.

## limitations and workarounds

### custom field types

custom fields without explicit types degrade gracefully — they'll be `Unknown` to the type checker. you can annotate them:

```by
from django.db import models

class CustomField(models.Field)

class MyModel(models.Model):
    custom: CustomField = CustomField()  # type: CustomType
```

### cross-module reverse accessors

reverse accessors only work within the same module. if you define `Book` in `app/models/book.py` and `Author` in `app/models/author.py`, then `author.book_set` won't resolve. move related models to the same file, or annotate the accessor:

```by
class Author(models.Model):
    ...
    book_set: RelatedManager[Book]  # explicit annotation
```

### dynamic model construction

models created dynamically (e.g., via `type()` or factories) don't check. stick to class definitions.

### querysets from `values()` and `annotate()`

the return types of `values()`, `values_list()`, and `annotate()` aren't fully precise — they check at the generic level but don't know the specific fields returned.

## incompatible patterns

**`init` shorthand or `data class` modifier**

django uses a metaclass for instrumentation. declaring your own `__init__` or stacking `@dataclass` conflicts with this and breaks at runtime. you'll get an error:

```by
class User(models.Model):
    name = models.CharField(max_length=100)

    init(name: str):  # error: conflicts with django's __init__
        self.name = name
```

**use:** let django synthesize the constructor. custom initialization logic goes in `__init__` defined with a normal `def` statement if needed (though this is rarely necessary).

## examples

basic model:

```by
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)
    email = models.EmailField(null=True)

    class Meta:
        ordering = ["name"]
```

foreign keys and relationships:

```by
class Book(models.Model):
    title = models.CharField(max_length=200)
    author = models.ForeignKey(Author, on_delete=models.CASCADE)
    published = models.DateField(null=True)

    class Meta:
        unique_together = ["title", "author"]

book = Book(title="Example", author=author_instance)  # checks correctly
```

queries with lookup validation:

```by
def get_recent_books_by_author(author: Author) -> list[Book]:
    return list(Book.objects.filter(
        author=author,
        published__year=2024,
        title__icontains="python"
    ))  # all lookups validated
```

optional chaining in views:

```by
def get_author_email(book_id: int) -> str | None:
    book = Book.objects.get(id=book_id)
    return book.author?.email  # checks correctly
```

## templates

django template files get their own language support in the editor — completions, go-to-definition and semantic highlighting, all joined up with the models, views and urls described here. see [django templates](django-templates.md).

## see also

- [django templates](django-templates.md) — editor support for template files
- [django documentation](https://docs.djangoproject.com/)
- [django-stubs](https://github.com/typeddjango/django-stubs)
- framework compatibility matrix in the [frameworks overview](index.md#basedpython-features-and-framework-compatibility)
