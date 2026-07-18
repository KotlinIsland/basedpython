# framework support

basedpython provides deep support for popular python frameworks. the type checker understands framework-specific patterns, and the transpiler keeps basedpython features compatible with framework runtime behavior.

supported frameworks: [Pydantic](pydantic.md), [SQLAlchemy](sqlalchemy.md), [pytest](pytest.md), and [Django](django.md).

## what framework support means

when you use a supported framework with basedpython:

- **type checking works precisely** — the checker understands framework magic like synthesized constructors, descriptor fields, and dependency injection, so your `.by` code checks correctly
- **transpilation stays compatible** — basedpython features (like checked cast, optional chaining, reified generics) work correctly inside framework constructs, tested against the real framework
- **framework-specific diagnostics** — you get checks that make sense for the framework (unknown fixture names in pytest, invalid field lookups in django, etc.)

## framework support limitations

framework support is graceful: if a pattern is too dynamic to type-check, the checker falls back to ordinary inference rather than guessing. this means:

- a framework not installed → no special checking activates for it
- a pattern the checker can't resolve → you'll see an `unknown attribute` error, which you can annotate around if needed
- a limitation → documented in the framework's page with a workaround

framework support is also **not exhaustive**. each framework has a conformance matrix showing what works and what doesn't, including baseline limitations of the framework itself (e.g., django has no type annotations at all, so field types must come from stubs).

## getting started

check each framework's documentation to learn what works, what doesn't, and any limitations or required setup (like installing additional stubs packages).

## basedpython features and framework compatibility

basedpython features generally work well inside framework code, but some patterns interact with framework syntax and have restrictions:

- **`init` shorthand and `data class` modifiers** — these conflict with frameworks that synthesize their own constructors (pydantic, sqlalchemy, django). you'll get an error if you try to use them in a framework model or declarative class
- **basedpython enums as fields** — payload-less enums work fine; payload enums also work but have limitations in some frameworks
- **reified generics** — works with generic framework classes (e.g., pydantic's generic models), but the transpiler never wraps a framework class itself with the generic machinery
- **optional chaining, checked cast, coalesce** — all work correctly inside framework code
- **lazy imports** — compatible with framework registration and initialization

each framework page details its specific limitations.

## future candidates

worth supporting next, in rough value-per-effort order:

- **attrs** — minimal friction, mostly works through existing dataclass machinery
- **fastapi** — high value; reuses pydantic support and pytest fixture injection
- **msgspec** — compact struct support
- **typer / click** — decorator-based parameter DSL
- **litestar** — fastapi-shaped framework
