# Django REST framework

Types come from the `djangorestframework-stubs` package. drf declares model field paths as
class-body lists of plain strings, so a typo survives the stubs and only fails at runtime — when the
serializer is first built, or when a request asks for that ordering / search / filter. ty resolves
each literal entry against the model the declaring class names.

`missing-type-argument` is put back to its default level here: real drf code subclasses
`ModelSerializer` and `ListAPIView` without writing the type argument, and what the class body says
instead is the subject of half of these tests.

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

[rules]
missing-type-argument = "ignore"
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

## View model specialization

drf's bases are generic over the model, but real drf code names it in the class body instead of
writing the type argument. The model the view's own `queryset` is a queryset of is substituted for
the `Unknown` the unparameterized base leaves behind.

```py
from django.db import models
from rest_framework import generics, viewsets

class Book(models.Model):
    title = models.CharField(max_length=100)

class Unparameterized(generics.ListAPIView):
    queryset = Book.objects.all()

class ViewSet(viewsets.ModelViewSet):
    queryset = Book.objects.all()

def f(view: Unparameterized, viewset: ViewSet) -> None:
    reveal_type(view.get_queryset())  # revealed: QuerySet[Book, Book]
    reveal_type(view.get_object())  # revealed: Book
    reveal_type(viewset.get_queryset())  # revealed: QuerySet[Book, Book]
    reveal_type(viewset.get_object())  # revealed: Book
    # the resolved model reaches the queryset's own machinery
    reveal_type(view.get_queryset().first())  # revealed: Book | None
    reveal_type(view.get_queryset().values_list("title", flat=True))  # revealed: QuerySet[Book, str]
```

## An explicit type argument is never contradicted

A view that does write the type argument keeps it: the substitution only ever fills a position that
carries no information today.

```py
from django.db import models
from rest_framework import generics

class Author(models.Model):
    name = models.CharField(max_length=100)

class Book(models.Model):
    title = models.CharField(max_length=100)

class Explicit(generics.ListAPIView[Book]):
    queryset = Book.objects.all()

class Mismatched(generics.ListAPIView[Author]):
    queryset = Book.objects.all()

def f(explicit: Explicit, mismatched: Mismatched) -> None:
    reveal_type(explicit.get_queryset())  # revealed: QuerySet[Book, Book]
    reveal_type(mismatched.get_queryset())  # revealed: QuerySet[Author, Author]
```

## View serializer specialization

`get_serializer()` builds its result from the `serializer_class` the view names, and
`get_serializer_class()` returns it.

```py
from django.db import models
from rest_framework import generics, serializers

class Book(models.Model):
    title = models.CharField(max_length=100)

class BookSerializer(serializers.ModelSerializer):
    class Meta:
        model = Book
        fields = ["title"]

class View(generics.ListAPIView):
    queryset = Book.objects.all()
    serializer_class = BookSerializer

def f(view: View) -> None:
    reveal_type(view.get_serializer())  # revealed: BookSerializer
    reveal_type(view.get_serializer_class())  # revealed: type[BookSerializer]
    # `many=` sends drf through `ListSerializer` instead, so nothing is claimed
    reveal_type(view.get_serializer(many=True))  # revealed: BaseSerializer[Unknown]
    reveal_type(view.get_serializer(**{}))  # revealed: BaseSerializer[Unknown]
```

## Serializer model specialization

A `ModelSerializer` names its model in `Meta.model`, so `save()`, `create()` and `update()` return
it. The `Meta` may be inherited.

```py
from django.db import models
from rest_framework import serializers

class Book(models.Model):
    title = models.CharField(max_length=100)

class BookSerializer(serializers.ModelSerializer):
    class Meta:
        model = Book
        fields = ["title"]

class InheritsMeta(BookSerializer): ...

def f(serializer: BookSerializer, inherits: InheritsMeta, book: Book) -> None:
    reveal_type(serializer.save())  # revealed: Book
    reveal_type(serializer.create({}))  # revealed: Book
    reveal_type(serializer.update(book, {}))  # revealed: Book
    reveal_type(inherits.save())  # revealed: Book
```

## `many=True` at the constructor

drf builds a `ListSerializer` for `many=True`, whose `save()` returns a list. The stubs describe
none of that, so a serializer constructed that way reads as one model — exactly as it already does
when the type argument is written by hand, which is what the two halves of this test show.

```py
from django.db import models
from rest_framework import serializers

class Book(models.Model):
    title = models.CharField(max_length=100)

class Resolved(serializers.ModelSerializer):
    class Meta:
        model = Book
        fields = ["title"]

class Written(serializers.ModelSerializer[Book]):
    class Meta:
        model = Book
        fields = ["title"]

reveal_type(Resolved(data=[], many=True).save())  # revealed: Book
reveal_type(Written(data=[], many=True).save())  # revealed: Book
```

## A view's `many=` is visible, so nothing is claimed

The same `many=` reached through the view is an argument of the call, and refuses.

```py
from django.db import models
from rest_framework import generics, serializers

class Book(models.Model):
    title = models.CharField(max_length=100)

class BookSerializer(serializers.ModelSerializer):
    class Meta:
        model = Book
        fields = ["title"]

class View(generics.ListCreateAPIView):
    queryset = Book.objects.all()
    serializer_class = BookSerializer

def f(view: View) -> None:
    reveal_type(view.get_serializer(data=[], many=True).save())  # revealed: Unknown
```

## Specialization refusals

Nothing is claimed where the class body does not say it plainly: a queryset or serializer built in a
method, a `Meta` that names no model, a plain `Serializer`, or a project's own override of the
method — drf reads nothing from the class body to produce that one.

```py
from django.db import models
from rest_framework import generics, serializers

class Book(models.Model):
    title = models.CharField(max_length=100)

class FromMethod(generics.ListAPIView):
    def get_queryset(self):
        return Book.objects.all()

class NoMeta(serializers.ModelSerializer): ...
class PlainSerializer(serializers.Serializer): ...

class BookSerializer(serializers.ModelSerializer):
    class Meta:
        model = Book
        fields = ["title"]

class OverridesSerializerClass(generics.ListAPIView):
    queryset = Book.objects.all()
    serializer_class = BookSerializer

    def get_serializer_class(self) -> type[serializers.BaseSerializer]:
        return BookSerializer

def f(
    method: FromMethod,
    no_meta: NoMeta,
    plain: PlainSerializer,
    overrides: OverridesSerializerClass,
) -> None:
    reveal_type(method.get_queryset())  # revealed: Unknown
    reveal_type(method.get_object())  # revealed: Unknown
    reveal_type(no_meta.save())  # revealed: Unknown
    reveal_type(plain.save())  # revealed: Unknown
    reveal_type(overrides.get_serializer())  # revealed: BaseSerializer[Unknown]
```
