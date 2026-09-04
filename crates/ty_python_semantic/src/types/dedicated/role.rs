//! framework role classification — the single query answering "is this
//! class one that a supported framework transforms at runtime"
//!
//! both checker features (member synthesis, dedicated diagnostics) and the
//! transpiler (lowering compatibility gates via `TypeInfo`) consult this
//! query, so a class is classified in exactly one place. detection is
//! semantic — the resolved mro through the type system — never
//! import-string matching, so aliased imports and inheritance chains all
//! classify correctly
//!
//! the enum grows one variant per framework as its support session lands
//! (`SqlalchemyDeclarative`, `DjangoModel`); see
//! `docs/basedpython/frameworks/index.md`

use crate::Db;
use crate::types::class::CodeGeneratorKind;
use crate::types::dedicated::{basedpython_ui, django, pydantic, pytest, sqlalchemy};
use crate::types::enums::is_enum_class;
use crate::types::{ClassLiteral, FunctionType, StaticClassLiteral, Type};

/// the kind of framework class-transformer that applies to a class
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, salsa::SalsaValue, get_size2::GetSize)]
pub enum FrameworkRole {
    /// a pydantic model — `pydantic.BaseModel` in the mro
    PydanticModel,
    /// a django model — `django.db.models.Model` in the mro
    DjangoModel,
    /// a sqlalchemy 2.0 declarative model — `sqlalchemy.orm.DeclarativeBase`
    /// in the mro (and not a `MappedAsDataclass` model)
    SqlalchemyDeclarative,
}

/// classify `class` against the supported frameworks. `None` for an
/// ordinary class
pub fn class_framework_role<'db>(
    db: &'db dyn Db,
    class: ClassLiteral<'db>,
) -> Option<FrameworkRole> {
    static_class_framework_role(db, class.as_static()?)
}

#[salsa::tracked(returns(copy), heap_size=ruff_memory_usage::heap_size)]
fn static_class_framework_role<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Option<FrameworkRole> {
    if pydantic::is_model(db, class) {
        return Some(FrameworkRole::PydanticModel);
    }
    if django::is_model(db, class) {
        return Some(FrameworkRole::DjangoModel);
    }
    if sqlalchemy::is_declarative(db, class) {
        return Some(FrameworkRole::SqlalchemyDeclarative);
    }
    None
}

/// whether adding a type annotation to a bare `name = value` assignment in
/// `class`'s body would change the class's runtime semantics. true for
/// dataclass-like classes and framework models (pydantic / django /
/// sqlalchemy), `NamedTuple`s and `TypedDict`s — where an annotated
/// assignment turns a plain class variable into a field — and for enums,
/// where a bare assignment defines a member. the inferred-annotation
/// transform must leave such classes' body assignments alone
pub fn class_body_annotation_is_semantic<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> bool {
    CodeGeneratorKind::from_class(db, class).is_some()
        || is_enum_class(db, Type::ClassLiteral(class))
}

/// the kind of pytest function whose parameters pytest fills from the fixture
/// registry — the parallel of [`FrameworkRole`] for function-level frameworks
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, salsa::SalsaValue, get_size2::GetSize)]
pub(crate) enum FunctionFrameworkRole {
    /// a pytest fixture — a function decorated with `@pytest.fixture`
    PytestFixture,
    /// a pytest test — a `test*` function in a collected test file
    PytestTest,
    /// a basedpython-ui composable — a function decorated with the framework's
    /// `@composable`, whose body is a composition scope
    Composable,
}

impl FunctionFrameworkRole {
    /// whether pytest manages this function — fills its parameters from the
    /// fixture registry and reads its `parametrize` markers. The pytest checks
    /// gate on this rather than on "has any role", since a composable's
    /// parameters are ordinary ones
    pub(crate) const fn is_pytest(self) -> bool {
        match self {
            Self::PytestFixture | Self::PytestTest => true,
            Self::Composable => false,
        }
    }
}

/// classify `function` against the supported function-level frameworks.
/// `None` for an ordinary function. a function that is both a fixture and
/// named like a test classifies as a fixture: the decorator is explicit,
/// the name convention is incidental
#[salsa::tracked(returns(copy), heap_size = ruff_memory_usage::heap_size)]
pub fn function_framework_role<'db>(
    db: &'db dyn Db,
    function: FunctionType<'db>,
) -> Option<FunctionFrameworkRole> {
    if pytest::is_fixture_function(db, function) {
        return Some(FunctionFrameworkRole::PytestFixture);
    }
    if pytest::is_test_function(db, function) {
        return Some(FunctionFrameworkRole::PytestTest);
    }
    if basedpython_ui::is_composable(db, function) {
        return Some(FunctionFrameworkRole::Composable);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::TestDbBuilder;
    use crate::{HasType, SemanticModel};
    use ruff_db::files::system_path_to_file;
    use ruff_db::parsed::parsed_module;

    /// resolve the framework role of the last class defined in `/src/main.py`
    fn role_of_last_class(source: &str) -> anyhow::Result<Option<FrameworkRole>> {
        // BaseModel is only recognized when `pydantic.main` resolves from a
        // third-party search path, so the mock package lives in site-packages
        let db = TestDbBuilder::new()
            .with_site_packages("/sp")
            .with_file(
                "/sp/pydantic/__init__.pyi",
                "from pydantic.main import BaseModel as BaseModel\n",
            )
            .with_file("/sp/pydantic/main.pyi", "class BaseModel: ...\n")
            .with_file("/src/main.py", source)
            .build()?;

        let file = system_path_to_file(&db, "/src/main.py")?;
        let module = parsed_module(&db, db.program_file(file).python_file(&db)).load(&db);
        let model = SemanticModel::new(&db, crate::Db::program_file(&db, file));
        let class_def = module
            .suite()
            .iter()
            .rev()
            .find_map(|stmt| stmt.as_class_def_stmt())
            .expect("source should define a class");
        let ty = class_def
            .inferred_type(&model)
            .expect("class should infer a type");
        let class = ty
            .as_class_literal()
            .expect("class definition should infer a class literal");
        Ok(class_framework_role(&db, class))
    }

    /// resolve the framework role of the last class in `/src/main.py`, with a
    /// mock django package in site-packages (`Model` must be *defined* in
    /// `django.db.models.base` on a third-party search path to be recognized)
    fn django_role_of_last_class(source: &str) -> anyhow::Result<Option<FrameworkRole>> {
        let db = TestDbBuilder::new()
            .with_site_packages("/sp")
            .with_file("/sp/django/__init__.pyi", "")
            .with_file("/sp/django/db/__init__.pyi", "")
            .with_file(
                "/sp/django/db/models/__init__.pyi",
                "from django.db.models.base import Model as Model\n",
            )
            .with_file("/sp/django/db/models/base.pyi", "class Model: ...\n")
            .with_file("/src/main.py", source)
            .build()?;

        let file = system_path_to_file(&db, "/src/main.py")?;
        let module = parsed_module(&db, db.program_file(file).python_file(&db)).load(&db);
        let model = SemanticModel::new(&db, crate::Db::program_file(&db, file));
        let class_def = module
            .suite()
            .iter()
            .rev()
            .find_map(|stmt| stmt.as_class_def_stmt())
            .expect("source should define a class");
        let ty = class_def
            .inferred_type(&model)
            .expect("class should infer a type");
        let class = ty
            .as_class_literal()
            .expect("class definition should infer a class literal");
        Ok(class_framework_role(&db, class))
    }

    #[test]
    fn django_model_direct_base() -> anyhow::Result<()> {
        let role = django_role_of_last_class(
            "from django.db import models\nclass Author(models.Model):\n    pass\n",
        )?;
        assert_eq!(role, Some(FrameworkRole::DjangoModel));
        Ok(())
    }

    #[test]
    fn django_model_through_inheritance() -> anyhow::Result<()> {
        let role = django_role_of_last_class(
            "from django.db.models import Model\nclass Base(Model): ...\nclass Author(Base):\n    pass\n",
        )?;
        assert_eq!(role, Some(FrameworkRole::DjangoModel));
        Ok(())
    }

    #[test]
    fn ordinary_class_with_django_imported_has_no_role() -> anyhow::Result<()> {
        let role =
            django_role_of_last_class("from django.db import models\nclass Author:\n    pass\n")?;
        assert_eq!(role, None);
        Ok(())
    }

    /// resolve the framework role of the last class in `/src/main.py`, with a
    /// mock sqlalchemy package in site-packages (`DeclarativeBase` must be
    /// *defined* in `sqlalchemy.orm.decl_api`, `Mapped` in `sqlalchemy.orm.base`,
    /// both on a third-party search path)
    fn sqlalchemy_role_of_last_class(source: &str) -> anyhow::Result<Option<FrameworkRole>> {
        let db = TestDbBuilder::new()
            .with_site_packages("/sp")
            .with_file("/sp/sqlalchemy/__init__.pyi", "")
            .with_file(
                "/sp/sqlalchemy/orm/__init__.pyi",
                "from sqlalchemy.orm.decl_api import DeclarativeBase as DeclarativeBase\n\
                 from sqlalchemy.orm.decl_api import MappedAsDataclass as MappedAsDataclass\n\
                 from sqlalchemy.orm.base import Mapped as Mapped\n",
            )
            .with_file(
                "/sp/sqlalchemy/orm/decl_api.pyi",
                "class DeclarativeBase: ...\nclass MappedAsDataclass: ...\n",
            )
            .with_file("/sp/sqlalchemy/orm/base.pyi", "class Mapped[T]: ...\n")
            .with_file("/src/main.py", source)
            .build()?;

        let file = system_path_to_file(&db, "/src/main.py")?;
        let module = parsed_module(&db, db.program_file(file).python_file(&db)).load(&db);
        let model = SemanticModel::new(&db, crate::Db::program_file(&db, file));
        let class_def = module
            .suite()
            .iter()
            .rev()
            .find_map(|stmt| stmt.as_class_def_stmt())
            .expect("source should define a class");
        let ty = class_def
            .inferred_type(&model)
            .expect("class should infer a type");
        let class = ty
            .as_class_literal()
            .expect("class definition should infer a class literal");
        Ok(class_framework_role(&db, class))
    }

    #[test]
    fn sqlalchemy_declarative_direct_base() -> anyhow::Result<()> {
        let role = sqlalchemy_role_of_last_class(
            "from sqlalchemy.orm import DeclarativeBase\nclass User(DeclarativeBase):\n    pass\n",
        )?;
        assert_eq!(role, Some(FrameworkRole::SqlalchemyDeclarative));
        Ok(())
    }

    #[test]
    fn sqlalchemy_declarative_through_inheritance() -> anyhow::Result<()> {
        let role = sqlalchemy_role_of_last_class(
            "from sqlalchemy.orm import DeclarativeBase\nclass Base(DeclarativeBase): ...\nclass User(Base):\n    pass\n",
        )?;
        assert_eq!(role, Some(FrameworkRole::SqlalchemyDeclarative));
        Ok(())
    }

    #[test]
    fn sqlalchemy_mapped_as_dataclass_is_not_declarative_role() -> anyhow::Result<()> {
        // a `MappedAsDataclass` model goes through the dataclass path, not the
        // declarative constructor synthesis, so it has no declarative role
        let role = sqlalchemy_role_of_last_class(
            "from sqlalchemy.orm import DeclarativeBase, MappedAsDataclass\nclass Base(MappedAsDataclass, DeclarativeBase): ...\nclass User(Base):\n    pass\n",
        )?;
        assert_eq!(role, None);
        Ok(())
    }

    #[test]
    fn ordinary_class_with_sqlalchemy_imported_has_no_role() -> anyhow::Result<()> {
        let role = sqlalchemy_role_of_last_class(
            "from sqlalchemy.orm import DeclarativeBase\nclass User:\n    pass\n",
        )?;
        assert_eq!(role, None);
        Ok(())
    }

    #[test]
    fn pydantic_model_direct_base() -> anyhow::Result<()> {
        let role = role_of_last_class(
            "from pydantic import BaseModel\nclass User(BaseModel):\n    name: str\n",
        )?;
        assert_eq!(role, Some(FrameworkRole::PydanticModel));
        Ok(())
    }

    #[test]
    fn pydantic_model_through_inheritance_and_alias() -> anyhow::Result<()> {
        let role = role_of_last_class(
            "from pydantic import BaseModel as BM\nclass Base(BM): ...\nclass User(Base):\n    name: str\n",
        )?;
        assert_eq!(role, Some(FrameworkRole::PydanticModel));
        Ok(())
    }

    #[test]
    fn ordinary_class_has_no_role() -> anyhow::Result<()> {
        let role = role_of_last_class("import pydantic\nclass User:\n    name: str\n")?;
        assert_eq!(role, None);
        Ok(())
    }

    #[test]
    fn first_party_module_named_pydantic_is_not_recognized() -> anyhow::Result<()> {
        // a first-party `pydantic` shadows the installed one; its BaseModel
        // resolves from the first-party search path and must not classify
        let db = TestDbBuilder::new()
            .with_site_packages("/sp")
            .with_file("/sp/pydantic/main.pyi", "class BaseModel: ...\n")
            .with_file("/src/pydantic/__init__.py", "")
            .with_file("/src/pydantic/main.py", "class BaseModel: ...\n")
            .with_file(
                "/src/main.py",
                "from pydantic.main import BaseModel\nclass User(BaseModel): ...\n",
            )
            .build()?;
        let file = system_path_to_file(&db, "/src/main.py")?;
        let module = parsed_module(&db, db.program_file(file).python_file(&db)).load(&db);
        let model = SemanticModel::new(&db, crate::Db::program_file(&db, file));
        let class_def = module
            .suite()
            .iter()
            .find_map(|stmt| stmt.as_class_def_stmt())
            .expect("source should define a class");
        let class = class_def
            .inferred_type(&model)
            .and_then(crate::types::Type::as_class_literal)
            .expect("class definition should infer a class literal");
        assert_eq!(class_framework_role(&db, class), None);
        Ok(())
    }

    /// resolve the function-level framework role of the last top-level
    /// function in `target`, with a mock pytest package in site-packages
    /// (`fixture` must be *defined* in `_pytest.fixtures` on a third-party
    /// search path to be recognized)
    fn pytest_function_role(
        files: &[(&str, &str)],
        target: &str,
    ) -> anyhow::Result<Option<FunctionFrameworkRole>> {
        use crate::types::infer_definition_types;
        use ty_python_core::semantic_index;

        let mut builder = TestDbBuilder::new()
            .with_site_packages("/sp")
            .with_file("/sp/pytest/__init__.pyi", "from _pytest.fixtures import fixture as fixture\n")
            .with_file("/sp/_pytest/__init__.pyi", "")
            .with_file(
                "/sp/_pytest/fixtures.pyi",
                "class FixtureFunctionDefinition: ...\n\
                 def fixture(function=..., *, scope: str = ..., name: str | None = None) -> FixtureFunctionDefinition: ...\n",
            );
        for (path, source) in files {
            builder = builder.with_file(path, source);
        }
        let db = builder.build()?;

        let file = system_path_to_file(&db, target)?;
        let module = parsed_module(&db, db.program_file(file).python_file(&db)).load(&db);
        let index = semantic_index(&db, db.program_file(file));
        let function_node = module
            .suite()
            .iter()
            .rev()
            .find_map(|stmt| stmt.as_function_def_stmt())
            .expect("source should define a function");
        let definition = index.expect_single_definition(function_node);
        let function = infer_definition_types(&db, definition)
            .function_type(definition)
            .expect("function definition should infer a function type");
        Ok(function_framework_role(&db, function))
    }

    #[test]
    fn pytest_bare_fixture() -> anyhow::Result<()> {
        let role = pytest_function_role(
            &[(
                "/src/conftest.py",
                "import pytest\n@pytest.fixture\ndef db() -> int:\n    return 1\n",
            )],
            "/src/conftest.py",
        )?;
        assert_eq!(role, Some(FunctionFrameworkRole::PytestFixture));
        Ok(())
    }

    #[test]
    fn pytest_called_fixture() -> anyhow::Result<()> {
        let role = pytest_function_role(
            &[(
                "/src/conftest.py",
                "import pytest\n@pytest.fixture(scope=\"session\")\ndef db() -> int:\n    return 1\n",
            )],
            "/src/conftest.py",
        )?;
        assert_eq!(role, Some(FunctionFrameworkRole::PytestFixture));
        Ok(())
    }

    #[test]
    fn pytest_module_level_test() -> anyhow::Result<()> {
        let role = pytest_function_role(
            &[("/src/test_it.py", "def test_answer() -> None:\n    pass\n")],
            "/src/test_it.py",
        )?;
        assert_eq!(role, Some(FunctionFrameworkRole::PytestTest));
        Ok(())
    }

    #[test]
    fn pytest_test_named_function_in_ordinary_file_has_no_role() -> anyhow::Result<()> {
        let role = pytest_function_role(
            &[("/src/helpers.py", "def test_answer() -> None:\n    pass\n")],
            "/src/helpers.py",
        )?;
        assert_eq!(role, None);
        Ok(())
    }

    #[test]
    fn pytest_ordinary_function_in_test_file_has_no_role() -> anyhow::Result<()> {
        let role = pytest_function_role(
            &[("/src/test_it.py", "def helper() -> None:\n    pass\n")],
            "/src/test_it.py",
        )?;
        assert_eq!(role, None);
        Ok(())
    }
}
