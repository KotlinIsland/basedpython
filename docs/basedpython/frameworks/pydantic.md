# pydantic support

> session 1 of the [framework rollout](index.md) — checker support largely
> inherited from upstream ty; this session is an audit, a gap-fill, and the
> transpiler conformance column

## status: verified

the transpiler conformance column and the `.by` surface are implemented and
tested; the checker audit is done, with the cheap wins landed and the deep
gaps recorded below. what this session added:

- **`JustFloat` / `JustComplex` bind to the builtins at runtime** — a `.by`
    `float` / `complex` field in *any* runtime-annotation-introspecting class
    (pydantic, `dataclasses`, attrs, anything using `get_type_hints`) used to
    crash schema generation, because the `ty_extensions` alias lowered to an
    opaque `_TyExtMarker`. the exclusion of `int` is static-only, so the
    runtime binding is now `float` / `complex`
    (`by_transforms/src/transforms/lazy_import.rs`)
- **transpiler conformance pins** for the pydantic matrix column — reified
    generics never wrap a model class, class-body field defaults survive the
    mutable-defaults transform, soundness guards stay inside method bodies
    (`by_transforms/src/transforms/frameworks.rs` tests)
- **`.by` divergence suite** — `resources/mdtest/basedpython_pydantic.md`
    (with a committed lockfile), exercised by both the checker and the
    runtime-divergence harness. `crates/ty/tests/mdtest_divergence.rs` now
    gates pydantic-importing blocks on the framework being installed, mirroring
    the `typing_extensions` gate
- **checker coverage** for computed fields, bare validator decorators, and the
    `model_copy` / `model_construct` / `model_dump_json` return types
    (`resources/mdtest/external/pydantic.md`)

known limitations and gaps recorded this session:

- **a payload-enum *union* used directly as a field type does not validate.**
    `shape: Shape` where `Shape` is a based payload enum lowers the enum name
    to an opaque base class (`class Shape: pass`), which pydantic cannot build a
    schema for. annotate with the explicit variant union
    (`shape: Shape.Circle | Shape.Square`) or a single variant instead — both
    validate (covered in the divergence suite). all-payload-less enums (which
    lower to a stdlib `Enum`) validate directly
- **`model_dump()` returns `Unknown`, not `dict[str, Any]`.** sound but
    imprecise; modelling it precisely means synthesizing the signature in
    `dedicated/pydantic.rs`. `model_dump_json()` is already `str`
- **`x is <based-enum member>` wrongly lowers to `isinstance`** and crashes at
    runtime (an enum member is a value, not a type). this is a general
    identity-narrowing bug, not pydantic-specific; filed separately. the
    divergence suite uses `==` to compare enum members

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

### 1. checker audit and gap-fill — done

covered and pinned this session (`external/pydantic.md` new sections):

- **per-field `Field(frozen=True)`** now makes assignment to that field an
    `invalid-assignment` error while other fields on the same (non-frozen) model
    stay writable — a per-field `frozen` flag flows from the field specifier
    through `FieldInstance` / `FieldKind::Pydantic` into an overloaded
    synthesized `__setattr__` (`Literal["field"] -> Never` per frozen field,
    `str -> None` catch-all) ✓
- `@computed_field` properties read back at their declared type ✓
- bare `@field_validator` — the value parameter checks precisely; `cls`
    resolves to `Self` (not `type[Self]`), the same limitation the explicit-
    `@classmethod` section already documents ✓
- `model_copy`, `model_construct` return the model type precisely (instance and
    class); `model_dump_json` → `str` ✓
- generic models: `Box[int](value=…)` specializes the field to `int` ✓ (covered
    in the divergence suite)
- **`Annotated[T, Field(...)]` metadata** is now read — a PEP 681 field
    specifier in the annotation metadata (`Annotated[int, Field(default=0)]`,
    `Annotated[str, Field(strict=False)]`) is recognized, clearing two false
    errors. field specifiers are set up while inferring the annotation, and
    `annotated_field_specifier` (in `static_literal.rs`) pulls the
    `FieldInstance` out of the metadata ✓
- **unannotated field specifier** (`age = Field(default=0)` with no annotation)
    is now an `unannotated-model-field` error — pydantic raises at class
    creation; dataclasses tolerate it, so the check is gated to models
    (`types/diagnostic.rs` + `check_unannotated_model_field` in
    `infer/builder.rs`) ✓

recorded, not fixed (each with reasoning):

- **`model_dump()` → `Unknown`** — sound but imprecise; needs a synthesized
    signature in `dedicated/pydantic.rs`. deferred, non-blocking
- **`Field(default=None, validate_default=True)`** should error but doesn't —
    pydantic's stub returns `Any` from that overload, hiding the invalid
    default. forcing default validation generally regresses custom
    `dataclass_transform` specifiers and converters, so this needs
    pydantic-specific default extraction; deferred (TODO kept in
    `external/pydantic.md`)
- `validate_by_name` limitation (`# This is a known limitation`) — recorded
- recursive model types — blocked on general recursive-type support; record
    only, as scoped
- discriminated unions (`Field(discriminator=...)`) — record only
- pydantic version bump policy: the lockfile pins one version; bumping it is a
    deliberate act with a full external-test run (both `external/pydantic.lock`
    and `basedpython_pydantic.lock` move together)

### 2. framework role plumbing — done

- `FrameworkRole::PydanticModel` resolves through `class_framework_role`;
    `dedicated/role.rs` tests cover the direct base, inheritance-through-alias,
    the non-model negative, and first-party shadowing ✓
- `TypeInfo::framework_class_role` is exercised end-to-end through the project
    db by the `frameworks.rs` gate tests (a model is recognized → the gates fire;
    an ordinary class → they don't) ✓

### 3. transpiler conformance column — done

every pydantic cell of the [compat matrix](index.md#transpiler-compatibility)
is implemented and tested:

- **`init` shorthand in a model body → transform error** — landed with the
    common infrastructure (`transforms/frameworks.rs`), tested ✓
- **`data class` modifier on a model → transform error** — landed, tested ✓
- **reified generics gate** — the reified-generics pass only ever wraps a
    *function* whose type parameter reaches a value position; it never visits a
    class, so a model class is structurally never given a `@generic` wrapper.
    `Box[int](...)` is native pydantic. pinned by
    `frameworks.rs::generic_pydantic_model_never_reified` and the divergence
    suite's generic-model round-trip ✓
- **conformance pins** (`frameworks.rs` + divergence suite): class-body field
    defaults untouched by the mutable-defaults transform ✓; soundness guards
    stay inside method bodies, signatures untouched ✓; based-enum payload
    *variants* validate as field types (single variant, explicit variant union)
    — cover construction + `model_dump`; the bare enum-union *name* is a
    documented limitation (see status) ✓

### 4. `.by`-surface tests — done

`resources/mdtest/basedpython_pydantic.md` (`.by` blocks + committed lockfile):
float/complex fields, based enums as fields (all three shapes), optional
chaining on optional fields, checked `cast` of a `model_dump()` value, generic
model round-trip, mutable-default pin — each transpiled + executed by
`mdtest_divergence.rs` against an installed pydantic

## explicitly out of scope

- runtime plugin behaviour (`pydantic.plugin`), mypy-plugin parity beyond what
    the dedicated module models
- serialization schema correctness (`model_json_schema`)
- pydantic v1 compatibility mode

## files

| area             | files                                                                                                                            |
| ---------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| dedicated logic  | `crates/ty_python_semantic/src/types/dedicated/pydantic.rs`                                                                      |
| lax aliases      | `crates/ty_vendored/ty_extensions/pydantic.pyi`                                                                                  |
| checker tests    | `crates/ty_python_semantic/resources/mdtest/external/pydantic.md` (+ lockfile)                                                   |
| transpiler gates | `crates/by_transforms/src/transforms/frameworks.rs` consulting `TypeInfo::framework_class_role`                                  |
| runtime binding  | `crates/by_transforms/src/transforms/lazy_import.rs` (`JustFloat` / `JustComplex` → builtins)                                    |
| divergence tests | `crates/ty_python_semantic/resources/mdtest/basedpython_pydantic.md` (+ `.lock`); harness `crates/ty/tests/mdtest_divergence.rs` |
