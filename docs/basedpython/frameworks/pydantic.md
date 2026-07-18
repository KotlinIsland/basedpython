# pydantic support

> session 1 of the [framework rollout](index.md) — checker support largely
> inherited from upstream ty; this session is an audit, a gap-fill, and the
> transpiler conformance column

## status

upstream ty ships deep pydantic v2 support and this fork inherits all of it:

- `types/dedicated/pydantic.rs` — model detection (`is_model`), `ModelMetadata`
    / `ModelConfig` resolution (`model_config` merging class keywords,
    `model_config = ConfigDict(...)` / dict literals, and base classes),
    strict/lax constructor input types via the `ty_extensions.pydantic` `Lax*`
    aliases, root models, `pydantic_settings` optional-field constructors,
    `extra` handling with collision-safe `**extra` synthesis
- recognition seams populated: `KnownModule::Pydantic*`,
    `KnownClass::{PydanticBaseModel, PydanticBaseSettings, PydanticConfigDict,   PydanticRootModel}`, `KnownFunction::PydanticField`
- constructor synthesis through `CodeGeneratorKind::Pydantic` +
    `own_synthesized_member`, driven by pep 681 `dataclass_transform` +
    field-specifier evaluation in `evaluate_known_cases`
- 1250 lines of real-dependency mdtests:
    `crates/ty_python_semantic/resources/mdtest/external/pydantic.md`
    (uv-locked)

## work items

### 1. checker audit and gap-fill

walk `external/pydantic.md` and burn down its recorded `TODO`s, then extend
coverage to the areas it doesn't touch. known gaps from the current test file:

- `Annotated[...]` field metadata mishandled (two `TODO`-marked false errors)
- frozen-field assignment not yet an error (`## Frozen models and fields`)
- `validate_by_name` limitation (`# This is a known limitation`)
- recursive model types (blocked on general recursive-type support — record,
    don't fix here)

uncovered areas to audit and test (fix what's cheap, file the rest in the doc):

- `@field_validator` / `@model_validator` signature checking (bare-decorator
    form; the explicit-`@classmethod` form is already tested)
- `@computed_field` properties
- `model_construct`, `model_copy`, `model_dump` / `model_dump_json` precision
- generic models: field specialization through `Model[int](...)`, including
    interaction with basedpython fluid specializations
- discriminated unions (`Field(discriminator=...)`) — likely record-only
- pin the pydantic version bump policy: the lockfile pins one version; bumping
    it is a deliberate act with a full external-test run

### 2. framework role plumbing (validates the common machinery)

- `FrameworkRole::PydanticModel` already resolves through
    `class_framework_role` (see [index](index.md)); confirm coverage in
    `dedicated/role.rs` tests includes inheritance and non-model negatives
- confirm `TypeInfo::framework_class_role` returns the role through the
    project db in a `by_transforms` cross-file test

### 3. transpiler conformance column

implement and test every pydantic cell of the [compat matrix](index.md#transpiler-compatibility):

- **`init` shorthand in a model body → transform error** — *landed with the
    common infrastructure* (`transforms/frameworks.rs`). pydantic synthesizes
    `__init__`; an innocent-looking `init(self, let x: int)` silently bypasses
    validation. a user who really wants a custom `__init__` writes
    `def __init__` explicitly, which stays allowed — matching pydantic's own
    escape hatch
- **`data class` modifier on a model → transform error** — *landed with the
    common infrastructure*. stacking `@dataclass(slots=True)` onto
    `ModelMetaclass` is runtime-broken
- **reified generics gate** — the `@generic` wrapper must never be applied to
    a pydantic model class; pydantic's own `__class_getitem__` already reifies,
    and constructor reification `M[int](...)` is native pydantic behaviour
    (divergence test: a reified generic model round-trips through validation)
- **conformance pins** (divergence tests asserting today's behaviour stays
    true): class-body mutable defaults untouched by the mutable-defaults
    transform (pydantic deep-copies defaults itself); soundness guards never
    alter model method signatures; based-enum payload variants validate as
    pydantic field types (they lower to stdlib dataclasses, which pydantic
    handles natively — cover construction, validation failure, and
    `model_dump`)

### 4. `.by`-surface tests

the external mdtest is `.py`; add a `basedpython_pydantic.md` divergence suite
exercising models written in `.by` — based enums as fields, optional chaining
on optional fields, checked `cast` of `model_dump()` results — through
transpile + execute

## explicitly out of scope

- runtime plugin behaviour (`pydantic.plugin`), mypy-plugin parity beyond what
    the dedicated module models
- serialization schema correctness (`model_json_schema`)
- pydantic v1 compatibility mode

## files

| area             | files                                                                                                                        |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| dedicated logic  | `crates/ty_python_semantic/src/types/dedicated/pydantic.rs`                                                                  |
| lax aliases      | `crates/ty_vendored/ty_extensions/pydantic.pyi`                                                                              |
| checker tests    | `crates/ty_python_semantic/resources/mdtest/external/pydantic.md` (+ lockfile)                                               |
| transpiler gates | `crates/by_transforms/src/transforms/{init_method,modifiers,reified_generic}.rs` consulting `TypeInfo::framework_class_role` |
| divergence tests | `crates/ty_python_semantic/resources/mdtest/basedpython_pydantic.md`                                                         |
