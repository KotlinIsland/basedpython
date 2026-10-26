//! the members each class of a document inherits: what an editor's structure view lists under a
//! class when it shows inherited members
//!
//! ```py
//! class Shape:
//!     name: str
//!
//!     def __init__(self, name: str) -> None:
//!         self.name = name
//!
//!     def area(self) -> float: ...
//!
//!     def describe(self) -> str: ...
//!
//! class Circle(Shape):
//!     def __init__(self, radius: float) -> None:
//!         self.radius = radius
//!
//!     def area(self) -> float: ...
//! ```
//!
//! `Circle` inherits `name` and `describe` from `Shape`, and the members of `object` that neither
//! declares: the members each class of its method resolution order declares, as that class's
//! outline lists them, attributes a method assigns on `self` among them. a name is listed once,
//! from the first class in that order to declare it, and not at all when the class declares it
//! itself, as PyCharm's structure view lists them

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::name::Name;
use ruff_text_size::TextRange;
use rustc_hash::{FxHashMap, FxHashSet};
use ty_python_core::ProgramFile;
use ty_python_semantic::{HasDefinition, SemanticModel, class_definition_mro};

use crate::symbols::symbols_for_file;
use crate::{Db, HierarchicalSymbols, SymbolId, SymbolKind, symbol_detail};

/// the members a class of the document inherits
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassInheritedMembers {
    /// the class's name where the document declares it
    pub name_range: TextRange,
    pub members: Vec<InheritedMember>,
}

/// a member a class inherits, as the outline of the superclass that declares it lists it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritedMember {
    pub name: String,
    pub kind: SymbolKind,
    /// what the outline shows beside the name: a method's parameters, a nested class's bases
    pub detail: Option<String>,
    /// the superclass that declares it
    pub superclass: Name,
    /// the module the superclass is declared in, when a module name reaches it
    pub superclass_module: Option<String>,
    pub file: File,
    /// the whole declaration
    pub full_range: TextRange,
    /// its name
    pub name_range: TextRange,
}

/// the inherited members of each class `file`'s outline holds, nested classes among them, in
/// source order. a class inheriting nothing but what it declares itself is there with no members
pub fn document_inherited_members(
    db: &dyn Db,
    file: ProgramFile<'_>,
) -> Vec<ClassInheritedMembers> {
    let symbols = symbols_for_file(db, file).to_hierarchical();
    let module = parsed_module(db, file.python_file(db)).load(db);
    let model = SemanticModel::new(db, file);
    let program = file.program(db);
    let mut outlines: FxHashMap<File, HierarchicalSymbols> = FxHashMap::default();

    let mut classes = Vec::new();
    let mut pending: Vec<SymbolId> = symbols.iter().map(|(id, _)| id).collect();
    pending.reverse();
    while let Some(id) = pending.pop() {
        let Some(symbol) = symbols.get(id) else {
            continue;
        };
        let mut children: Vec<SymbolId> = symbols.children(id).map(|(child, _)| child).collect();
        children.reverse();
        pending.extend(children);
        if symbol.kind != SymbolKind::Class {
            continue;
        }
        let Some(class) = covering_node(module.syntax().into(), symbol.name_range)
            .ancestors()
            .find_map(|node| match node {
                AnyNodeRef::StmtClassDef(class) if class.name.range == symbol.name_range => {
                    Some(class)
                }
                _ => None,
            })
        else {
            continue;
        };

        // what the class declares itself hides what it would inherit under the same name
        let mut seen: FxHashSet<String> = symbols
            .children(id)
            .map(|(_, member)| member.name.to_string())
            .collect();
        let mut members = Vec::new();
        for superclass in class_definition_mro(db, class.definition(&model)) {
            let superclass_file = superclass.file.file(db);
            let superclass_program_file = ProgramFile::new(db, superclass_file, program);
            let flat = symbols_for_file(db, superclass_program_file);
            let Some(superclass_id) = flat.iter().find_map(|(id, candidate)| {
                (candidate.kind == SymbolKind::Class
                    && candidate.name_range == superclass.selection_range)
                    .then_some(id)
            }) else {
                continue;
            };
            let outline = outlines
                .entry(superclass_file)
                .or_insert_with(|| flat.to_hierarchical());
            let superclass_module = ty_module_resolver::file_to_module(db, superclass.file)
                .map(|module| module.name(db).to_string());
            for (_, member) in outline.children(superclass_id) {
                if !seen.insert(member.name.to_string()) {
                    continue;
                }
                members.push(InheritedMember {
                    name: member.name.to_string(),
                    kind: member.kind,
                    detail: symbol_detail(db, superclass_program_file, &member),
                    superclass: superclass.name.clone(),
                    superclass_module: superclass_module.clone(),
                    file: superclass_file,
                    full_range: member.full_range,
                    name_range: member.name_range,
                });
            }
        }
        classes.push(ClassInheritedMembers {
            name_range: symbol.name_range,
            members,
        });
    }
    classes
}

#[cfg(test)]
mod tests {
    use crate::tests::CursorTest;
    use crate::{SymbolKind, document_inherited_members};
    use insta::assert_snapshot;
    use ruff_db::source::{line_index, source_text};

    impl CursorTest {
        /// for each class, its name, then one line per inherited member: its kind, its name, its
        /// detail, and the superclass and module it is declared in. `object`'s members are counted
        /// rather than listed
        fn inherited_members(&self) -> String {
            let file = self.program_file(self.cursor.file);
            let source = source_text(&self.db, self.cursor.file);
            let index = line_index(&self.db, self.cursor.file);
            let mut out = Vec::new();
            for class in document_inherited_members(&self.db, file) {
                out.push(format!(
                    "{} (line {})",
                    &source[class.name_range],
                    index.line_index(class.name_range.start())
                ));
                let mut from_object = 0;
                for member in &class.members {
                    if member.superclass.as_str() == "object" {
                        from_object += 1;
                        continue;
                    }
                    let kind = match member.kind {
                        SymbolKind::Method => "method",
                        SymbolKind::Constructor => "constructor",
                        SymbolKind::Property => "property",
                        SymbolKind::Field => "field",
                        SymbolKind::Class => "class",
                        _ => "other",
                    };
                    out.push(format!(
                        "  {kind} {}{} <- {} in {}",
                        member.name,
                        member.detail.as_deref().unwrap_or_default(),
                        member.superclass,
                        member.superclass_module.as_deref().unwrap_or("?"),
                    ));
                }
                out.push(format!("  and {from_object} from object"));
            }
            out.join("\n")
        }
    }

    #[test]
    fn each_class_lists_what_it_inherits_and_does_not_declare() {
        let test = CursorTest::builder()
            .source(
                "shapes.py",
                "
class Shape:
    name: str
    sides = 0

    def __init__(self, name: str) -> None:
        self.name = name
        self.label = name.title()

    def area(self) -> float:
        return 0.0

    @property
    def describe(self) -> str:
        return self.name

    class Meta: ...
",
            )
            .source(
                "main.py",
                "<CURSOR>
from shapes import Shape

class Circle(Shape):
    def __init__(self, radius: float) -> None:
        super().__init__('circle')
        self.radius = radius

    def area(self) -> float:
        return self.radius

class Unit(Circle):
    sides = 1

    class Inner: ...
",
            )
            .build();
        assert_snapshot!(test.inherited_members(), @"
        Circle (line 4)
          field name <- Shape in shapes
          field sides <- Shape in shapes
          property describe(self) -> str <- Shape in shapes
          class Meta <- Shape in shapes
          field label <- Shape in shapes
          and 22 from object
        Unit (line 12)
          constructor __init__(self, radius: float) -> None <- Circle in main
          method area(self) -> float <- Circle in main
          field radius <- Circle in main
          field name <- Shape in shapes
          property describe(self) -> str <- Shape in shapes
          class Meta <- Shape in shapes
          field label <- Shape in shapes
          and 22 from object
        Inner (line 15)
          and 23 from object
        ");
    }
}
