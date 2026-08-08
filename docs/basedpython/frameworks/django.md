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

### lookups written as expressions

django's `__` lookups can be written as ordinary expressions:

```by
Book.objects.filter(author.name == "Ursula", published > date(1970, 1, 1))
```

→

```python
Book.objects.filter(author__name="Ursula", published__gt=date(1970, 1, 1))
```

`filter`, `exclude`, `get` and `aget` take them. a path joins with `__`, so `author.name` traverses the relation and `published.year` is django's date transform, and each operator spells its lookup:

| expression | keyword    |
| ---------- | ---------- |
| `a == v`   | `a=v`      |
| `a > v`    | `a__gt=v`  |
| `a >= v`   | `a__gte=v` |
| `a < v`    | `a__lt=v`  |
| `a <= v`   | `a__lte=v` |
| `a in v`   | `a__in=v`  |

the leading name is a field, so it is checked and it has a type: `author` is an `Author`, `published` is a `date`, and a segment that names no field is reported exactly as the keyword form's is. the value is checked against the field the same way too.

a `JSONField` holds arbitrary json rather than fields, so a subscript indexes into one — django's key and index transforms:

```by
Doc.objects.filter(data["key"] == 1, data["a"]["b"] > 2, data[0] == 3)
```

→

```python
Doc.objects.filter(data__key=1, data__a__b__gt=2, data__0=3)
```

a string subscript is an object key, an integer one is an array index, and the operators apply on top of either. every key is legal json, so nothing is reported for one — just as nothing is reported for the keyword form's `filter(data__anything=1)`

resolution is a *last* fallback, so nothing that resolves today changes meaning. a name bound anywhere in the lexical chain — a local, a parameter, a module global, a builtin — keeps it, and the comparison stays an ordinary one the method takes positionally:

```by
def search(title: str):
    Book.objects.filter(title == title)   # the parameter, compared to itself
    Book.objects.filter(id == 1)          # `id` is the builtin — write `pk == 1`
```

everything else passes through untouched: `filter(Q(a=1) | Q(b=2))`, `filter(**kwargs)`, and an already-written `filter(author__name="x")` all mean what they always did.

an expression that cannot be read as a lookup is left exactly as written, which leaves its leading name an `unresolved-reference` — the feature never quietly changes a query. these are the refusals:

- `!=`. django has no `__` spelling for it: it writes `.exclude(a=1)` or `~Q(a=1)`, both of which change the method being called
- a chained comparison (`1 < a < 5`), a reversed one (`"x" == title`), or a call on the left (`title.upper() == "X"`)
- a lookup before an argument that stays positional, and two lookups that would spell the same keyword — python rejects both
- the `*_or_create` family, whose first positional parameter is `defaults`
- a subscript on anything but a `JSONField` — `title["k"]` on a `CharField` is a `FieldError` at runtime. `ArrayField` and `HStoreField` carry key and index transforms too, but they are postgres-only and are not read here yet
- a subscript django cannot be handed as a keyword: a key held in a variable, a slice, a negative index, a key that is not spellable inside an identifier (`data["a b"]`), and one holding the `__` separator
- a numeric *string* key. django tries `int()` on every segment, so `data__0` is the index — `data[0]` asks for it, and `data["0"]` has no spelling of its own
- a key that collides with one of django's lookups on `JSONField`: `exact`, `in`, `isnull`, `range`, the comparisons, the string lookups, and the `has_key` family. django reads the last segment of a keyword as a lookup when the field has one by that name, so `data__gt=1` asks for `data > 1` rather than for the key `"gt"`
- a dot after a subscript (`data["a"].b`). the dotted part of a path names the field; the subscripts index into it

### class-body field-name lists

several django and drf constructs spell model field paths as a class-body list of plain strings, which the stubs type as `list[str]`. each literal entry is resolved against the model the declaring class names:

- a model's `Meta.ordering` — `order_by` syntax, so a leading `-` and `?` are legal. django reports a bad entry itself, as `models.E015`
- a drf `ModelSerializer`'s / a `ModelForm`'s `Meta.fields` and `Meta.exclude`, against `Meta.model`. drf builds a read-only property field for a non-field attribute, so a model method, a property, `pk` and an `<fk>_id` attname are all legal, as is a field the serializer declares itself. `"__all__"` is a sentinel
- a drf view's `ordering_fields`, `search_fields` and `filterset_fields`, against the model its own `queryset` is a queryset of. a `search_fields` entry may carry one of `SearchFilter`'s `^`, `=`, `@`, `$` prefixes

nothing is reported unless the model is certain. a serializer without a `Meta.model`, a view that builds its queryset in `get_queryset`, and a list holding any non-literal element are all left alone

`source="author.name"` on a serializer field is deliberately **not** checked: drf resolves it with `getattr` at serialization time against whatever object it is handed, which is routinely an annotated row or a dict rather than an instance of `Meta.model`

### drf view and serializer specialization

`djangorestframework-stubs` declares the drf bases generic over the model, but real drf code names the model in the class body rather than writing `ListAPIView[Book]`. the model the class body names is substituted for the type argument nobody writes:

- a view's `get_queryset()` and `get_object()`, from the model its own `queryset` is a queryset of
- a view's `get_serializer()` and `get_serializer_class()`, from the `serializer_class` it names
- a `ModelSerializer`'s `save()`, `create()` and `update()`, from `Meta.model`

```by
class BookViewSet(viewsets.ModelViewSet):
    queryset = Book.objects.all()
    serializer_class = BookSerializer

view.get_queryset()      # QuerySet[Book, Book], was QuerySet[Unknown, Unknown]
view.get_serializer()    # BookSerializer, was BaseSerializer[Unknown]
serializer.save()        # Book, was Unknown
```

a view or serializer that *does* write its type argument keeps it — the substitution only fills a position that carries no information today. anything the class body does not say plainly is left alone: a queryset built in `get_queryset`, a `serializer_class` chosen in `get_serializer_class`, a serializer with no `Meta.model`, and your own override of any of these methods

`.data` and `.validated_data` stay `Any` on purpose — see below

### settings

`django.conf.settings` answers every name through `__getattr__`, so the stubs can only say `Any`. your settings module is ordinary python sitting in your project, and `manage.py` says which module it is, so a setting the module assigns is read off it:

```by
from django.conf import settings

settings.ROOT_URLCONF   # str
settings.DEBUG          # bool
settings.MY_OWN         # whatever your settings module assigns it
```

the module comes from the `DJANGO_SETTINGS_MODULE` your own scripts assign — `manage.py` first, then `wsgi.py` or `asgi.py`. nothing is guessed: a project that configures settings with `settings.configure()`, or names the module only in its environment, gets today's `Any` for every setting

a split-settings layout is read through its star import, so `from .base import *` in the module `manage.py` names brings `base`'s settings along

these stay `Any` on purpose:

- a setting whose value is a container. `DATABASES = {"default": {...}}` infers `dict[str, dict[str, str]]` from what one deployment happens to hold, and django's contract is that anything may read and write keys the literal never mentions — `settings.DATABASES[alias]["TEST"]["USER"]` is django's own code
- a setting your module does not assign, even one `django.conf.global_settings` gives a default for. django's own code is written to survive a settings module putting anything in them, and typing them turns that defensive code into errors
- writing a setting. `settings.DEBUG = 1` is what `override_settings` does, and it is checked no more strictly than before

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

### drf `many=True` at the constructor

`BookSerializer(data=[...], many=True)` builds a `ListSerializer` at runtime, so its `save()` returns a *list* of models. the stubs describe none of that — `many` is an ordinary `bool` parameter and the constructed type is the serializer class either way — so `save()` reads as one model:

```by
for book in BookSerializer(data=payload, many=True).save():  # `Book` is not iterable
```

writing `ModelSerializer[Book]` by hand has always read the same way; the model resolved from `Meta.model` just makes it read that way without the annotation. going through the view (`self.get_serializer(data=payload, many=True)`) is not affected — a `many=` argument there is visible, so nothing is claimed about the result

### drf `.data` and `.validated_data`

both stay `Any`. their key sets look like they follow from `Meta.fields`, and they don't:

- `validate()` may add, drop or rename any key, and returning a rewritten dict is the documented way to do cross-field validation
- `save(**kwargs)` merges its keywords into what `create()` receives
- `to_representation()` may add keys to `.data`
- a key is a field's `source`, not its name, and a dotted `source="author.name"` restructures the dict entirely
- `.data` on a serializer built from `data=` holds only the validated subset, while `.data` on one built from an instance holds every field
- `many=True` makes both a list

a `TypedDict` is closed, so every one of these turns correct code into an error. `Any` reports nothing, which is the right answer here

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
