//! The class a dotted type name names: what a debugger's *jump to type source* asks.
//!
//! A debugger knows a value's type by name, `type(value).__module__` and its `__qualname__`:
//! `shapes.base.Circle`, or `shapes.base.Shape.Meta` for a class nested in another. The name is
//! resolved as an import of it would be — its module from the search paths, then each name after it
//! as a member of what came before, through re-exports and submodules — so `builtins.list` is the
//! class typeshed declares and `fastapi.FastAPI` the one its package does. A name that reaches
//! something other than a class, or nothing, as `make.<locals>.Local` does, is not answered.

use ty_module_resolver::ModuleName;
use ty_python_core::ProgramFile;
use ty_python_semantic::SemanticModel;
use ty_python_semantic::types::Type;

use crate::Db;
use crate::declaration::{DeclarationAt, single_declaration};
use crate::goto::Definitions;

/// The class `type_name` names, as a module of the program `context` is in would import it, with
/// its declaration where its source is, or its stub for a class that has no other.
pub fn type_declaration(
    db: &dyn Db,
    context: ProgramFile<'_>,
    type_name: &str,
) -> Option<DeclarationAt> {
    let model = SemanticModel::new(db, context);
    let mut parts = type_name.split('.');
    let module = model.resolve_module_name(&ModuleName::new(parts.next()?)?)?;
    let path: Vec<&str> = parts.collect();
    let ty = model.path_type(module, &path)?;
    if !matches!(ty, Type::ClassLiteral(_)) {
        return None;
    }
    let definitions = Definitions::from_ty(db, &model.program_environment(), ty)?.map_stubs(db);
    let declaration = single_declaration(db, &definitions)?;
    let target = definitions.into_navigation_targets(db).into_iter().next()?;
    Some(DeclarationAt {
        declaration,
        target,
    })
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use ruff_db::source::source_text;
    use ruff_source_file::LineIndex;

    use crate::tests::CursorTest;

    impl CursorTest {
        fn type_declaration(&self, type_name: &str) -> String {
            let Some(found) =
                super::type_declaration(&self.db, self.program_file(self.cursor.file), type_name)
            else {
                return "None".to_string();
            };
            let file = found.target.file();
            let source = source_text(&self.db, file);
            let line =
                LineIndex::from_source_text(&source).line_index(found.target.focus_range().start());
            format!(
                "{} {} at {}:{line}",
                found.declaration.kind.as_str(),
                found.declaration.qualified_name.as_deref().unwrap_or("-"),
                file.path(&self.db).as_str(),
            )
        }
    }

    fn test() -> CursorTest {
        CursorTest::builder()
            .source("shapes/__init__.py", "from .base import Circle as Circle\n")
            .source(
                "shapes/base.py",
                "\
class Shape:
    class Meta: ...

class Circle(Shape): ...

RADIUS = 1.0

def make() -> None:
    class Local: ...
",
            )
            .source("main.py", "<CURSOR>")
            .build()
    }

    #[test]
    fn a_class_is_found_by_its_module_and_qualified_name() {
        let test = test();
        assert_snapshot!(test.type_declaration("shapes.base.Circle"), @"class shapes.base.Circle at /shapes/base.py:4");
        assert_snapshot!(test.type_declaration("shapes.base.Shape.Meta"), @"class shapes.base.Shape.Meta at /shapes/base.py:2");
    }

    #[test]
    fn a_class_is_found_through_a_re_export() {
        assert_snapshot!(test().type_declaration("shapes.Circle"), @"class shapes.base.Circle at /shapes/base.py:4");
    }

    #[test]
    fn a_builtin_is_its_stub() {
        assert_snapshot!(test().type_declaration("builtins.list"), @"class builtins.list at stdlib/builtins.byi:2659");
    }

    #[test]
    fn what_is_not_a_class_is_not_answered() {
        let test = test();
        assert_snapshot!(test.type_declaration("shapes.base.RADIUS"), @"None");
        assert_snapshot!(test.type_declaration("shapes.base"), @"None");
        assert_snapshot!(test.type_declaration("shapes.base.make.<locals>.Local"), @"None");
        assert_snapshot!(test.type_declaration("nowhere.Thing"), @"None");
    }
}
