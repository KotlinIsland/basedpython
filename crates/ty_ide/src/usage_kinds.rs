//! What each reference a search finds does with the name: what an editor's *find usages*
//! groups them by.
//!
//! ```py
//! from shapes.base import Shape        # import
//!
//! class Circle(Shape): ...             # superclass
//!
//! def total(shapes: list[Shape]): ...  # type hint
//!
//! if isinstance(item, Shape): ...      # isinstance
//!
//! Shape = make_shape()                 # write
//! print(Shape)                         # read
//! ```
//!
//! These are the groups PyCharm's Python sorts usages into, in the order it tries them: a
//! reference inside an import statement is an import whatever else it is, then the class list of
//! an `isinstance` call, then a type expression, then a class's bases. What is none of those is a
//! read or a write of the name, or the declaration itself.

use ruff_db::parsed::parsed_module;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::{self as ast, AnyNodeRef};
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::ProgramFile;
use ty_python_semantic::SemanticModel;
use ty_python_semantic::types::ide_support::{
    CallArgumentForm, call_argument_forms, calls_isinstance,
};

use crate::django_template::is_django_template_path;
use crate::{Db, ReferenceKind, ReferenceTarget};

/// What a reference does with the name it refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UsageKind {
    /// The name where it is declared: a `def`'s or a `class`'s name, or a first assignment.
    Declaration,
    /// In an `import` or a `from ... import` statement.
    Import,
    /// Among the classes an `isinstance` call tests against: its second argument, or an element
    /// of a tuple there.
    Isinstance,
    /// In a type expression: an annotation, a return or `raises` annotation, a type alias's
    /// value, a type parameter's bound or default, or an argument a call takes as a type, such
    /// as `cast`'s first.
    TypeHint,
    /// In the parentheses of a class's header, among its bases or its keywords.
    Superclass,
    /// Assigning, deleting or otherwise binding the name.
    Write,
    /// Reading the name's value, which calling it is.
    Read,
}

/// What each of `targets` does with the name it refers to, in the same order. `program` is the
/// program the search that found them ran in.
pub fn usage_kinds(
    db: &dyn Db,
    program: ProgramFile<'_>,
    targets: &[ReferenceTarget],
) -> Vec<UsageKind> {
    let program = program.program(db);
    targets
        .iter()
        .map(|target| usage_kind(db, ProgramFile::new(db, target.file(), program), target))
        .collect()
}

fn usage_kind(db: &dyn Db, file: ProgramFile<'_>, target: &ReferenceTarget) -> UsageKind {
    // a django template's names are only read, and its text is no python to classify
    let template = target
        .file()
        .path(db)
        .as_system_path()
        .is_some_and(is_django_template_path);
    (!template)
        .then(|| syntactic_kind(db, file, target.range()))
        .flatten()
        .unwrap_or(match target.kind() {
            ReferenceKind::Read => UsageKind::Read,
            ReferenceKind::Write => UsageKind::Write,
            ReferenceKind::Other => UsageKind::Declaration,
        })
}

/// The group the syntax around `range` puts a reference in, `None` when it is only a read or a
/// write.
fn syntactic_kind(db: &dyn Db, file: ProgramFile<'_>, range: TextRange) -> Option<UsageKind> {
    let module = parsed_module(db, file.python_file(db)).load(db);
    let covering = covering_node(module.syntax().into(), range);
    let ancestors: Vec<AnyNodeRef<'_>> = covering.ancestors().collect();

    if ancestors.iter().any(|node| {
        matches!(
            node,
            AnyNodeRef::StmtImport(_) | AnyNodeRef::StmtImportFrom(_)
        )
    }) {
        return Some(UsageKind::Import);
    }

    let model = SemanticModel::new(db, file);
    let mut kind = None;
    // from the innermost node out, so that the nearest of nested groups wins: a class in a
    // `cast` in a base class list is in a type expression
    for node in &ancestors {
        kind = match node {
            AnyNodeRef::ExprCall(call) => call_argument_kind(&model, call, range),
            AnyNodeRef::Parameter(parameter) => {
                holds(parameter.annotation.as_deref(), range).then_some(UsageKind::TypeHint)
            }
            AnyNodeRef::StmtFunctionDef(function) => {
                let returns = function
                    .returns
                    .as_deref()
                    .filter(|_| !function.is_asserts_return);
                (holds(returns, range) || holds(function.raises.as_deref(), range))
                    .then_some(UsageKind::TypeHint)
            }
            AnyNodeRef::StmtAnnAssign(assignment) => {
                holds(Some(&*assignment.annotation), range).then_some(UsageKind::TypeHint)
            }
            AnyNodeRef::StmtTypeAlias(alias) => {
                holds(Some(&*alias.value), range).then_some(UsageKind::TypeHint)
            }
            AnyNodeRef::TypeParamTypeVar(type_param) => (holds(type_param.bound.as_deref(), range)
                || holds(type_param.default.as_deref(), range))
            .then_some(UsageKind::TypeHint),
            AnyNodeRef::TypeParamParamSpec(type_param) => {
                holds(type_param.default.as_deref(), range).then_some(UsageKind::TypeHint)
            }
            AnyNodeRef::TypeParamTypeVarTuple(type_param) => {
                holds(type_param.default.as_deref(), range).then_some(UsageKind::TypeHint)
            }
            AnyNodeRef::StmtClassDef(class) => class
                .arguments
                .as_deref()
                .is_some_and(|arguments| arguments.range().contains_range(range))
                .then_some(UsageKind::Superclass),
            _ => None,
        };
        // a statement's groups are all inside it
        if kind.is_some() || node.is_statement() {
            break;
        }
    }
    kind
}

/// The group a reference at `range`, inside one of `call`'s arguments or its callee, is in.
fn call_argument_kind(
    model: &SemanticModel<'_>,
    call: &ast::ExprCall,
    range: TextRange,
) -> Option<UsageKind> {
    let (index, argument) = call
        .arguments
        .iter_source_order()
        .enumerate()
        .find(|(_, argument)| argument.range().contains_range(range))?;
    if index == 1 && matches!(argument, ast::ArgOrKeyword::Arg(_)) && calls_isinstance(model, call)
    {
        return Some(UsageKind::Isinstance);
    }
    (call_argument_forms(model, call).get(index) == Some(&CallArgumentForm::Type))
        .then_some(UsageKind::TypeHint)
}

fn holds(expression: Option<&ast::Expr>, range: TextRange) -> bool {
    expression.is_some_and(|expression| expression.range().contains_range(range))
}

#[cfg(test)]
mod tests {
    use crate::tests::CursorTest;
    use crate::{find_references, usage_kinds};
    use insta::assert_snapshot;
    use ruff_db::source::{line_index, source_text};

    impl CursorTest {
        /// each reference to the name at the cursor, the declaration included: its line, its
        /// line's text, and what it does with the name
        fn usage_kinds(&self) -> String {
            let file = self.program_file(self.cursor.file);
            let Some(targets) = find_references(&self.db, file, self.cursor.offset, true) else {
                return "no references".to_string();
            };
            let kinds = usage_kinds(&self.db, file, &targets);
            targets
                .iter()
                .zip(kinds)
                .map(|(target, kind)| {
                    let source = source_text(&self.db, target.file());
                    let index = line_index(&self.db, target.file());
                    let line = index.line_index(target.range().start());
                    let text = &source[index.line_range(line, &source)];
                    format!("{line} {:?}: {}", kind, text.trim())
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    #[test]
    fn each_reference_is_grouped_by_what_it_does() {
        let test = CursorTest::builder()
            .source(
                "shapes.py",
                "
class Sh<CURSOR>ape: ...
",
            )
            .source(
                "main.py",
                "
from typing import cast
from shapes import Shape
import shapes

class Circle(Shape): ...

class Square(shapes.Shape, metaclass=type): ...

def total(items: list[Shape], other: 'Shape') -> Shape | None:
    for item in items:
        if isinstance(item, (Shape, int)):
            return cast(Shape, item)
    print(Shape, isinstance(Shape, type))
    return Shape()

type Shapes = list[Shape]

def first[T: Shape](items: list[T]) -> T:
    return items[0]

Shape = Circle
del Shape
",
            )
            .build();
        assert_snapshot!(test.usage_kinds(), @"
        2 Declaration: class Shape: ...
        3 Import: from shapes import Shape
        6 Superclass: class Circle(Shape): ...
        8 Superclass: class Square(shapes.Shape, metaclass=type): ...
        10 TypeHint: def total(items: list[Shape], other: 'Shape') -> Shape | None:
        10 TypeHint: def total(items: list[Shape], other: 'Shape') -> Shape | None:
        10 TypeHint: def total(items: list[Shape], other: 'Shape') -> Shape | None:
        12 Isinstance: if isinstance(item, (Shape, int)):
        13 TypeHint: return cast(Shape, item)
        14 Read: print(Shape, isinstance(Shape, type))
        14 Read: print(Shape, isinstance(Shape, type))
        15 Read: return Shape()
        17 TypeHint: type Shapes = list[Shape]
        19 TypeHint: def first[T: Shape](items: list[T]) -> T:
        22 Write: Shape = Circle
        23 Write: del Shape
        ");
    }
}
