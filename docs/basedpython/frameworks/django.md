# django support

> session 4 of the [framework rollout](index.md) — the largest column:
> value-inferred fields, member synthesis, the stub-requirement diagnostic,
> and a project-wide index for reverse accessors

## status

**implemented and verified** (all four test layers). what shipped, and where
it deviates from the plan below:

- **recognition + role**: `KnownModule::DjangoDbModels*`,
    `KnownClass::{DjangoModel, DjangoField, DjangoForeignKey,   DjangoOneToOneField, DjangoManyToManyField, DjangoManager}`,
    `dedicated/django.rs`, `FrameworkRole::DjangoModel`, transpiler gates —
    all as planned
- **field types — plan correction**: django-stubs (6.x) has **no `null=`
    constructor overloads**; `_ST`/`_GT` are set only by its mypy plugin. the
    stubs do declare per-field-class `_pyi_private_set_type` /
    `_pyi_private_get_type` markers for that plugin, so ty re-derives the
    plugin's specialization at the field constructor call
    (`ConstructorBinding::return_type` → `django::field_constructor_instance_type`):
    marker types, `to=` substitution for relation fields (`through=` for m2m),
    `null=True` unioning `None`. after pinning, reads/writes/`__init__`
    synthesis all flow through the general descriptor machinery. anything
    dynamic (string `to=`, non-literal `null=`, markerless custom fields)
    degrades to no pinning
- **member synthesis**: `__init__` (keyword-only, all optional, `pk` +
    fk attnames, m2m excluded), `id`/`pk`, `<field>_id` attnames,
    `Meta.abstract` handling — as planned, through `own_synthesized_member`.
    two re-scopes: `objects` needed **no refinement** (the stubs declare
    `ClassVar[Manager[Self]]`, which resolves precisely), and
    `DoesNotExist`/`MultipleObjectsReturned` keep their stub declarations
    (base exception types) — per-model exception subclasses are a possible
    future nicety, not needed for correct checking of `except` clauses
- **reverse accessors — re-scoped to same-module**: implemented as a
    class-keyed tracked query over the model's *own module* (the standard
    `models.py` layout) instead of the project-wide edge index; cross-module
    relations degrade to unresolved attributes. reasons: the aggregate's
    invalidation cost was the design's flagged risk, and enumerating
    definitions structurally (not resolving every global symbol) keeps the
    query cycle-free. the project-wide index remains future work with this
    query as its per-file building block
- **`missing-framework-stubs`** — implemented as a registered lint fired at
    each `django` import statement when the runtime package resolves without
    `django-stubs`, rather than literally once per project: per-file
    diagnostics are how the salsa architecture parallelizes, and the lint is
    suppressable/configurable like any other. the framework table
    (`dedicated/mod.rs`, `EXTERNALLY_STUBBED_FRAMEWORKS`) is the general
    machinery future frameworks extend
- **lookup-kwarg dsl (work item 4)**: **implemented** — `filter`/`get`/
    `exclude`/`get_or_create`/`update_or_create` (+ async) lookup kwargs,
    `create()`/`acreate()` kwargs, and literal field-name arguments to
    `order_by`/`only`/`defer`/`values`/`values_list`/`earliest`/`latest` are
    validated against the field list (relation traversal, operand types for
    recognized lookups) via the `invalid-field-lookup` diagnostic; the resolver
    lives in `dedicated/django.rs`, the call-site hook in the builder's
    `check_call` loop. also added: `get_<field>_display()` for choice fields.
    a full parity comparison against the mypy plugin is in
    [django-stubs-parity.md](django-stubs-parity.md), which also records what
    remains (per-model exception identity, `values`/`annotate` synthetic types,
    `from_queryset`, project-wide reverse accessors) and why each is blocked on
    infrastructure rather than django knowledge
- **transpiler column**: gates + conformance pins verified (see the section
    below). the sandpit run surfaced and fixed a general composition bug:
    the lazy-import `_LazyAttr` proxy was not `isinstance`-transparent, so
    soundness checks against lazily-imported names failed at runtime; the
    proxy now implements `__instancecheck__`/`__subclasscheck__`
- **tests**: `dedicated/role.rs` + `transforms/frameworks.rs` unit tests;
    `mdtest/django.md` mock-stub mechanism pins;
    `mdtest/external/django.md` real-dependency suite (django 6.0.7 +
    django-stubs 6.0.7, uv-locked); sandpit runtime conformance project
    (`basedpython-sandpit/django-conformance`) executing the lowered output
    against real django + sqlite

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
- django-stubs is designed around its mypy plugin. **plan correction from
    the audit**: current stubs have no `null=`-selected `__new__` overloads —
    the descriptor generics are left entirely to the plugin, which reads the
    stubs' own `_pyi_private_set_type`/`_pyi_private_get_type` markers. ty
    consumes the same markers (see [status](#status)). what does work
    plugin-free: `objects: ClassVar[Manager[Self]]`, the whole queryset api,
    and the model exception attributes — the audit is recorded at the top of
    `mdtest/external/django.md`

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

### 4. lookup-kwarg dsl (implemented)

`filter(name__startswith="x")` — split on `__`, walk: field name → (optional)
relation hops → lookup suffix from a per-field-type lookup table; check the
value type against the lookup's operand type. literal-string keys only.
this reuses the fields list and the fk edges; it is the highest-value
diagnostic django users ask for.

**done** (see [status](#status) and [django-stubs-parity.md](django-stubs-parity.md)):
the resolver (`resolve_lookup` / `resolve_create_kwarg` / `resolve_field_name`)
is in `dedicated/django.rs`; the call-site hook (`check_django_queryset_call`)
runs in the builder's `check_call` loop, keyed off `queryset_method_kind` +
`queryset_or_manager_model`. it is deliberately conservative — unrecognized
lookups, unresolved relation targets, and non-literal keys degrade to no error.

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
    django project (`by build` → run from `out/`, exactly the workflow the
    sandpit conformance project uses). one transpiler change was needed after
    all: the lazy-import `_LazyAttr` proxy now delegates
    `__instancecheck__`/`__subclasscheck__`, since soundness checks pass
    lazily-imported model classes to `isinstance` at runtime

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
