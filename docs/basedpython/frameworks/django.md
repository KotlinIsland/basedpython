# django support

> session 4 of the [framework rollout](index.md) — the largest column:
> value-inferred fields, member synthesis, the stub-requirement diagnostic,
> and a project-wide index for reverse accessors

## the shape of the problem

django ships no type annotations at all, and its models are maximally
dynamic:

```py
class Author(models.Model):
    name = models.CharField(max_length=100)     # unannotated assignment
    birthday = models.DateField(null=True)

class Book(models.Model):
    author = models.ForeignKey(Author, on_delete=models.CASCADE)

author.name         # str            (field descriptor)
author.birthday     # date | None    (null=True changes the type)
author.id           # int            (auto pk, never declared)
book.author_id      # int            (fk attname, never declared)
author.book_set     # RelatedManager[Book]  (reverse accessor, defined by *Book*)
Author.objects      # Manager[Author]
Author.objects.filter(name__startswith="x")   # lookup-kwarg dsl
```

## stubs: require django-stubs

types come from the `django-stubs` pep 561 package, which the resolver
already prioritizes over the untyped runtime package (`-stubs` resolution and
`py.typed` handling are inherited and tested). policy:

- **do not vendor django stubs** — maintenance and license churn for a
    fast-moving package; users install `django-stubs` like any dependency
- new diagnostic `missing-framework-stubs`: fires once per project when
    `django` resolves from site-packages without `django-stubs` present, with
    the install command in the message. this diagnostic is general common
    machinery — any future framework with external stubs reuses it
- django-stubs is designed around its mypy plugin; the parts that work
    plugin-free (field `__new__` overloads on `null=`, descriptor `__get__`/
    `__set__` generics) are exactly the parts ty's general machinery consumes.
    the session starts with a **plugin-free audit**: real-dependency mdtests
    against django + django-stubs recording what already works. everything the
    mypy plugin does that stubs can't express is this doc's work list

## work items

### 1. field types (value-inferred fields)

`name = CharField(max_length=100)` — no annotation; the field's read/write
types live in the descriptor instance the rhs evaluates to. django-stubs
encodes them as `Field[_ST, _GT]` specializations selected by `__new__`
overloads (`null=True` → the `| None` specialization), so this should mostly
be plain descriptor resolution — instance access invokes `__get__`, which the
existing protocol handles

what needs the **value-inferred fields** extraction mode
([index](index.md#fields-engine-extensions)) is everything that treats those
assignments as *fields of the model* rather than attribute lookups:
constructor synthesis, `full_clean`-adjacent diagnostics, and the lookup dsl.
the mode gathers unannotated class-body assignments whose rhs type is a
`django.db.models.Field` instance into the standard `Field` list
(`FieldKind::DjangoModel`), recording per-field facts the dedicated module
extracts from the call: `null`, `primary_key`, `default`, literal-string
`related_name` — literal arguments only, anything dynamic degrades that field
to unknown-but-present

### 2. member synthesis (`dedicated/django.rs` + the hub)

through `own_synthesized_member`, gated on `FrameworkRole::DjangoModel`:

- `objects` — `Manager[ThisModel]` when the body doesn't declare a manager.
    django-stubs declares a loosely-typed fallback on `Model`; this is the
    documented **refinement** case from the [index](index.md#synthesized-members-beyond-constructors):
    re-specialize, don't invent
- `pk`/`id` — synthesize `id: int` (`BigAutoField` per modern defaults) when
    no field has `primary_key=True`; `pk` aliases the pk field's type
- fk attnames — for every `ForeignKey` field `f`, synthesize `f_id` with the
    target pk's type (`| None` when `null=True`)
- `__init__` — keyword-only, every field optional (django accepts any subset;
    requiredness is a `full_clean`/save concern), field names + attnames
- `DoesNotExist` / `MultipleObjectsReturned` — per-model exception classes
- abstract models (`Meta.abstract = True`) contribute fields to concrete
    subclasses via the normal mro walk; `Meta` parsing follows the
    `ModelConfig` precedent from pydantic (literal values only, `unknown`
    poisoning on dynamic constructs)

### 3. reverse accessors (project-wide index)

`author.book_set` is defined by `Book`, not `Author` — resolution needs an
inverted fk edge index over the whole project. design:

- per-file tracked query `model_fk_edges(db, file)` → `[(source model,   target model, related_name | None)]`, cheap (reads the fields list, no
    body inference)
- an aggregating query unions edges over first-party files, keyed by target;
    `own_synthesized_member` consults it for `<lower>_set` /
    `related_name` members → `RelatedManager[Source]`
- **incrementality risk is the reason this is staged last**: the aggregate
    depends on every first-party file's edge list; per-file tracking keeps
    invalidation proportional to edits, but the session must benchmark a
    large-project check before enabling it by default. fallback position if
    it doesn't pay: ship without reverse accessors, and let the
    unresolved-attribute diagnostic on a known model suggest the
    `related_name` explicitly ("did you mean the reverse accessor of
    `Book.author`?") — navigation value without the index cost

### 4. lookup-kwarg dsl (stretch, design-approved)

`filter(name__startswith="x")` — split on `__`, walk: field name → (optional)
relation hops → lookup suffix from a per-field-type lookup table; check the
value type against the lookup's operand type. literal-string keys only.
this reuses the fields list and the fk edges; it is the highest-value
diagnostic django users ask for, and the most work — implement only after
1–3 land, possibly as its own follow-up session

### 5. recognition seams

| seam             | additions                                                                                                                                                                                   |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KnownModule`    | `django.db.models.base` (Model, ModelBase), `django.db.models.fields` (+ `.related` for ForeignKey), `django.db.models.manager` — verify canonical modules against django-stubs, not django |
| `KnownClass`     | `DjangoModel`, `DjangoField`, `DjangoForeignKey`, `DjangoManager`                                                                                                                           |
| dedicated module | `crates/ty_python_semantic/src/types/dedicated/django.rs`                                                                                                                                   |
| role             | `FrameworkRole::DjangoModel` arm in `dedicated/role.rs`                                                                                                                                     |

## transpiler conformance column

- **`init` shorthand / `data class` modifier in a model body → transform
    error** (`ModelBase` metaclass, same shape as pydantic/sqlalchemy)
- **conformance pins** (divergence tests against real django, sqlite,
    `django.setup()` in-process): model modules written in `.by` register
    correctly after `by build`; lazy-import lowering doesn't defer
    model/signal registration (models modules exercise attribute access at
    class-definition time, which materializes lazy modules — pin it with a
    test rather than trusting the argument); soundness guards in model
    methods; optional chaining over nullable fks
- **`by build` and the app layout**: migrations, `INSTALLED_APPS`, and
    `manage.py` reference plain `.py` module paths — the built output is the
    django project. document the workflow (`by build` → run `manage.py` from
    `out/`) in the getting-started docs as part of this session; no transpiler
    changes expected

## test plan

1. real-dependency mdtests `external/django.md` (django + django-stubs,
    uv-locked): the plugin-free audit first, then reveals/errors for every
    synthesis in work item 2
1. mock-stub mdtests for the field-extraction mode and `Meta` parsing
    (hand-mini `django/db/models/…` stubs) — mechanism pins that don't churn
    with django-stubs releases
1. `missing-framework-stubs` diagnostic tests (django present, stubs absent —
    mock site-packages makes this easy)
1. divergence suite + a sandpit django project for the conformance pins

## out of scope

- `settings` object typing (per-project settings-module resolution — future)
- querysets beyond what stubs + synthesis give (`values()`/`values_list`
    precision, `annotate` expressions)
- async orm variants beyond stub-level checking
- django rest framework (a future candidate of its own)
