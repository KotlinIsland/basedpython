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
//! methods and properties of those classes. a dunder method is counted for its
//! overrides alone, `__eq__` overridden like any method: its usages are not
//! counted, since the language calls it rather than its name. a constructor and
//! the methods it calls (`__init__`, `__new__`, `__post_init__`,
//! `__init_subclass__`) are left out, since a subclass's is no override of its
//! base's: it takes what its own class needs, and neither the override checks
//! nor `by/documentSuperMembers` hold it to the one it replaces. a module's
//! dunder function is left out, and so is an `extension` block, which names a
//! class it does not declare. a nested function or a class inside a class is not
//! counted

use ruff_db::parsed::parsed_module;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::helpers::is_dunder;
use ruff_text_size::TextRange;
use ty_python_core::ProgramFile;
use ty_python_semantic::types::ide_support::{
    class_member_definitions, is_constructor_like_method, overridden_members,
};

use crate::declaration::ClassName;
use crate::find_references::find_references_to_each;
use crate::goto_implementation::implementations_at_each;
use crate::symbols::symbols_for_file;
use crate::{Db, NavigationTarget, SymbolKind};

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
    /// when not asked for, and for a dunder method
    pub references: Option<usize>,
    /// how many classes subclass a class, directly or not, and how many members
    /// of those override a method. `None` when not asked for, and for a function
    pub implementations: Option<usize>,
    /// whether a method is abstract, an `@abstractmethod` or a protocol member
    /// with no body, so that what overrides it implements it
    pub is_abstract: bool,
    /// what `implementations` counts, in the order `textDocument/implementation`
    /// answers them: each subclass of a class, or each member overriding a
    /// method, with the class it is in. empty when not asked for
    pub implementors: Vec<Implementor>,
}

/// a subclass of a counted class, or a member overriding a counted method
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Implementor {
    /// the subclass, or the class whose member overrides the method. `None` for
    /// one in no class
    pub class: Option<ClassName>,
    /// the subclass or the overriding member, where a click on it goes
    pub target: NavigationTarget,
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
        // the language calls a dunder, not its name
        let (offsets, counted): (Vec<_>, Vec<_>) = declarations
            .iter()
            .enumerate()
            .filter(|(_, declaration)| !is_dunder(&declaration.name))
            .map(|(index, declaration)| (declaration.name_range.start(), index))
            .unzip();
        let found = find_references_to_each(db, file, &offsets, false);
        for (index, references) in counted.into_iter().zip(found) {
            declarations[index].references =
                Some(references.map_or(0, |references| references.len()));
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
            declaration.implementors = implementations
                .unwrap_or_default()
                .into_iter()
                // the declaration is its own first implementation
                .filter(|implementation| {
                    implementation.target.file() != source_file
                        || implementation.target.focus_range() != declaration.name_range
                })
                .map(|implementation| Implementor {
                    class: implementation.class,
                    target: implementation.target,
                })
                .collect();
            declaration.implementations = Some(declaration.implementors.len());
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
        // a dunder is only counted for its overrides, which a function has none of, and a
        // constructor overrides nothing
        let counted = match kind {
            DeclarationKind::Method => !is_constructor_like_method(name),
            DeclarationKind::Class | DeclarationKind::Function => !is_dunder(name),
        };
        if counted {
            declarations.push(DeclarationUsages {
                name: name.to_string(),
                kind,
                full_range,
                name_range,
                references: None,
                implementations: None,
                is_abstract: false,
                implementors: Vec::new(),
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
        /// asked for, `<class file:line>` for each implementor, and
        /// `(abstract)` for an abstract method
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
                    for implementor in &declaration.implementors {
                        let file = implementor.target.file();
                        let target_source = source_text(&self.db, file);
                        let _ = write!(
                            line,
                            " <{} {}:{}>",
                            implementor.class.as_ref().map_or("?", |class| class
                                .qualified_name
                                .as_deref()
                                .unwrap_or(&class.name)),
                            file.path(&self.db).as_str(),
                            LineIndex::from_source_text(&target_source)
                                .line_index(implementor.target.focus_range().start())
                        );
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
        class Animal 4 refs=2 impls=2 <main.Dog /main.py:11> <main.Puppy /main.py:15>
        method speak 6 refs=1 impls=2 <main.Dog /main.py:12> <main.Puppy /main.py:16> (abstract)
        method name 8 refs=1 impls=0
        class Dog 11 refs=2 impls=1 <main.Puppy /main.py:15>
        method speak 12 refs=0 impls=1 <main.Puppy /main.py:16>
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
        class Shape 2 refs=2 impls=1 <main.Square /main.py:4>
        method area 3 refs=1 impls=1 <main.Square /main.py:5>
        function unused 6 refs=0
        ");
    }

    /// A decorated method is counted by its own `def`: what the decorator hands back is defined
    /// elsewhere, and is neither an override of the method nor overridden by it.
    #[test]
    fn a_decorated_method_is_counted_as_its_def() {
        let test = cursor_test(
            "<CURSOR>
def logged(func):
    def wrapper(*args, **kwargs):
        return func(*args, **kwargs)
    return wrapper

class Shape:
    def area(self) -> float:
        return 0.0

class Circle(Shape):
    @logged
    def area(self) -> float:
        return 1.0
",
        );
        assert_snapshot!(test.usage_counts(), @"
        function logged 2 refs=1
        class Shape 7 refs=1 impls=1 <main.Circle /main.py:11>
        method area 8 refs=0 impls=1 <main.Circle /main.py:13>
        class Circle 11 refs=0 impls=0
        method area 13 refs=0 impls=0
        ");
    }

    #[test]
    fn nested_functions_extensions_and_module_dunders_are_not_counted() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "<CURSOR>
class A:
    def f(self) -> None:
        def inner() -> None: ...
        inner()

extension list:
    def second(self) -> int:
        return 0

def __getattr__(name: str) -> int:
    return 0
",
            )
            .build();
        assert_snapshot!(test.usage_counts(), @"
        class A 2 refs=0 impls=0
        method f 3 refs=0 impls=0
        method second 8 refs=0 impls=0
        ");
    }

    /// a dunder method is counted for the methods overriding it, but not for its usages: the
    /// language calls it, not its name. a constructor, and a method it calls, is not counted at
    /// all: a subclass's is no override of its base's
    #[test]
    fn a_dunder_method_is_counted_for_its_overrides_alone_and_a_constructor_not_at_all() {
        let test = cursor_test(
            "<CURSOR>
class Shape:
    def __init__(self, name: str) -> None:
        self.name = name

    def __new__(cls, *args: object) -> 'Shape':
        return super().__new__(cls)

    def __init_subclass__(cls) -> None: ...

    def __eq__(self, other: object) -> bool:
        return False

class Circle(Shape):
    def __init__(self) -> None:
        super().__init__('circle')

    def __new__(cls, *args: object) -> 'Circle':
        return super().__new__(cls)

    def __eq__(self, other: object) -> bool:
        return True

Circle() == Shape('shape')
",
        );
        assert_snapshot!(test.usage_counts(), @"
        class Shape 2 refs=3 impls=1 <main.Circle /main.py:14>
        method __eq__ 11 impls=1 <main.Circle /main.py:21>
        class Circle 14 refs=2 impls=0
        method __eq__ 21 impls=0
        ");
    }
}
