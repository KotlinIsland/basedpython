# sqlalchemy support

> session 2 of the [framework rollout](index.md) — first user of the
> descriptor-annotated fields-engine extension

## status: verified

constructor synthesis from `Mapped[T]` annotations is implemented and tested
across all four layers:

- **recognition** — `KnownModule::SqlalchemyOrmBase` (`Mapped`),
    `KnownModule::SqlalchemyOrmDeclApi` (`DeclarativeBase`, `MappedAsDataclass`),
    third-party gated; `KnownClass::SqlalchemyMapped` /
    `SqlalchemyDeclarativeBase` / `SqlalchemyMappedAsDataclass`
- **detection** — `dedicated/sqlalchemy.rs::is_declarative` (mro contains
    `DeclarativeBase`, and not `MappedAsDataclass` — the dataclass_transform
    path keeps winning for those), surfaced as
    `FrameworkRole::SqlalchemyDeclarative` and
    `CodeGeneratorKind::SqlalchemyDeclarative`
- **fields** — the descriptor-annotated extraction mode
    (`sqlalchemy_own_fields`): a class-body annotation whose declared type is a
    `Mapped[T]` descriptor is a field of the unwrapped type `T`
    (`mapped_field_type`); mixin/abstract fields flow in through the mro walk
    (which now collects from ordinary classes for sqlalchemy, like pydantic)
- **synthesis** — `own_synthesized_member` emits `(self, *, <field>: T = ...,   …) -> None`: keyword-only, every parameter optional; a user-defined
    `__init__` wins; an unresolvable base degrades to the runtime
    `__init__(self, **kw)` rather than an unsound closed signature
- **tests** — unit (`dedicated/role.rs`), mock-stub (`mdtest/sqlalchemy.md`),
    real-dependency (`mdtest/external/sqlalchemy.md`, lockfile committed),
    transform gates + soundness pin (`by_transforms` `frameworks.rs`), runtime
    divergence (`mdtest/basedpython_sqlalchemy.md`, run against real sqlalchemy
    on in-memory sqlite)

known limitations, deliberately scoped out of v1: only `Mapped[T]` (and direct
`Mapped` subclasses) unwrap — `DynamicMapped` / `WriteOnlyMapped` collections
degrade to no synthesis; only `DeclarativeBase` is recognized, not
`DeclarativeBaseNoMeta` (both degrade gracefully to the stubs' `**kw`); the
relationship-without-`Mapped` diagnostic (a design-approved stretch) is not
implemented

## scope

sqlalchemy **2.0 declarative** style only:

```py
class Base(DeclarativeBase): ...

class User(Base):
    __tablename__ = "user"
    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str]
    addresses: Mapped[list["Address"]] = relationship(back_populates="user")
```

legacy 1.x patterns (`Column()` attributes without `Mapped`, `declarative_base()`
factory classes, `Query`) are explicitly out of scope — they check as their
stubs allow, with no dedicated help

## what already works

sqlalchemy 2.0 ships inline types built on the descriptor protocol, which ty
resolves generally:

- `user.id` → `int`, `User.id` → `InstrumentedAttribute[int]` (class/instance
    duality via `Mapped.__get__` overloads)
- `select(User.name).where(User.id == 5)` — comparison operators on
    `InstrumentedAttribute` produce `ColumnElement[bool]`
- session/result generics (`Session.execute`, `Result.scalars`), async api
- baseline real-dependency tests exist:
    `crates/ty_python_semantic/resources/mdtest/external/sqlalchemy.md`
- `MappedAsDataclass` models — pep 681 `dataclass_transform`, same path as
    pydantic

## the gap: constructor synthesis

a plain declarative model inherits `def __init__(self, **kw: Any)` — every
constructor call is unchecked (`User(nam="typo")` passes). sqlalchemy's actual
runtime accepts any subset of mapped attributes as keywords. synthesize the
truthful signature:

```py
User.__init__  # (self, *, id: int = ..., name: str = ..., addresses: list[Address] = ...) -> None
```

- keyword-only, **every parameter optional** — sqlalchemy allows constructing
    with any subset (non-nullable columns are enforced at flush, not `__init__`);
    a required-parameter synthesis would be a lie
- parameter type is the `Mapped[T]` argument, not the descriptor
- includes `relationship()` fields — they are constructor keywords too

### mechanism

- new `CodeGeneratorKind`/`FieldKind` arm (`SqlalchemyMapped`), classified in
    `CodeGeneratorKind::from_class` via `dedicated/sqlalchemy.rs::is_declarative`
    (mro contains `DeclarativeBase`, and the class is not `MappedAsDataclass` —
    that path already works and must keep winning)
- fields gathered by the **descriptor-annotated** extraction mode
    ([index](index.md#fields-engine-extensions)): an annotation whose
    unsubscripted origin is `Mapped` (or a subclass) declares a field of the
    argument type; anything else in the body (`__tablename__`, `ClassVar`,
    plain annotations, methods) is not a field
- synthesis goes through the `own_synthesized_member` hub — a user-defined
    `__init__` in the class body wins, `__abstract__` and mixin classes
    contribute fields to subclasses through the existing mro walk
- skip synthesis entirely when anything is unresolvable (dynamic base,
    conditional fields) — fall back to `**kw: Any`, never guess

## recognition seams to populate

| seam             | additions                                                                                                                                                                                                                                                                   |
| ---------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KnownModule`    | `sqlalchemy.orm.decl_api` (DeclarativeBase, MappedAsDataclass), `sqlalchemy.orm.base`/`sqlalchemy.orm.attributes` (Mapped, InstrumentedAttribute) — verify canonical defining modules against the locked sqlalchemy version before committing, re-exports are not canonical |
| `KnownClass`     | `SqlalchemyDeclarativeBase`, `SqlalchemyMapped`, `SqlalchemyMappedAsDataclass`                                                                                                                                                                                              |
| dedicated module | `crates/ty_python_semantic/src/types/dedicated/sqlalchemy.rs` — `is_declarative`, mapped-annotation unwrapping helpers                                                                                                                                                      |
| role             | `FrameworkRole::SqlalchemyDeclarative` arm in `dedicated/role.rs`                                                                                                                                                                                                           |

## diagnostics

- the `external/sqlalchemy.md` `TODO` ("this should ideally be an error") is
    resolved: `User(invalid_arg=42)` and typo'd keywords are now
    `unknown-argument` errors, and a wrong-typed value is `invalid-argument-type`,
    because the synthesized `__init__` is truthful. the old first example relied
    on the loose `**kw: Any` (it passed `name=` for a column whose python
    attribute is `internal_name`, which raises at runtime); it was rewritten to
    the correct form
- stretch (design-approved, implement only if time allows):
    `relationship()` assigned without a `Mapped[...]` annotation → diagnostic
    suggesting the 2.0 form

## transpiler conformance column

- **`init` shorthand / `data class` modifier in a declarative body → transform
    error** (same rationale and shape as pydantic; the metaclass is
    instrumentation-bearing)
- **based enums as column types** — declaring `Mapped[Shape]` is fine to
    *check*, but mapping it to a column needs a user-supplied `TypeDecorator`;
    v1 does nothing special (the stubs surface the error), record a possible
    future diagnostic
- **conformance pins** (divergence tests): optional chaining across nullable
    relationships (`user?.address?.city`); soundness guards inside model
    methods don't disturb instrumentation; class-body defaults untouched;
    reification pass skips non-generic model classes

## test plan

1. mock-stub mdtests for detection + field extraction (minimal
    `sqlalchemy/orm/…` stubs under `.venv/<path-to-site-packages>/`) — these
    pin the *mechanism*
1. real-dependency mdtests extending `external/sqlalchemy.md`: synthesized
    constructor signature reveals, typo'd keyword → error, subset construction
    ok, mixin/abstract inheritance, user-defined `__init__` wins,
    `MappedAsDataclass` unchanged
1. `basedpython_sqlalchemy.md` divergence suite: a `.by` model module
    transpiles and runs against real sqlalchemy (in-memory sqlite), covering
    the conformance pins above

## out of scope

- lookup/typing precision inside `select()` beyond what inline stubs give
- alembic, hybrid extensions (`hybrid_property` checks as its stubs allow)
- legacy 1.x style
