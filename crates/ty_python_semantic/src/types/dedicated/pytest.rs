//! dedicated pytest support — the fixture injection registry
//!
//! pytest fills a test or fixture parameter by *name* from a scoped registry
//! of fixture providers: the function's own module, then the `conftest.py`
//! chain from its directory up to the project, then pytest's builtin
//! fixtures. no ordinary checker validates this, so a parameter annotated
//! with a type that drifts from the fixture's real type checks the body
//! against a lie, and a renamed fixture leaves a request that only fails at
//! collection time. an *unannotated* parameter is the common case, and
//! nothing else in the language can type it: only the registry knows what
//! pytest will bind, so [`injected_parameter_type`] supplies it.
//!
//! detection is semantic: a function is a fixture because a decorator
//! resolves to `_pytest.fixtures.fixture` (`KnownFunction::PytestFixture`),
//! through any alias or re-export. the builtin fixtures are themselves
//! `@fixture`-decorated functions in the `_pytest` package, so they resolve
//! through the very same [`module_fixtures`] machinery — the builtin table
//! only maps a name to the `_pytest` submodule that defines it, and the
//! types come from those (typed) modules, so a pytest upgrade updates them
//! for free. see `docs/basedpython/frameworks/pytest.md`

use ruff_db::files::{File, FilePath, system_path_to_file};
use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast};
use rustc_hash::{FxHashMap, FxHashSet};
use ty_module_resolver::{KnownModule, ModuleName, file_to_module, resolve_module_confident};
use ty_python_core::definition::Definition;
use ty_python_core::semantic_index;

use crate::Db;
use crate::place::known_module_symbol;
use crate::types::ProgramEnvironment;
use crate::types::dedicated::role::{FunctionFrameworkRole, function_framework_role};
use crate::types::{
    FunctionType, KnownClass, KnownFunction, Type, definition_expression_type,
    infer_definition_types,
};

/// a pytest fixture provider resolved for a requested parameter name.
pub(in crate::types) struct ResolvedFixture<'db> {
    /// the type a parameter bound to this fixture receives, with generator
    /// unwrapping applied. `None` means the fixture's type could not be
    /// derived (unannotated / gradual), so a parameter bound to it is not
    /// checked
    pub(in crate::types) provided_type: Option<Type<'db>>,
    /// the fixture's defining function, for a secondary annotation pointing
    /// at it. `None` for the special `request` fixture, which has no source
    /// definition
    pub(in crate::types) definition: Option<Definition<'db>>,
}

/// what a `@pytest.fixture` decorator declares about a function.
struct FixtureMarker {
    /// the fixture's name, when overridden by `@pytest.fixture(name="...")`
    /// with a literal string; `None` otherwise (the function name is used)
    name_override: Option<Name>,
}

/// `true` if `function` is decorated with `@pytest.fixture` (bare or called).
pub(in crate::types) fn is_fixture_function<'db>(
    db: &'db dyn Db,
    function: FunctionType<'db>,
) -> bool {
    fixture_marker(db, function).is_some()
}

fn fixture_marker<'db>(db: &'db dyn Db, function: FunctionType<'db>) -> Option<FixtureMarker> {
    let file = function.file(db);
    let definition = function.definition(db);
    let module = parsed_module(db, function.python_file(db)).load(db);
    let node = function.node(db, file, &module);
    let types = infer_definition_types(db, definition);

    node.decorator_list.iter().find_map(|decorator| {
        match &decorator.expression {
            // `@pytest.fixture(scope=..., name=...)`: the marker is the callee
            ast::Expr::Call(call) => {
                if !is_pytest_fixture(db, types.expression_type(&call.func)) {
                    return None;
                }
                let name_override = call.arguments.find_keyword("name").and_then(|keyword| {
                    definition_expression_type(db, definition, &keyword.value)
                        .as_string_literal()
                        .map(|literal| Name::new(literal.value(db)))
                });
                Some(FixtureMarker { name_override })
            }
            // `@pytest.fixture`
            expression => {
                is_pytest_fixture(db, types.expression_type(expression)).then_some(FixtureMarker {
                    name_override: None,
                })
            }
        }
    })
}

fn is_pytest_fixture<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    matches!(
        ty,
        Type::FunctionLiteral(function) if function.known(db) == Some(KnownFunction::PytestFixture)
    )
}

/// the fixtures a file defines at module level, keyed by fixture name.
///
/// this reads only decorator resolution and each candidate's signature, so a
/// consumer does not drag whole-module inference in. a later definition of the
/// same name wins, mirroring python rebinding.
///
/// a signature is not always cheap, though: under `infer-unannotated-signatures`
/// an unannotated one is recovered from its body, and a body in this very module
/// asks which fixture its parameters resolve to — so the two meet in a cycle.
/// no fixtures is the right thing to start that fixpoint from: a test whose own
/// signature is still being computed cannot be the fixture another one wants.
#[salsa::tracked(
    returns(ref),
    cycle_initial = |_, _, _| FxHashMap::default(),
    heap_size = ruff_memory_usage::heap_size,
)]
pub(in crate::types) fn module_fixtures(
    db: &dyn Db,
    file: File,
) -> FxHashMap<Name, FunctionType<'_>> {
    let parsed = parsed_module(db, db.program_file(file).python_file(db)).load(db);
    let index = semantic_index(db, db.program_file(file));

    let mut fixtures = FxHashMap::default();
    for statement in parsed.suite() {
        let ast::Stmt::FunctionDef(function_node) = statement else {
            continue;
        };
        let definition = index.expect_single_definition(function_node);
        let Some(function) = infer_definition_types(db, definition).function_type(definition)
        else {
            continue;
        };
        let Some(marker) = fixture_marker(db, function) else {
            continue;
        };
        let name = marker
            .name_override
            .unwrap_or_else(|| function_node.name.id.clone());
        fixtures.insert(name, function);
    }
    fixtures.shrink_to_fit();
    fixtures
}

/// the `conftest.py` files that apply to `file`, nearest first.
///
/// pytest merges fixtures from every `conftest.py` on the path from the test
/// file's directory up to the rootdir. we walk the directory ancestors and
/// collect each `conftest.py` that exists; `system_path_to_file` is
/// revision-tracked, so a newly added or removed `conftest.py` invalidates
/// only the files beneath it.
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
pub(in crate::types) fn conftest_chain(db: &dyn Db, file: File) -> Vec<File> {
    let mut chain = Vec::new();
    let FilePath::System(path) = file.path(db) else {
        return chain;
    };
    let Some(directory) = path.parent() else {
        return chain;
    };
    for ancestor in directory.ancestors() {
        for extension in COLLECTED_EXTENSIONS {
            if let Ok(conftest) =
                system_path_to_file(db, ancestor.join(format!("conftest.{extension}")))
                && conftest != file
            {
                chain.push(conftest);
            }
        }
    }
    chain.shrink_to_fit();
    chain
}

/// resolve the fixture named `name` for `file`, in pytest's shadowing order:
/// the file's own fixtures, then the `conftest.py` chain (nearest first),
/// then the builtin fixtures. the first hit wins.
pub(in crate::types) fn resolve_fixture<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    name: &str,
) -> Option<ResolvedFixture<'db>> {
    if let Some(function) = module_fixtures(db, file).get(name) {
        return Some(resolved_from_function(db, env, *function));
    }
    for conftest in conftest_chain(db, file) {
        if let Some(function) = module_fixtures(db, *conftest).get(name) {
            return Some(resolved_from_function(db, env, *function));
        }
    }
    resolve_builtin_fixture(db, env, name)
}

fn resolved_from_function<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    function: FunctionType<'db>,
) -> ResolvedFixture<'db> {
    ResolvedFixture {
        provided_type: fixture_provided_type(db, env, function),
        definition: Some(function.definition(db)),
    }
}

/// the pytest builtin fixtures we resolve, mapping the fixture name to the
/// `_pytest` submodule whose `@fixture`-decorated function defines it. third
/// party plugin fixtures (`pytest11` entry points) are not discovered in v1,
/// which is why the unknown-fixture diagnostic ships off by default.
const BUILTIN_FIXTURE_MODULES: &[(&str, &str)] = &[
    ("tmp_path", "_pytest.tmpdir"),
    ("tmp_path_factory", "_pytest.tmpdir"),
    ("monkeypatch", "_pytest.monkeypatch"),
    ("capsys", "_pytest.capture"),
    ("capsysbinary", "_pytest.capture"),
    ("capfd", "_pytest.capture"),
    ("capfdbinary", "_pytest.capture"),
    ("recwarn", "_pytest.recwarn"),
    ("caplog", "_pytest.logging"),
    ("pytestconfig", "_pytest.fixtures"),
];

fn resolve_builtin_fixture<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    name: &str,
) -> Option<ResolvedFixture<'db>> {
    // `request` is injected by pytest itself rather than defined as a
    // fixture function; its type is `FixtureRequest`
    if name == "request" {
        let request = known_module_symbol(db, env, KnownModule::PytestFixtures, "FixtureRequest")
            .place
            .ignore_possibly_undefined()?
            .to_instance_approximation(db, env)?;
        return Some(ResolvedFixture {
            provided_type: Some(request),
            definition: None,
        });
    }

    let (_, module_name) = BUILTIN_FIXTURE_MODULES
        .iter()
        .find(|(fixture_name, _)| *fixture_name == name)?;
    let module = resolve_module_confident(
        db,
        env.resolver_environment(db),
        &ModuleName::new(module_name)?,
    )?;
    let function = module_fixtures(db, module.file(db)?).get(name)?;
    Some(resolved_from_function(db, env, *function))
}

/// the type a parameter bound to `function` receives, or `None` when it
/// cannot be derived (an unannotated or otherwise gradual return).
///
/// a yield fixture annotates its return as `Iterator[T]` / `Generator[T,
/// ...]` (or the async variants); the provided value is the yielded `T`, so
/// the generator wrapper is unwrapped. a plain `-> T` fixture provides `T`.
fn fixture_provided_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    function: FunctionType<'db>,
) -> Option<Type<'db>> {
    let signature = function.signature(db);
    let return_type = signature.iter().last()?.return_ty;
    if return_type.is_dynamic() {
        return None;
    }
    Some(unwrap_generator(db, env, return_type))
}

/// the yielded element of a generator/iterator type, or `ty` unchanged.
fn unwrap_generator<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Type<'db> {
    for known in [
        KnownClass::Generator,
        KnownClass::AsyncGenerator,
        KnownClass::Iterator,
        KnownClass::AsyncIterator,
        KnownClass::Iterable,
    ] {
        if let Some(specialization) = ty.known_specialization(db, env, known) {
            if let Some(element) = specialization.types(db).first() {
                return *element;
            }
        }
    }
    ty
}

/// `true` if `ty` is an instance of pytest's `MarkGenerator` — the type of
/// `pytest.mark`, whose `.parametrize` attribute builds the decorator.
fn is_mark_generator<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> bool {
    let Some(class) = ty
        .nominal_class(db, env)
        .and_then(|class| class.class_literal(db).as_static())
    else {
        return false;
    };
    class.name(db).as_str() == "MarkGenerator"
        && file_to_module(db, class.program_file(db).resolver_file(db))
            .and_then(|module| module.known(db))
            == Some(KnownModule::PytestMarkStructures)
}

/// a `@pytest.mark.parametrize` marker on a test, with the expressions a
/// check anchors its diagnostics to.
pub(in crate::types) struct ParametrizeMarker<'ast> {
    /// the `argnames` expression
    pub(in crate::types) argnames: &'ast ast::Expr,
    /// the names parsed out of `argnames`
    pub(in crate::types) names: Vec<Name>,
    /// the `argvalues` expression, when one is supplied
    pub(in crate::types) argvalues: Option<&'ast ast::Expr>,
}

/// the `@pytest.mark.parametrize` marker `decorator` applies to `function`.
/// `None` when `decorator` is any other decorator, or when its argnames are
/// not static literals (dynamic → not checkable).
pub(in crate::types) fn parametrize_marker<'ast>(
    db: &dyn Db,
    env: &ProgramEnvironment<'_>,
    function: FunctionType<'_>,
    decorator: &'ast ast::Decorator,
) -> Option<ParametrizeMarker<'ast>> {
    let ast::Expr::Call(call) = &decorator.expression else {
        return None;
    };
    let ast::Expr::Attribute(attribute) = call.func.as_ref() else {
        return None;
    };
    if attribute.attr.as_str() != "parametrize" {
        return None;
    }
    let types = infer_definition_types(db, function.definition(db));
    if !is_mark_generator(db, env, types.expression_type(attribute.value.as_ref())) {
        return None;
    }
    let argnames = call.arguments.find_argument_value("argnames", 0)?;
    Some(ParametrizeMarker {
        names: parametrize_names(argnames)?,
        argnames,
        argvalues: call.arguments.find_argument_value("argvalues", 1),
    })
}

/// the parameter names `@pytest.mark.parametrize` supplies to `function`.
///
/// pytest passes these as ordinary arguments from the marker's value rows, so
/// they are not fixture requests: they neither resolve against the registry
/// nor take a fixture's type.
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
pub(in crate::types) fn parametrized_names(
    db: &dyn Db,
    function: FunctionType<'_>,
) -> FxHashSet<Name> {
    let env = &ProgramEnvironment::from_file(function.program_file(db));
    let file = function.file(db);
    let module = parsed_module(db, function.python_file(db)).load(db);
    let mut names: FxHashSet<Name> = function
        .node(db, file, &module)
        .decorator_list
        .iter()
        .filter_map(|decorator| parametrize_marker(db, env, function, decorator))
        .flat_map(|marker| marker.names)
        .collect();
    names.shrink_to_fit();
    names
}

/// the type an unannotated parameter named `name` of `function` receives.
///
/// pytest binds such a parameter by name from the fixture registry, so the
/// body sees the fixture's provided type rather than an implicit `Unknown`.
/// `None` — leaving the parameter gradual — when `function` is not a pytest
/// function, when the name is supplied by `parametrize` instead, or when no
/// fixture resolves or the resolved one's type cannot be derived.
pub(in crate::types) fn injected_parameter_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    function: FunctionType<'db>,
    name: &str,
) -> Option<Type<'db>> {
    if !function_framework_role(db, function).is_some_and(FunctionFrameworkRole::is_pytest) {
        return None;
    }
    if parametrized_names(db, function).contains(name) {
        return None;
    }
    resolve_fixture(db, env, function.file(db), name)?.provided_type
}

/// parse `@pytest.mark.parametrize` argnames — a comma-separated string or a
/// list/tuple of string literals — into the parametrized names. `None` when
/// the argnames are not a static string literal (dynamic → not checkable).
fn parametrize_names(argnames: &ast::Expr) -> Option<Vec<Name>> {
    fn names_from_elements(elements: &[ast::Expr]) -> Option<Vec<Name>> {
        elements
            .iter()
            .map(|element| {
                element
                    .as_string_literal_expr()
                    .map(|literal| Name::new(literal.value.to_str().trim()))
            })
            .collect()
    }

    match argnames {
        ast::Expr::StringLiteral(literal) => Some(
            literal
                .value
                .to_str()
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(Name::new)
                .collect(),
        ),
        ast::Expr::List(list) => names_from_elements(&list.elts),
        ast::Expr::Tuple(tuple) => names_from_elements(&tuple.elts),
        _ => None,
    }
}

/// the source extensions pytest's naming conventions apply to. a `.by` module
/// transpiles to a `.py` of the same stem, so it is collected under exactly the
/// same rules and has to be recognised here under its own name
const COLLECTED_EXTENSIONS: [&str; 2] = ["py", "by"];

/// `true` if `file` is collected by pytest under the default conventions:
/// its name is `conftest`, `test_*`, or `*_test`.
fn is_test_file(db: &dyn Db, file: File) -> bool {
    let FilePath::System(path) = file.path(db) else {
        return false;
    };
    let Some(name) = path.file_name() else {
        return false;
    };
    let Some(extension) = path.extension() else {
        return false;
    };
    if !COLLECTED_EXTENSIONS.contains(&extension) {
        return false;
    }
    let stem = name
        .strip_suffix(extension)
        .and_then(|stem| stem.strip_suffix('.'))
        .unwrap_or(name);
    stem == "conftest" || stem.starts_with("test_") || stem.ends_with("_test")
}

/// `true` if `function` is a pytest test: a module-level `test*`-named
/// function in a collected test file. `conftest.py` holds fixtures, not
/// tests, so its functions are never tests; and pytest only collects tests
/// at module scope, so a nested helper or a method named `test_*` (class
/// based tests are out of scope for v1) does not count.
pub(in crate::types) fn is_test_function<'db>(
    db: &'db dyn Db,
    function: FunctionType<'db>,
) -> bool {
    let file = function.file(db);
    if !is_test_file(db, file) {
        return false;
    }
    let FilePath::System(path) = file.path(db) else {
        return false;
    };
    path.file_name() != Some("conftest.py")
        && function.name(db).starts_with("test")
        && function.definition(db).file_scope(db).is_global()
}

use ty_python_core::use_def_map;

use crate::place::definitions::DefinitionResolution;
use crate::types::may_exist_at_runtime;

mod collection;
mod fixtures;

pub use fixtures::{
    FixtureBinding, FixtureExposure, FixtureNameSource, fixture_bindings_for_parameter,
    fixture_exposures_for_definition, pytest_global_plugin_files,
};

/// Returns whether `definition` remains bound in its defining scope and may exist at runtime.
fn is_available_definition<'db>(db: &'db dyn Db, definition: Definition<'db>) -> bool {
    let resolution = DefinitionResolution::from_bindings(
        db,
        use_def_map(db, definition.scope(db)).end_of_scope_bindings(definition.place(db)),
    );
    resolution.definitions().contains(&definition) && may_exist_at_runtime(db, definition)
}
