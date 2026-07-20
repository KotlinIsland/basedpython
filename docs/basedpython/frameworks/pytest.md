# pytest support

> session 3 of the [framework rollout](index.md) — introduces the injection
> registry, the one genuinely new resolution mechanism in framework support

## status: verified

implemented on this branch. the injection registry lives in
`crates/ty_python_semantic/src/types/dedicated/pytest.rs`; `function_framework_role`
(`PytestFixture`, `PytestTest`) sits in `dedicated/role.rs` next to the class
query; the checks are emitted from `infer_function_body` (in
`infer/builder/function.rs`), which is a body-scope region and so can safely
resolve the current function's own type without a salsa cycle.

- **recognition**: `KnownModule::PytestFixtures` (`_pytest.fixtures`) and
    `PytestMarkStructures` (`_pytest.mark.structures`), both third-party
    gated; `KnownFunction::PytestFixture` (the `fixture` decorator). no new
    `KnownClass` — `FixtureRequest` and `MarkGenerator` resolve through
    `known_module_symbol` / an `(module, name)` check, as pydantic does
- **registry**: `module_fixtures` (tracked, decorators + signatures only, no
    body inference), `conftest_chain` (tracked, walks directory ancestors
    collecting `conftest.py` via the revision-tracked `system_path_to_file`),
    `resolve_fixture` (module → conftest chain → builtins). the builtin table
    maps a fixture name to the `_pytest` submodule that defines it and reuses
    `module_fixtures` over that module, so the *types* come from the real
    (typed) `_pytest` sources; `request` resolves to `FixtureRequest`
- **provided type**: the fixture's declared return annotation, with
    `Iterator[T]` / `Generator[T, …]` / the async variants unwrapped to `T`.
    an unannotated (gradual) return yields no provided type, so a parameter
    bound to it is not checked (deriving it from the *inferred* body return is
    a recorded follow-up — `module_fixtures` deliberately avoids body inference)
- **diagnostics**: `invalid-fixture-type` (on; subdiagnostic at the fixture
    definition, cross-file), `unknown-fixture` (off by default — plugin
    fixtures are not discovered yet), `invalid-parametrize` (name + arity;
    element-type checking against annotations is the recorded follow-up)

### scope limits (recorded, not regressions)

- **module-level only**: fixtures and tests are recognized at module scope.
    class-based tests (`class Test*:` with fixture methods) and their `self`
    are out of scope for v1; `is_test_function` requires the definition to be
    in the global scope so a nested `test_*` helper is never misclassified
- **default collection conventions only**: `test_*.py` / `*_test.py` files and
    `test*` functions; `pytest.ini` / `pyproject` overrides are unread
- **no plugin fixture discovery** (`pytest11` entry points) — the reason
    `unknown-fixture` ships off by default
- `request.getfixturevalue(...)` and fixture cycles are unmodelled

## the problem

pytest fills test and fixture parameters by *name* from a scoped registry:

```py
@pytest.fixture
def db() -> Db:
    yield make_db()

def test_user(db: Db, tmp_path: Path) -> None: ...
```

no mainstream checker validates this. the failure modes are silent: a renamed
fixture leaves tests requesting a name that no longer exists (fails at
collection, not check time); an annotation that drifts from the fixture's
return type checks the body against a lie

## design: the injection registry

`crates/ty_python_semantic/src/types/dedicated/pytest.rs` plus project-level
salsa queries. resolution mirrors pytest's own order and is deliberately
file-scoped:

- `module_fixtures(db, file)` — fixture functions a file defines: functions
    whose decorator list resolves to `pytest.fixture` (semantic resolution —
    `@pytest.fixture`, `@pytest.fixture(scope=...)`, aliased imports all
    count). the fixture's *name* is the function name unless overridden by the
    decorator's `name=` argument (literal strings only; a dynamic name makes
    the fixture unresolvable and disables diagnostics for it, never guesses)
- `conftest_chain(db, file)` — the `conftest.py` files from the file's
    directory up to the project root, nearest first
- `resolve_fixture(db, file, name)` — same module → conftest chain →
    builtin table; first hit wins (pytest's shadowing order)
- **builtin fixtures** resolve through a name → (module, symbol) table into
    the typed `_pytest` stubs (`tmp_path` → `_pytest.tmpdir.tmp_path`, etc.) —
    the table is data in the dedicated module, the *types* come from stubs, so
    a pytest upgrade updates types for free

the fixture's **provided type** is derived from its return annotation with
generator unwrapping: `Iterator[T]` / `Generator[T, …]` → `T` (yield
fixtures), `AsyncIterator[T]` → `T` (async), plain `T` → `T`. an unannotated
fixture provides its inferred return, falling back to no-check when inference
is `Unknown`-ish

**which functions get checked**: fixture functions themselves, and test
functions — `test_*` functions in `test_*.py` / `*_test.py` files (pytest's
default conventions; honoring configured overrides is a recorded follow-up,
not v1). everything else is untouched

`function_framework_role(db, function) -> Option<FunctionFrameworkRole>`
(`PytestFixture`, `PytestTest`) lands in `dedicated/role.rs` alongside the
class query, as the [index](index.md#framework-roles) anticipates

## diagnostics

| check                           | behaviour                                                                                                                                                                                                                                                                                                                  |
| ------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| annotated param vs fixture type | on by default — if `resolve_fixture` finds the fixture and the declared annotation isn't assignable from the provided type, error with both types and the fixture's definition as a subdiagnostic                                                                                                                          |
| unused/unknown fixture name     | **off by default** — a param that resolves to no fixture. third-party plugins inject fixtures via `pytest11` entry points which v1 does not discover, so this diagnostic would false-positive on any plugin user; it ships as an opt-in lint until plugin discovery (site-packages `dist-info` entry-point scanning) lands |
| `parametrize` arity/types       | `@pytest.mark.parametrize("a,b", [...])` — when argnames is a literal string, check the name list against the function signature and each param-set's arity; element-type checking against annotations where the values are literal. dynamic argnames/argvalues → skip                                                     |
| fixture cycles                  | recorded follow-up, not v1                                                                                                                                                                                                                                                                                                 |

subdiagnostics always point at the resolved fixture definition — the value of
this feature is navigation as much as checking

## salsa discipline

`conftest_chain` depends on file-system layout, and `module_fixtures` must not
drag whole-module type inference into every consumer:

- `module_fixtures` is a tracked query reading only decorator resolution and
    signatures (no body inference)
- `resolve_fixture` composes tracked queries so a conftest edit only
    invalidates files under its directory
- follow the `.node()`-access rule: anything touching ast nodes is
    `#[salsa::tracked]`

## transpiler conformance column

pytest's contract with lowered output is *introspection*: it matches
parameter names, unwraps decorators, and inspects signatures. the conformance
pins (divergence tests running real pytest over transpiled `.by` test files):

- parameter names and order survive every lowering (soundness guards insert
    body statements only; decorators are preserved verbatim)
- a yield fixture written in `.by` (with `?.`, based enums, checked casts in
    its body) collects and injects correctly
- `parametrize` over based-enum variants works (variants are real classes)

## running `.by` test suites

pytest collects `.py` files, so `.by` tests run against transpiled output:
`by build` then `pytest out/` works today, with tracebacks mapped through the
line table. a `by test` convenience command (build + run pytest with mapped
tracebacks) is the natural follow-up — record as a separate feature, not part
of this session

## test plan

1. mock-stub mdtests for the registry mechanics: minimal `pytest`/`_pytest`
    stubs; fixtures in module / conftest / nested conftest; shadowing order;
    `name=` override; yield unwrapping; async fixtures; negative cases (docs
    warn: one `##` header per example — registry tests are especially prone to
    the block-concatenation trap)
1. real-dependency mdtests extending `external/pytest.md` (currently only
    `pytest.fail` terminality): builtin fixture types (`tmp_path`,
    `monkeypatch`, `capsys`), `pytest.raises`, parametrize
1. divergence suite `basedpython_pytest.md` + a sandpit project running real
    pytest over transpiled tests for the conformance pins

## out of scope

- plugin-provided fixtures (entry-point discovery) — prerequisite for turning
    the unknown-fixture diagnostic on by default; design recorded above
- `pytest.ini`/`pyproject` collection-convention overrides
- `request.getfixturevalue(...)` dynamic access
