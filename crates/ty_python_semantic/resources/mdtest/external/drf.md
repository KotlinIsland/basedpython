# Django REST framework

Types come from the `djangorestframework-stubs` package. drf declares model field paths as
class-body lists of plain strings, so a typo survives the stubs and only fails at runtime — when the
serializer is first built, or when a request asks for that ordering / search / filter. ty resolves
each literal entry against the model the declaring class names.

```toml
[environment]
python-version = "3.13"
python-platform = "linux"

[project]
dependencies = [
    "django==6.0.7",
    "django-stubs==6.0.7",
    "djangorestframework==3.17.2",
    "djangorestframework-stubs==3.17.1",
]
```

## Serializer `Meta.fields`

Every entry must name something reachable on `Meta.model` — drf builds a read-only property field
for a non-field attribute, so a method, a property, the `pk` alias and an `<fk>_id` attname are all
as valid as a field — or a field the serializer itself declares. `fields = "__all__"` is a sentinel.

```py
from django.db import models
from rest_framework import serializers

class Author(models.Model):
    name = models.CharField(max_length=100)

class Book(models.Model):
    title = models.CharField(max_length=100)
    author = models.ForeignKey(Author, on_delete=models.CASCADE)

    def get_absolute_url(self) -> str:
        return ""

class Everything(serializers.ModelSerializer[Book]):
    declared = serializers.CharField(source="title")

    class Meta:
        model = Book
        # a field, a relation, the pk alias, an attname, a model method, a declared field
        fields = ["title", "author", "pk", "id", "author_id", "get_absolute_url", "declared"]

class AllFields(serializers.ModelSerializer[Book]):
    class Meta:
        model = Book
        fields = "__all__"

class Reverse(serializers.ModelSerializer[Author]):
    class Meta:
        model = Author
        # the reverse accessor django synthesizes for `Book.author`
        fields = ["id", "name", "book_set"]

class Typo(serializers.ModelSerializer[Book]):
    class Meta:
        model = Book
        fields = [
            "title",
            # error: [invalid-field-lookup] "Model `Book` has no field `titel` (in `fields`)"
            "titel",
        ]
```

## Serializer `Meta.exclude`

`exclude` is resolved the same way; drf asserts on an entry that matches no model field.

```py
from django.db import models
from rest_framework import serializers

class Book(models.Model):
    title = models.CharField(max_length=100)
    published = models.DateField()

class Fine(serializers.ModelSerializer[Book]):
    class Meta:
        model = Book
        exclude = ["published"]

class Typo(serializers.ModelSerializer[Book]):
    class Meta:
        model = Book
        # error: [invalid-field-lookup] "Model `Book` has no field `publshed` (in `exclude`)"
        exclude = ["publshed"]
```

## Serializer refusals

Nothing is reported when the model is not certain: no `Meta.model`, a `Meta.model` that is not a
model, or a list that cannot be read exhaustively.

```py
from django.db import models
from rest_framework import serializers

class Book(models.Model):
    title = models.CharField(max_length=100)

class NotAModel: ...

EXTRA = ["nope"]

class NoModel(serializers.ModelSerializer[Book]):
    class Meta:
        fields = ["nope"]

class WrongModel(serializers.ModelSerializer[Book]):
    class Meta:
        model = NotAModel
        fields = ["nope"]

class Dynamic(serializers.ModelSerializer[Book]):
    class Meta:
        model = Book
        fields = ["nope", *EXTRA]

class FromAName(serializers.ModelSerializer[Book]):
    class Meta:
        model = Book
        fields = EXTRA

class PlainSerializer(serializers.Serializer[Book]):
    class Meta:
        model = Book
        fields = ["nope"]
```

## View filter-backend field lists

`ordering_fields`, `filterset_fields` and `search_fields` are resolved against the model the view's
own `queryset` is a queryset of. `search_fields` entries may carry one of `SearchFilter`'s lookup
prefixes (`^`, `=`, `@`, `$`), and `"__all__"` is a sentinel.

```py
from django.db import models
from rest_framework import generics

class Author(models.Model):
    name = models.CharField(max_length=100)

class Book(models.Model):
    title = models.CharField(max_length=100)
    published = models.DateField()
    author = models.ForeignKey(Author, on_delete=models.CASCADE)

class Good(generics.ListAPIView[Book]):
    queryset = Book.objects.all()
    ordering_fields = ["published", "author__name"]
    search_fields = ["title", "^title", "=title", "$title", "author__name"]
    filterset_fields = ["title", "author"]

class AllOrdering(generics.ListAPIView[Book]):
    queryset = Book.objects.all()
    ordering_fields = "__all__"

class DictFilterset(generics.ListAPIView[Book]):
    queryset = Book.objects.all()
    filterset_fields = {"title": ["exact", "contains"]}

class Typos(generics.ListAPIView[Book]):
    queryset = Book.objects.all()
    # error: [invalid-field-lookup] "Model `Book` has no field `publishd` (in `ordering_fields`)"
    ordering_fields = ["publishd"]
    # error: [invalid-field-lookup] "Model `Book` has no field `titel` (in `search_fields`)"
    search_fields = ["^titel"]
    # error: [invalid-field-lookup] "Model `Book` has no field `nope` (in `filterset_fields`)"
    filterset_fields = {"nope": ["exact"]}

class TypoAcrossRelation(generics.ListAPIView[Book]):
    queryset = Book.objects.all()
    # error: [invalid-field-lookup] "Model `Author` has no field `nome` (in `ordering_fields`)"
    ordering_fields = ["author__nome"]

class Annotated(generics.ListAPIView[Book]):
    queryset = Book.objects.all()
    ordering_fields: list[str] = ["published"]
    # error: [invalid-field-lookup] "Model `Book` has no field `publishd` (in `search_fields`)"
    search_fields: list[str] = ["publishd"]
```

## View refusals

A view whose queryset cannot be traced to a model names nothing to resolve against. drf removed the
`model` attribute in 3.0, so that is not a source either.

```py
from django.db import models
from rest_framework import generics

class Book(models.Model):
    title = models.CharField(max_length=100)

class FromMethod(generics.ListAPIView[Book]):
    ordering_fields = ["nope"]

    def get_queryset(self):
        return Book.objects.all()

class LegacyModelAttribute(generics.ListAPIView[Book]):
    model = Book
    ordering_fields = ["nope"]

class NotAView:
    queryset = Book.objects.all()
    ordering_fields = ["nope"]
```
