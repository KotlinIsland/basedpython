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
use crate::types::dedicated::pydantic;
use crate::types::{ClassLiteral, StaticClassLiteral};

/// the kind of framework class-transformer that applies to a class
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, salsa::Update, get_size2::GetSize)]
pub enum FrameworkRole {
    /// a pydantic model — `pydantic.BaseModel` in the mro
    PydanticModel,
}

/// classify `class` against the supported frameworks. `None` for an
/// ordinary class
pub fn class_framework_role<'db>(
    db: &'db dyn Db,
    class: ClassLiteral<'db>,
) -> Option<FrameworkRole> {
    static_class_framework_role(db, class.as_static()?)
}

#[salsa::tracked(heap_size=ruff_memory_usage::heap_size)]
fn static_class_framework_role<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Option<FrameworkRole> {
    if pydantic::is_model(db, class) {
        return Some(FrameworkRole::PydanticModel);
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
        let module = parsed_module(&db, file).load(&db);
        let model = SemanticModel::new(&db, file);
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
        let module = parsed_module(&db, file).load(&db);
        let model = SemanticModel::new(&db, file);
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
}
