//! how many places use each declaration of a document, and how many classes
//! subclass it or members override it: the counts an editor draws above each
//! declaration ("3 usages", "2 overrides"), for the whole document at once
//!
//! a count is what asking of the declaration's name answers, found by the same
//! code: [`crate::find_references()`] without the declaration for the usages,
//! and [`crate::goto_implementation()`] less the declaration itself for the
//! subclasses and overrides. so the count over a declaration is the number of
//! places its click goes to
//!
//! each of those is a search of every file of the project, which asked of each
//! declaration in turn reads every file once per declaration. the counts are
//! found instead with one pass over the files for all the declarations of the
//! document: each file is read once, and searched for each declaration whose
//! name it holds, with the same search, so each count is the same
//!
//! the declarations counted are the document's outline ([`crate::document_symbols()`])
//! at the depth a reader scans a module at: its classes and functions, and the
//! methods and properties of those classes. a dunder is left out, being called
//! by the language rather than by name, and so is an `extension` block, which
//! names a class it does not declare. a nested function or a class inside a
//! class is not counted

use ruff_db::parsed::parsed_module;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::helpers::is_dunder;
use ruff_text_size::TextRange;
use ty_python_core::ProgramFile;
use ty_python_semantic::types::ide_support::{class_member_definitions, overridden_members};

use crate::find_references::find_references_to_each;
use crate::goto_implementation::implementations_at_each;
use crate::symbols::symbols_for_file;
use crate::{Db, SymbolKind};

/// which counts to work out: each is a search of the workspace
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageCountKinds {
    /// how many places use each declaration
    pub references: bool,
    /// how many classes subclass each class, and how many members override each
    /// method
    pub implementations: bool,
}

/// what a declaration is, for the counts it has: a function is neither
/// subclassed nor overridden
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationKind {
    Class,
    Function,
    /// a method or a property of a class
    Method,
}

/// a declaration of the document, and its counts
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationUsages {
    pub name: String,
    pub kind: DeclarationKind,
    /// the whole declaration, its decorators included
    pub full_range: TextRange,
    /// its name where it is declared
    pub name_range: TextRange,
    /// how many places use it, the declaration itself not among them. `None`
    /// when not asked for
    pub references: Option<usize>,
    /// how many classes subclass a class, directly or not, and how many members
    /// of those override a method. `None` when not asked for, and for a function
    pub implementations: Option<usize>,
    /// whether a method is abstract, an `@abstractmethod` or a protocol member
    /// with no body, so that what overrides it implements it
    pub is_abstract: bool,
}

/// the declarations of `file` with the counts `kinds` asks for, in source order
pub fn document_usage_counts(
    db: &dyn Db,
    file: ProgramFile<'_>,
    kinds: UsageCountKinds,
) -> Vec<DeclarationUsages> {
    let declarations = counted_declarations(db, file);
    if !kinds.references && !kinds.implementations {
        return declarations;
    }
    let source_file = file.file(db);
    let mut declarations = declarations;

    if kinds.references {
        let offsets: Vec<_> = declarations
            .iter()
            .map(|declaration| declaration.name_range.start())
            .collect();
        let found = find_references_to_each(db, file, &offsets, false);
        for (declaration, references) in declarations.iter_mut().zip(found) {
            declaration.references = Some(references.map_or(0, |references| references.len()));
        }
    }
    if kinds.implementations {
        // a function is neither subclassed nor overridden
        let (offsets, counted): (Vec<_>, Vec<_>) = declarations
            .iter()
            .enumerate()
            .filter(|(_, declaration)| declaration.kind != DeclarationKind::Function)
            .map(|(index, declaration)| (declaration.name_range.start(), index))
            .unzip();
        let found = implementations_at_each(db, file, &offsets);
        for (index, implementations) in counted.into_iter().zip(found) {
            let declaration = &mut declarations[index];
            declaration.implementations = Some(implementations.map_or(0, |found| {
                found
                    .iter()
                    .filter(|target| {
                        // the declaration is its own first implementation
                        target.file() != source_file
                            || target.focus_range() != declaration.name_range
                    })
                    .count()
            }));
        }
    }

    // abstract or not only changes how a method's overrides are named, so it is
    // only worked out for one that has some
    if declarations.iter().any(|declaration| {
        declaration.kind == DeclarationKind::Method
            && declaration.implementations.is_some_and(|count| count > 0)
    }) {
        let members = class_member_definitions(db, file);
        for declaration in &mut declarations {
            if declaration.kind == DeclarationKind::Method
                && declaration.implementations.is_some_and(|count| count > 0)
            {
                declaration.is_abstract = members
                    .iter()
                    .filter(|member| member.name_range == declaration.name_range)
                    .any(|member| overridden_members(db, member).is_abstract);
            }
        }
    }
    declarations
}

/// the declarations of `file` that are counted, with no counts yet
fn counted_declarations(db: &dyn Db, file: ProgramFile<'_>) -> Vec<DeclarationUsages> {
    let module = parsed_module(db, file.python_file(db)).load(db);
    let symbols = symbols_for_file(db, file).to_hierarchical();
    let mut declarations = Vec::new();

    let mut add = |name: &str, kind, full_range, name_range| {
        if !is_dunder(name) {
            declarations.push(DeclarationUsages {
                name: name.to_string(),
                kind,
                full_range,
                name_range,
                references: None,
                implementations: None,
                is_abstract: false,
            });
        }
    };

    for (id, symbol) in symbols.iter() {
        match symbol.kind {
            SymbolKind::Function => {
                add(
                    &symbol.name,
                    DeclarationKind::Function,
                    symbol.full_range,
                    symbol.name_range,
                );
            }
            SymbolKind::Class => {
                let is_extension = matches!(
                    covering_node(module.syntax().into(), symbol.full_range).node(),
                    AnyNodeRef::StmtClassDef(class) if class.is_extension()
                );
                if !is_extension {
                    add(
                        &symbol.name,
                        DeclarationKind::Class,
                        symbol.full_range,
                        symbol.name_range,
                    );
                }
                for (_, member) in symbols.children(id) {
                    if matches!(
                        member.kind,
                        SymbolKind::Method | SymbolKind::Constructor | SymbolKind::Property
                    ) {
                        add(
                            &member.name,
                            DeclarationKind::Method,
                            member.full_range,
                            member.name_range,
                        );
                    }
                }
            }
            _ => {}
        }
    }
    declarations.sort_by_key(|declaration| declaration.full_range.start());
    declarations
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use crate::tests::{CursorTest, cursor_test};
    use insta::assert_snapshot;
    use ruff_db::source::source_text;
    use ruff_source_file::LineIndex;

    use super::{DeclarationKind, UsageCountKinds, document_usage_counts};

    impl CursorTest {
        /// one line per declaration of the cursor's file: its kind, its name,
        /// the line its name is on, then `refs=` and `impls=` with each count
        /// asked for, and `(abstract)` for an abstract method
        fn usage_counts(&self) -> String {
            let file = self.cursor.file;
            let source = source_text(&self.db, file);
            let index = LineIndex::from_source_text(&source);
            let declarations = document_usage_counts(
                &self.db,
                self.program_file(file),
                UsageCountKinds {
                    references: true,
                    implementations: true,
                },
            );
            if declarations.is_empty() {
                return "[]".to_string();
            }
            declarations
                .iter()
                .map(|declaration| {
                    let kind = match declaration.kind {
                        DeclarationKind::Class => "class",
                        DeclarationKind::Function => "function",
                        DeclarationKind::Method => "method",
                    };
                    let mut line = format!(
                        "{kind} {} {}",
                        declaration.name,
                        index.line_index(declaration.name_range.start())
                    );
                    if let Some(references) = declaration.references {
                        let _ = write!(line, " refs={references}");
                    }
                    if let Some(implementations) = declaration.implementations {
                        let _ = write!(line, " impls={implementations}");
                    }
                    if declaration.is_abstract {
                        line.push_str(" (abstract)");
                    }
                    line
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    #[test]
    fn each_declaration_is_counted() {
        let test = cursor_test(
            "<CURSOR>
from abc import ABC, abstractmethod

class Animal(ABC):
    @abstractmethod
    def speak(self) -> str: ...

    def name(self) -> str:
        return 'animal'

class Dog(Animal):
    def speak(self) -> str:
        return 'woof'

class Puppy(Dog):
    def speak(self) -> str:
        return 'yip'

def make(animal: Animal) -> str:
    return animal.speak() + animal.name()

make(Dog())
make(Puppy())
",
        );
        assert_snapshot!(test.usage_counts(), @"
        class Animal 4 refs=2 impls=2
        method speak 6 refs=1 impls=2 (abstract)
        method name 8 refs=1 impls=0
        class Dog 11 refs=2 impls=1
        method speak 12 refs=0 impls=1
        class Puppy 15 refs=1 impls=0
        method speak 16 refs=0 impls=0
        function make 19 refs=2
        ");
    }

    #[test]
    fn usages_in_other_files_are_counted() {
        let test = CursorTest::builder()
            .source(
                "lib.py",
                "<CURSOR>
class Shape:
    def area(self) -> float:
        return 0.0

def unused() -> None: ...
",
            )
            .source(
                "main.py",
                "
from lib import Shape

class Square(Shape):
    def area(self) -> float:
        return 1.0

print(Shape().area(), Square().area())
",
            )
            .build();
        assert_snapshot!(test.usage_counts(), @"
        class Shape 2 refs=2 impls=1
        method area 3 refs=1 impls=1
        function unused 6 refs=0
        ");
    }

    #[test]
    fn dunders_nested_functions_and_extensions_are_not_counted() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "<CURSOR>
class A:
    def __init__(self) -> None: ...

    def f(self) -> None:
        def inner() -> None: ...
        inner()

extension list:
    def second(self) -> int:
        return 0
",
            )
            .build();
        assert_snapshot!(test.usage_counts(), @"
        class A 2 refs=0 impls=0
        method f 5 refs=0 impls=0
        method second 10 refs=0 impls=0
        ");
    }
}
