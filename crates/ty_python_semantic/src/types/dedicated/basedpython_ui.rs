//! dedicated basedpython-ui support — recognising the framework's observables
//! and composition scopes
//!
//! the framework (`basedpython_ui`) is a compose-style ui library: a
//! `@composable` function describes a piece of ui, and re-runs whenever one of
//! the observables it reads — a `State[T]`, `StateList[T]`, `StateDict[K, V]`,
//! `Derived[T]` or `Ambient[T]` — changes. nothing framework-specific is
//! encoded beyond that: these queries answer "is this value an observable",
//! "what does it hold" and "is this function a composition scope", and every
//! ui-specific check builds on them. detection is semantic — the resolved mro
//! and the decorator's resolved function — never import-string matching, so
//! aliases, re-exports and subclasses all classify correctly
//!
//! unlike the other frameworks here, `basedpython_ui` is recognised on a
//! first-party search path too ([`KnownModule::is_framework`]), because it is
//! developed in place
//!
//! [`KnownModule::is_framework`]: ty_module_resolver::KnownModule::is_framework

use ty_module_resolver::{KnownModule, file_to_module};

use crate::Db;
use crate::types::function::{FunctionDecorators, KnownFunction};
use crate::types::{ClassBase, FunctionType, KnownClass, ProgramEnvironment, Type};

/// which of the framework's observables a value is
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObservableKind {
    /// `State[T]`: a cell, read and written through `.value`
    State,
    /// `StateList[T]`: an observable list of immutable elements
    StateList,
    /// `StateDict[K, V]`: an observable mapping of immutable keys and values
    StateDict,
    /// `Derived[T]`: a memoised computation over state, read through `.value`
    Derived,
    /// `Ambient[T]`: a tree-scoped value, read through `.current`
    Ambient,
}

impl ObservableKind {
    /// the class that declares this observable, in `basedpython_ui.runtime`
    const fn class(self) -> KnownClass {
        match self {
            Self::State => KnownClass::BasedpythonUiState,
            Self::StateList => KnownClass::BasedpythonUiStateList,
            Self::StateDict => KnownClass::BasedpythonUiStateDict,
            Self::Derived => KnownClass::BasedpythonUiDerived,
            Self::Ambient => KnownClass::BasedpythonUiAmbient,
        }
    }

    /// the framework's observable classes, each declared in `basedpython_ui.runtime`
    const ALL: [Self; 5] = [
        Self::State,
        Self::StateList,
        Self::StateDict,
        Self::Derived,
        Self::Ambient,
    ];

    /// the methods that change this observable — every one of them notifies
    /// the observable's readers, which is what makes writing one during
    /// composition an error, and what makes a call to one a write worth
    /// tracing to its readers
    const fn mutators(self) -> &'static [&'static str] {
        match self {
            Self::State => &["set", "update"],
            Self::StateList => &[
                "append",
                "insert",
                "remove_at",
                "remove",
                "pop",
                "clear",
                "__setitem__",
            ],
            Self::StateDict => &[
                "remove",
                "pop",
                "clear",
                "update",
                "setdefault",
                "__setitem__",
            ],
            Self::Derived | Self::Ambient => &[],
        }
    }

    /// whether `method` is one of this observable's [mutators](Self::mutators)
    pub(crate) fn is_mutator(self, method: &str) -> bool {
        self.mutators().contains(&method)
    }

    /// whether `method` mutates *some* observable: a call to a method of this
    /// name is worth typing to see whether it is a write
    pub(crate) fn is_any_mutator(method: &str) -> bool {
        Self::ALL.iter().any(|kind| kind.is_mutator(method))
    }
}

/// `ty` without a use-site restriction or alias around it: a `final
/// StateList[int]` (what a constructor call infers under `let`) is an
/// observable exactly as the `StateList[int]` inside is.
///
/// Every predicate that asks what shape a value *is* — observable, builtin
/// container, read-only view — has to look through these wrappers, or a
/// `final list[out int]` reads as neither a list nor a view
pub(crate) fn underlying<'db>(db: &'db dyn Db, ty: Type<'db>) -> Type<'db> {
    match ty {
        Type::Restricted(restricted) => underlying(db, restricted.value_type(db)),
        Type::TypeAlias(alias) => underlying(db, alias.value_type(db)),
        _ => ty,
    }
}

/// the observable `ty` is an instance of — a `State[T]`, `StateList[T]`,
/// `StateDict[K, V]`, `Derived[T]` or `Ambient[T]`, directly or through a
/// subclass — or `None` for anything else
pub(crate) fn observable_kind<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<ObservableKind> {
    let (class, specialization) = underlying(db, ty)
        .nominal_class(db, env)
        .and_then(|class| class.static_class_literal(db))?;
    class
        .iter_mro(db, specialization)
        .filter_map(ClassBase::into_class)
        .find_map(|base| {
            ObservableKind::ALL
                .into_iter()
                .find(|observable| base.is_known(db, observable.class()))
        })
}

/// whether `ty` is an instance of one of the framework's observables — a
/// `State[T]`, `StateList[T]`, `StateDict[K, V]`, `Derived[T]` or `Ambient[T]`,
/// directly or through a subclass
pub(crate) fn is_observable_instance<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> bool {
    observable_kind(db, env, ty).is_some()
}

/// the value a `State[T]` or `Derived[T]` holds — the `T` — when `ty` is an
/// instance of either. `None` for anything else, including the collection
/// observables (`StateList`, `StateDict`), whose element types are not a single
/// value
pub(crate) fn state_value_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<Type<'db>> {
    [
        KnownClass::BasedpythonUiState,
        KnownClass::BasedpythonUiDerived,
    ]
    .into_iter()
    .find_map(|holder| {
        let specialization = underlying(db, ty).known_specialization(db, env, holder)?;
        let [value] = specialization.types(db) else {
            return None;
        };
        Some(*value)
    })
}

/// the element type of a `StateList[T]` — the `T` — when `ty` is an instance
/// of one. `None` for anything else
pub(crate) fn state_list_element_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<Type<'db>> {
    let specialization =
        underlying(db, ty).known_specialization(db, env, KnownClass::BasedpythonUiStateList)?;
    let [element] = specialization.types(db) else {
        return None;
    };
    Some(*element)
}

/// whether `function` is decorated with the framework's `@composable`, resolved
/// through the decorator's type ([`FunctionDecorators::COMPOSABLE`]), so an
/// alias or re-export of the decorator counts
pub(crate) fn is_composable<'db>(db: &'db dyn Db, function: FunctionType<'db>) -> bool {
    function.has_known_decorator(db, FunctionDecorators::COMPOSABLE)
}

/// whether `function` is one of the framework's widget builders (`Text`,
/// `Button`, `Column`, …), which emits into the composition being built and so,
/// like a composable, can only be called while composing.
///
/// Resolved through the framework's `@builder` decorator
/// ([`FunctionDecorators::UI_BUILDER`]), the same way a composable is resolved
/// through `@composable`. Being declared in `basedpython_ui.widgets` is not
/// enough on its own: that module is free to hold ordinary helpers, and a
/// helper that emits nothing must stay callable outside a composition
pub(crate) fn is_widget_builder<'db>(db: &'db dyn Db, function: FunctionType<'db>) -> bool {
    function.has_known_decorator(db, FunctionDecorators::UI_BUILDER)
}

/// whether `function` is `basedpython_ui.runtime.Runtime.set_root` — the
/// runtime's own entry point, whose `root` argument is composed as the root
/// of the composition (what `run_app` / `compose_test` wrap). Resolved by the
/// method's declaring module rather than by a known class, so the check needs
/// nothing but the method's definition
pub(crate) fn is_set_root<'db>(db: &'db dyn Db, function: FunctionType<'db>) -> bool {
    function.name(db) == "set_root"
        && file_to_module(db, function.program_file(db).resolver_file(db))
            .and_then(|module| module.known(db))
            == Some(KnownModule::BasedpythonUiRuntime)
}

/// whether `known` is one of the framework's *slot* functions — `state`,
/// `state_list`, `state_dict`, `derived`, `remember` and the effects — whose
/// result is remembered per call site for the lifetime of the enclosing
/// composition scope
pub(crate) const fn is_slot_function(known: KnownFunction) -> bool {
    matches!(
        known,
        KnownFunction::BasedpythonUiState
            | KnownFunction::BasedpythonUiStateList
            | KnownFunction::BasedpythonUiStateDict
            | KnownFunction::BasedpythonUiDerived
            | KnownFunction::BasedpythonUiRemember
            | KnownFunction::BasedpythonUiLaunchedEffect
            | KnownFunction::BasedpythonUiDisposableEffect
            | KnownFunction::BasedpythonUiSideEffect
    )
}

/// whether `known` is one of the framework's entry points — `run_app`,
/// `compose_test` — whose `root` block is where a composition starts
pub(crate) const fn is_composition_root(known: KnownFunction) -> bool {
    matches!(
        known,
        KnownFunction::BasedpythonUiRunApp | KnownFunction::BasedpythonUiComposeTest
    )
}

#[cfg(test)]
mod tests {
    use ruff_db::files::system_path_to_file;
    use ruff_db::parsed::parsed_module;
    use ruff_python_ast as ast;
    use ty_python_core::semantic_index;

    use super::*;
    use crate::db::tests::{TestDb, TestDbBuilder};
    use crate::types::dedicated::role::{FunctionFrameworkRole, function_framework_role};
    use crate::types::infer_definition_types;
    use crate::{HasType, SemanticModel};

    const RUNTIME_STUB: &str = "\
class State[T]:
    value: T
    def __init__(self, initial: T) -> None: ...
class StateList[T]: ...
class StateDict[K, V]: ...
class Derived[T]:
    value: T
class Ambient[T]: ...
def composable[F](fn: F) -> F: ...
";

    const INIT_STUB: &str = "\
from .runtime import State as State, StateList as StateList, StateDict as StateDict, \
Derived as Derived, Ambient as Ambient, composable as composable
";

    /// a db with the mock framework installed in site-packages and `source` as
    /// `/src/main.py`
    fn installed_framework(source: &str) -> anyhow::Result<TestDb> {
        TestDbBuilder::new()
            .with_site_packages("/sp")
            .with_file("/sp/basedpython_ui/__init__.pyi", INIT_STUB)
            .with_file("/sp/basedpython_ui/runtime.pyi", RUNTIME_STUB)
            .with_file("/src/main.py", source)
            .build()
    }

    /// a db with the mock framework developed in place — a first-party package
    /// beside `source`, which is `/src/main.py`
    fn first_party_framework(source: &str) -> anyhow::Result<TestDb> {
        TestDbBuilder::new()
            .with_file("/src/basedpython_ui/__init__.py", INIT_STUB)
            .with_file("/src/basedpython_ui/runtime.py", RUNTIME_STUB)
            .with_file("/src/main.py", source)
            .build()
    }

    /// the inferred type of the value of the last assignment in `/src/main.py`
    fn last_assigned_type(db: &TestDb) -> Type<'_> {
        let file = system_path_to_file(db, "/src/main.py").expect("main.py should exist");
        let module = parsed_module(db, db.program_file(file).python_file(db)).load(db);
        let model = SemanticModel::new(db, crate::Db::program_file(db, file));
        let assignment = module
            .suite()
            .iter()
            .rev()
            .find_map(ast::Stmt::as_assign_stmt)
            .expect("source should end with an assignment");
        assignment
            .value
            .inferred_type(&model)
            .expect("assigned value should infer a type")
    }

    /// the function type of the last top-level `def` in `/src/main.py`
    fn last_function_type(db: &TestDb) -> FunctionType<'_> {
        let file = system_path_to_file(db, "/src/main.py").expect("main.py should exist");
        let module = parsed_module(db, db.program_file(file).python_file(db)).load(db);
        let index = semantic_index(db, db.program_file(file));
        let function_node = module
            .suite()
            .iter()
            .rev()
            .find_map(ast::Stmt::as_function_def_stmt)
            .expect("source should define a function");
        let definition = index.expect_single_definition(function_node);
        infer_definition_types(db, definition)
            .function_type(definition)
            .expect("a `@composable` function should keep its function-literal type")
    }

    #[test]
    fn installed_state_instance_is_observable() -> anyhow::Result<()> {
        let db = installed_framework("from basedpython_ui import State\nx = State(1)\n")?;
        let ty = last_assigned_type(&db);
        let env = db.program_environment();
        assert!(is_observable_instance(&db, &env, ty));
        assert_eq!(
            state_value_type(&db, &env, ty).map(|value| value.display(&db, &env).to_string()),
            Some("int".to_owned())
        );
        Ok(())
    }

    #[test]
    fn every_observable_is_recognised_through_the_package_re_export() -> anyhow::Result<()> {
        for (name, known) in [
            ("State[int]", KnownClass::BasedpythonUiState),
            ("StateList[int]", KnownClass::BasedpythonUiStateList),
            ("StateDict[str, int]", KnownClass::BasedpythonUiStateDict),
            ("Derived[int]", KnownClass::BasedpythonUiDerived),
            ("Ambient[int]", KnownClass::BasedpythonUiAmbient),
        ] {
            let db = installed_framework(&format!(
                "from basedpython_ui import State, StateList, StateDict, Derived, Ambient\n\
                 def make() -> {name}: ...\nx = make()\n"
            ))?;
            let ty = last_assigned_type(&db);
            let env = db.program_environment();
            let class = ty
                .nominal_class(&db, &env)
                .and_then(|class| class.static_class_literal(&db))
                .map(|(class, _)| class)
                .expect("an observable should infer a nominal instance");
            assert!(class.is_known(&db, known), "`{name}` should be `{known:?}`");
            assert!(
                is_observable_instance(&db, &env, ty),
                "`{name}` is an observable"
            );
        }
        Ok(())
    }

    #[test]
    fn subclass_of_an_observable_is_observable_but_holds_no_single_value() -> anyhow::Result<()> {
        let db = installed_framework(
            "from basedpython_ui import StateList\nclass Items(StateList[str]): ...\nx = Items()\n",
        )?;
        let ty = last_assigned_type(&db);
        let env = db.program_environment();
        assert!(is_observable_instance(&db, &env, ty));
        assert_eq!(state_value_type(&db, &env, ty), None);
        Ok(())
    }

    #[test]
    fn ordinary_instance_is_not_observable() -> anyhow::Result<()> {
        let db = installed_framework(
            "from basedpython_ui import State\nclass Plain: ...\nx = Plain()\n",
        )?;
        let ty = last_assigned_type(&db);
        let env = db.program_environment();
        assert!(!is_observable_instance(&db, &env, ty));
        assert_eq!(state_value_type(&db, &env, ty), None);
        Ok(())
    }

    /// the framework is developed in place, so — unlike `pydantic` (see
    /// `role.rs`) — a first-party `basedpython_ui` *is* recognised
    #[test]
    fn first_party_framework_module_is_recognised() -> anyhow::Result<()> {
        let db = first_party_framework("from basedpython_ui.runtime import State\nx = State(1)\n")?;
        let ty = last_assigned_type(&db);
        let env = db.program_environment();
        let class = ty
            .nominal_class(&db, &env)
            .and_then(|class| class.static_class_literal(&db))
            .map(|(class, _)| class)
            .expect("`State(1)` should infer a nominal instance");
        assert!(class.is_known(&db, KnownClass::BasedpythonUiState));
        assert!(is_observable_instance(&db, &env, ty));
        Ok(())
    }

    #[test]
    fn composable_decorator_marks_the_function() -> anyhow::Result<()> {
        let db = installed_framework(
            "from basedpython_ui import composable\n@composable\ndef view() -> None: ...\n",
        )?;
        let function = last_function_type(&db);
        assert!(function.has_known_decorator(&db, FunctionDecorators::COMPOSABLE));
        assert!(is_composable(&db, function));
        assert_eq!(
            function_framework_role(&db, function),
            Some(FunctionFrameworkRole::Composable)
        );
        Ok(())
    }

    #[test]
    fn first_party_composable_decorator_is_recognised() -> anyhow::Result<()> {
        let db = first_party_framework(
            "from basedpython_ui import composable\n@composable\ndef view() -> None: ...\n",
        )?;
        let function = last_function_type(&db);
        assert!(is_composable(&db, function));
        Ok(())
    }

    #[test]
    fn undecorated_function_is_not_composable() -> anyhow::Result<()> {
        let db = installed_framework(
            "from basedpython_ui import composable\ndef view() -> None: ...\n",
        )?;
        let function = last_function_type(&db);
        assert!(!is_composable(&db, function));
        assert_eq!(function_framework_role(&db, function), None);
        Ok(())
    }
}
