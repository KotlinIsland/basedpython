# framework support architecture

basedpython supports major python frameworks on three fronts:

1. **checking** — ty understands the framework's runtime magic (synthesized
    constructors, descriptor fields, name-keyed injection) so `.by` and `.py`
    code using the framework checks precisely
1. **transpiling** — every basedpython lowering stays runtime-correct inside
    framework constructs, verified by a per-framework conformance matrix
1. **diagnostics** — framework-grade checks that a generic checker cannot
    express (unknown fixture, invalid model field, missing stub package)

currently supported are [pydantic](pydantic.md), [sqlalchemy](sqlalchemy.md),
[pytest](pytest.md), and [django](django.md). each has its own design doc
scoped for one implementation session; this page defines the shared
architecture they all build on

## philosophy

**dedicated, in-tree, no plugin api.** framework knowledge lives in rust
modules inside `ty_python_semantic` (`src/types/dedicated/<framework>.rs`),
the pattern upstream ty established with `dedicated/pydantic.rs`. a plugin
api would freeze internal interfaces and push framework logic out of our
test suite; in-tree support is versioned with the checker, tested in ci, and
free to use internal apis. user-extensible plugins are an explicit non-goal

**semantic detection, never name matching.** a class is a pydantic model
because its resolved mro contains `pydantic.BaseModel` — resolved through the
type system, so aliasing (`from pydantic import BaseModel as BM`), re-exports,
and inheritance chains all work. `KnownModule` third-party gating
(`try_from_search_path_and_name`) already guarantees a first-party module that
happens to be called `pydantic` is never recognized

**general primitives first, dedicated code second.** most framework behaviour
is expressible with machinery ty already has — descriptors, pep 681
`dataclass_transform`, overloads, `-stubs` packages. dedicated rust is written
only where the framework's semantics genuinely exceed the type system:
constructor synthesis from unannotated fields, config parsing, name-keyed
injection. before writing a dedicated hook, check whether better stubs or a
general checker fix covers it

**graceful degradation.** framework not installed → no framework machinery
activates (detection resolves nothing). pattern too dynamic to resolve →
fall back to ordinary inference, never guess. a missed synthesis is an
`unknown attribute` the user can annotate around; a wrong synthesis is a
lie — when in doubt, don't synthesize

## the seams

framework support is implemented by extending a fixed set of existing seams.
follow-up sessions should not invent parallel machinery — if a framework need
doesn't fit a seam, extend the seam

| seam                                | location                                                                                                         | role                                                                                                                                |
| ----------------------------------- | ---------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| `KnownModule`                       | `crates/ty_module_resolver/src/module.rs`                                                                        | recognize framework modules; third-party variants gated by `is_third_party()` so they must resolve from a third-party search path   |
| `KnownClass` / `KnownFunction`      | `crates/ty_python_semantic/src/types/class/known.rs`, `types/function.rs`                                        | recognize framework classes/functions by name + canonical module (`PydanticBaseModel`, `PydanticField` are the precedent)           |
| dedicated modules                   | `crates/ty_python_semantic/src/types/dedicated/`                                                                 | per-framework logic: detection, config parsing, synthesis helpers (`pydantic.rs` is the template)                                   |
| `CodeGeneratorKind` + fields engine | `types/class.rs` (`from_class`), `types/class/static_literal.rs` (`fields`/`fields_inner`, `Field`, `FieldKind`) | classify field-bearing classes and gather their fields for constructor synthesis                                                    |
| `own_synthesized_member`            | `types/class/static_literal.rs`                                                                                  | the one hub where members are invented (`__init__`, `__match_args__`, `__sealed_members__`); explicit class-body members always win |
| `evaluate_known_cases`              | `types/call/bind.rs`                                                                                             | call-return special cases: field specifiers (`Field(...)`), `dataclass_transform`, decorator effects                                |
| `ty_extensions` vendored stubs      | `crates/ty_vendored/ty_extensions/*.pyi`, injected by `ty_vendored/build.rs`                                     | checker-internal type aliases a framework needs (`ty_extensions.pydantic` lax aliases are the precedent)                            |
| descriptor protocol                 | `types.rs` (`invoke_descriptor_protocol`, `instance_member` vs `class_member`)                                   | class-level vs instance-level attribute duality (`Mapped[T]`, django fields) — already general, frameworks just rely on it          |
| `TypeInfo`                          | `crates/by_transforms/src/type_info.rs`                                                                          | the transpiler's window into ty; framework role queries surface here for lowering gates                                             |
| mdtest environments                 | `crates/ty_test` (`[project] dependencies`, `<path-to-site-packages>`)                                           | hermetic mock stubs and uv-locked real framework versions in checker tests                                                          |

## new common machinery

these are the pieces this design adds; they are shared by all four frameworks
(and by future ones)

### framework roles

`crates/ty_python_semantic/src/types/dedicated/role.rs` defines the single
classification query both the checker and the transpiler consult:

```rs
pub enum FrameworkRole {
    PydanticModel,          // implemented
    DjangoModel,            // implemented
    SqlalchemyDeclarative,  // implemented
}

pub fn class_framework_role(db, class) -> Option<FrameworkRole>
```

- salsa-tracked, mro-based, delegating to each dedicated module's detection
    function (`pydantic::is_model`, `django::is_model`, and
    `sqlalchemy::is_declarative`)
- roles are deliberately coarse — one role per "kind of class the framework
    transforms". fine-grained facts (is it a root model? is it abstract?) stay
    in the dedicated module
- function-level roles (pytest fixture/test, later fastapi dependency) are a
    parallel query `function_framework_role`, added by the pytest session —
    same file, same shape

the transpiler reaches this through a new `TypeInfo::framework_class_role`
method, so lowering passes can gate on "am i inside a django model body"
without re-implementing detection

### fields-engine extensions

`fields_inner` currently gathers fields from *annotated* class-body
assignments (dataclass, pydantic, namedtuple, typeddict). two new extraction
modes cover the orm frameworks, keyed by new `FieldKind` arms — not by
hardcoded class names:

- **descriptor-annotated fields** (sqlalchemy, implemented) — the annotation
    is a marker generic: `x: Mapped[int]` declares a field whose instance type
    is `int` and whose class-level type is the descriptor's `__get__` result
    (`InstrumentedAttribute[int]`). the engine unwraps the marker
    (`sqlalchemy::mapped_field_type`, an exact-`Mapped` specialization read);
    the extraction mode is `sqlalchemy_own_fields` + `FieldKind::SqlalchemyMapped`,
    and — like pydantic — it collects from every class in the mro so declarative
    mixins contribute their fields
- **value-inferred fields** (django, implemented) — the field is an
    *unannotated* assignment `name = CharField(max_length=100)` whose RHS is a
    descriptor instance; field types come from the descriptor's
    `__get__`/`__set__`. django-stubs leaves the descriptor generics to its
    mypy plugin, so ty pins the specialization at the field constructor call
    from the stubs' own `_pyi_private_set_type`/`_pyi_private_get_type`
    markers plus the call's literal `to=`/`null=` facts
    (`dedicated/django.rs`); the extraction mode itself is
    `django_own_fields` + `FieldKind::Django`

both modes feed the same `Field` struct, so everything downstream —
constructor synthesis, `__match_args__`, frozen checks — works unchanged

### synthesized members beyond constructors

django needs members that aren't dataclass-style codegen: `objects`, the
auto `id` pk, fk `<name>_id` attnames, reverse accessors. these go through
the same `own_synthesized_member` hub, in a role-gated arm, under the hub's
standing rules:

- an explicit class-body member always wins over synthesis
- a member declared in *stubs* with a useless type (django-stubs declares
    `objects` too loosely without its mypy plugin) may be **refined** by the
    dedicated module — refinement replaces only the specialization, never the
    member's existence, and each refinement is individually documented and
    tested in the framework's dedicated module

### injection registry

pytest's fixture model — parameters filled by *name* from a scoped registry
of providers — is a new kind of resolution, designed once and reused (fastapi
`Depends` fits it later). it lives in `types/dedicated/pytest.rs` plus
project-level salsa queries:

- `module_fixtures(db, file)` — fixture functions a file defines
- `conftest_chain(db, file)` — the `conftest.py` ancestry inside the project
- `resolve_fixture(db, file, name)` — resolution in pytest's order

see [pytest](pytest.md) for the full design, including how builtin fixtures
resolve through `_pytest` stubs and why unknown-name diagnostics ship
off-by-default until plugin discovery exists

### stub strategy

| framework  | stubs                     | action                                                                                                                                                      |
| ---------- | ------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| pydantic   | inline (`py.typed`)       | nothing — site-packages resolution                                                                                                                          |
| sqlalchemy | inline (`py.typed`, 2.0+) | nothing                                                                                                                                                     |
| pytest     | inline (`py.typed`)       | nothing (`_pytest` internals are typed)                                                                                                                     |
| django     | none upstream             | require `django-stubs`; pep 561 `-stubs` priority already works in the resolver; add a `missing-framework-stubs` diagnostic when django resolves without it |

checker-internal aliases a framework needs (pydantic's `Lax*` inputs) ship as
`ty_extensions.<framework>` vendored stubs via the existing `build.rs`
injection. vendoring or patching *framework* stubs themselves
(`by_typeshed_patch` style) is reserved for stubs that are broken for us and
unfixable upstream — not used in v1, and any future use must follow the
patch pipeline's verify-before-implement rule

## transpiler compatibility

every basedpython lowering must stay runtime-correct inside framework
constructs. the matrix below is the conformance contract: each framework
session implements its column and turns every non-trivial cell into a test
(transform unit test for gates, `.by` divergence test with the real framework
for runtime behaviour)

**legend** — ✓ compatible as-is · G gated (pass consults
`framework_class_role` and adapts/skips) · D diagnostic (reject with a clear
message) · – not applicable

| lowering                                          | pydantic model                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  | sqlalchemy declarative                                                                                                         | django model                                                                                                                                                                                                                        | pytest test/fixture                                                                                  |
| ------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `init` shorthand in class body                    | D — the framework synthesizes `__init__`; declaring one silently changes validation/instrumentation semantics                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | D                                                                                                                              | D                                                                                                                                                                                                                                   | –                                                                                                    |
| `data class` / `frozen data class` modifier       | D — stacking `@dataclass(slots=True)` on a model metaclass is runtime-broken                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    | D                                                                                                                              | D                                                                                                                                                                                                                                   | –                                                                                                    |
| based enums as field types                        | ✓ verified — payload-less enums lower to a stdlib `Enum` and a payload variant lowers to a frozen dataclass, both validated natively; a single variant or an *explicit* variant union works as a field type. the bare enum-*union name* (`f: Shape`) does not — the union base is an opaque class pydantic can't schematize — annotate with the explicit variant union instead (documented limitation). also fixed here: `float`/`complex` fields (basedpython lowers to `JustFloat`/`JustComplex`, now bound to the builtins at runtime so schema generation sees a real type) | needs `TypeDecorator`; ✓ to declare, D later on column use                                                                     | needs custom field; same                                                                                                                                                                                                            | ✓                                                                                                    |
| reified generics / type reification               | ✓ verified — generic models implement `__class_getitem__`; constructor reification `M[int](...)` is native pydantic. the reified-generics pass only ever wraps a *function*, never a class, so a model class is structurally never given a `@generic` wrapper (pinned by a conformance test + a divergence round-trip)                                                                                                                                                                                                                                                          | G verified — models are rarely generic; reification only wraps functions, so a model class is never given a `@generic` wrapper | G                                                                                                                                                                                                                                   | ✓                                                                                                    |
| soundness checks (all positions)                  | ✓ — guards live inside bodies, signatures untouched                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             | ✓                                                                                                                              | ✓                                                                                                                                                                                                                                   | ✓ — parameter names and signatures must survive lowering, pytest introspects them (conformance test) |
| mutable default re-evaluation                     | ✓ — applies to function defaults only; class-body field defaults are untouched (conformance test pins this)                                                                                                                                                                                                                                                                                                                                                                                                                                                                     | ✓                                                                                                                              | ✓                                                                                                                                                                                                                                   | ✓                                                                                                    |
| optional chaining / force unwrap / coalesce       | ✓                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               | ✓ verified — `user?.address?.city` across a nullable relationship (divergence round-trip)                                      | ✓                                                                                                                                                                                                                                   | ✓                                                                                                    |
| lazy imports                                      | ✓                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               | ✓                                                                                                                              | ✓ verified — model registration under `django.setup()` pinned by the sandpit conformance run, which also caught and fixed a general gap: the `_LazyAttr` proxy now delegates `isinstance`/`issubclass`, which soundness checks need | ✓                                                                                                    |
| `__all__` epilogue, forward refs, typing redirect | ✓                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               | ✓                                                                                                                              | ✓                                                                                                                                                                                                                                   | ✓                                                                                                    |
| extensions (when implemented)                     | extension members on model classes resolve at transpile time — no runtime conflict; revisit when extensions land                                                                                                                                                                                                                                                                                                                                                                                                                                                                | same                                                                                                                           | same                                                                                                                                                                                                                                | same                                                                                                 |

cells marked ✓ still get at least one divergence test in the framework's
session — the matrix records *verified* compatibility, not assumed

gates live in the existing passes (they consult `TypeInfo::framework_class_role`),
not in a separate framework pass; the diagnostic cells are transform errors
emitted through `PassContext::errors` with messages naming the framework and
the fix

## testing strategy

four layers, cheapest first:

1. **mock-stub mdtests** — hermetic detection/synthesis units using
    `.venv/<path-to-site-packages>/…` files containing minimal hand-written
    framework stubs. fast, no network, pinpoint failures
1. **real-dependency mdtests** — `[project] dependencies = ["<fw>==x.y.z"]`
    with a committed uv lockfile; the checker runs against the framework's
    actual types. this is the primary correctness layer — framework stubs are
    large and hand-mocks drift
1. **transform unit tests** — gates and diagnostics in
    `by_transforms` (`transpile_typed` cross-file tests, since role detection
    needs the project db)
1. **runtime divergence** — `.by` mdtest blocks executed through
    `mdtest_divergence` and sandpit runs with the framework installed, proving
    the lowered output actually behaves (a pydantic model with based-enum
    fields validates; a fixture-injected test passes under real pytest)

gotchas that bite framework tests especially:

- all mdtest code blocks under one `##` header concatenate into one file —
    one header per example, or diagnostics silently vanish
- in rust-side tests, the mock site-packages directory must be **disjoint
    from the project root** (`/proj` + `/sp`, not `/` + `/sp`): a nested
    site-packages is claimed by the first-party search path during
    `file_to_module`, which defeats `KnownModule` third-party gating and
    silently disables detection (`by_transforms` `frameworks.rs` tests and
    `ty_python_semantic` `dedicated/role.rs` tests are the reference setups)

## future candidates

worth supporting next, in rough value-per-effort order:

- **attrs** — pure `dataclass_transform` + field specifiers; mostly falls out
    of existing machinery, a good low-cost validation of the seams
- **fastapi** — high value and almost entirely reuse: response/request models
    are pydantic; `Depends` injection fits the pytest injection registry;
    path/query parameter checks reuse the fields engine
- **msgspec** — `dataclass_transform`-shaped structs, small dedicated surface
- **typer / click** — decorator parameter dsl; option/argument types vs
    function signature, a self-contained check
- **litestar** — fastapi-shaped, after fastapi
