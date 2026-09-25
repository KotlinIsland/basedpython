//! finds the superclass members a class member overrides: the direction
//! [`crate::goto_implementation()`] does not go, which finds the members that
//! override one
//!
//! what counts as an override is the override checks' own walk up the MRO,
//! through [`overridden_members`]. of the declarations that walk finds, a member
//! goes to the nearest along each branch of the MRO: the first of them is what
//! `missing-override-decorator` names, and `invalid-method-override` compares
//! the member with each declaration the walk finds, these and the ones they
//! override in turn
//!
//! asked two ways, from the same code: of one member, at a position on its name
//! ([`super_members`]), which is what *Go to Super* asks; and of every member a
//! document declares at once ([`document_super_members`]), which is what an
//! editor drawing an "overrides" marker beside each one asks, on every pass over
//! the document

use ruff_python_ast::name::Name;
use ruff_text_size::{TextRange, TextSize};
use ty_python_core::ProgramFile;
use ty_python_semantic::types::Type;
use ty_python_semantic::types::ide_support::{
    ClassMemberDefinition, OverriddenMember, class_member_definitions, is_constructor_like_method,
    overridden_members,
};
use ty_python_semantic::{ProgramEnvironment, SemanticModel};

use crate::goto::Definitions;
use crate::{Db, HasNavigationTargets, NavigationTarget};

/// a superclass member that a class member overrides
#[derive(Debug, Clone)]
pub struct SuperMember {
    /// the member's name
    pub name: Name,
    /// the superclass that declares it
    pub superclass: Name,
    /// where the superclass declares it, or, for a member the superclass
    /// synthesizes rather than writes, the superclass itself
    pub target: NavigationTarget,
    /// whether the superclass synthesizes the member, as a dataclass does its
    /// `__init__`, so that [`Self::target`] is the class
    pub synthesized: bool,
    /// whether the member is abstract where the superclass declares it: an
    /// `@abstractmethod`, or a protocol method with no implementation
    pub is_abstract: bool,
}

/// a class member a document declares that overrides something, and what it
/// overrides
#[derive(Debug, Clone)]
pub struct OverridingMember {
    /// the member's name
    pub name: Name,
    /// the class whose body declares it
    pub class: Name,
    /// its name where it is declared: the name of a `def` or a nested class, a
    /// name a class body assigns, annotates or captures, or an import's alias
    pub name_range: TextRange,
    /// whether the member is itself abstract, in the sense of
    /// [`SuperMember::is_abstract`]
    pub is_abstract: bool,
    /// what it overrides directly, in MRO order. never empty
    pub super_members: Vec<SuperMember>,
}

/// the superclass members the class member declared at `offset` overrides
/// directly, in MRO order
///
/// `offset` is on the member's name where it is declared, as
/// [`OverridingMember::name_range`] has it. `None` when there is no such member
/// there; empty when there is one and it overrides nothing
pub fn super_members(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
) -> Option<Vec<SuperMember>> {
    let declared: Vec<_> = class_member_definitions(db, file)
        .into_iter()
        .filter(|member| {
            !member.name_range.is_empty() && member.name_range.contains_inclusive(offset)
        })
        .collect();
    if declared.is_empty() {
        return None;
    }
    let env = SemanticModel::new(db, file).program_environment();
    Some(overriding_member(db, &env, &declared)?.super_members)
}

/// every class member `file` declares that overrides something, with what it
/// overrides, in source order: [`super_members`] of each, from one pass over the
/// document
///
/// a member declared more than once is listed at each declaration that names it
/// in the source, as each is answered: a python property's getter and its setter
/// each override their own accessor. a declaration that stands for several, as a
/// basedpython property with `get` and `set` blocks stands for its getter and its
/// setter under the one name, is listed once, with what any of them overrides
///
/// constructors and the methods they call (`__init__`, `__new__`,
/// `__post_init__`, `__init_subclass__`) are left out: every class's `__init__`
/// overrides `object.__init__`, which says nothing about any of them, and
/// neither `invalid-method-override` nor `missing-override-decorator` holds one
/// to what it overrides. [`super_members`] still answers one when asked
pub fn document_super_members(db: &dyn Db, file: ProgramFile<'_>) -> Vec<OverridingMember> {
    let env = SemanticModel::new(db, file).program_environment();
    class_member_definitions(db, file)
        // the definitions come in source order, so the ones naming the same text
        // are next to each other
        .chunk_by(|left, right| left.name_range == right.name_range)
        .filter(|declared| {
            declared.first().is_some_and(|member| {
                !member.name_range.is_empty() && !is_constructor_like_method(&member.name)
            })
        })
        .filter_map(|declared| overriding_member(db, &env, declared))
        .filter(|member| !member.super_members.is_empty())
        .collect()
}

/// what the definitions `declared`, which all name the same member at the same
/// place in the source, override between them. `None` when `declared` is empty
fn overriding_member(
    db: &dyn Db,
    env: &ProgramEnvironment<'_>,
    declared: &[ClassMemberDefinition<'_>],
) -> Option<OverridingMember> {
    let first = declared.first()?;
    let mut is_abstract = false;
    let mut overridden: Vec<OverriddenMember<'_>> = Vec::new();
    for member in declared {
        let overrides = overridden_members(db, member);
        is_abstract |= overrides.is_abstract;
        for member in overrides.overridden {
            if !overridden
                .iter()
                .any(|found| found.superclass == member.superclass)
            {
                overridden.push(member);
            }
        }
    }
    Some(OverridingMember {
        name: first.name.clone(),
        class: first.class_name.clone(),
        name_range: first.name_range,
        is_abstract,
        super_members: targets(db, env, &first.name, overridden),
    })
}

/// where each of `overridden`, the members named `name` a member overrides, is
/// declared, or the superclass itself for one with no declaration to go to. one
/// with neither is left out
fn targets(
    db: &dyn Db,
    env: &ProgramEnvironment<'_>,
    name: &Name,
    overridden: Vec<OverriddenMember<'_>>,
) -> Vec<SuperMember> {
    overridden
        .into_iter()
        .filter_map(|member| {
            let target = Definitions::new(member.definitions)
                .into_navigation_targets(db)
                .into_iter()
                .next()
                .or_else(|| {
                    Type::ClassLiteral(member.superclass)
                        .navigation_targets(db, env)
                        .into_iter()
                        .next()
                })?;
            Some(SuperMember {
                name: name.clone(),
                superclass: member.superclass_name,
                target,
                synthesized: member.synthesized,
                is_abstract: member.is_abstract,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::tests::{CursorTest, cursor_test};
    use insta::assert_snapshot;
    use ruff_db::source::source_text;
    use ruff_source_file::LineIndex;

    impl CursorTest {
        /// one line per member: `Superclass.name` and the line it is declared
        /// on, `(synthesized)` when the superclass synthesizes it, `(abstract)`
        /// when the member is abstract there
        fn super_members(&self) -> String {
            let Some(members) = super::super_members(
                &self.db,
                self.program_file(self.cursor.file),
                self.cursor.offset,
            ) else {
                return "None".to_string();
            };
            if members.is_empty() {
                return "[]".to_string();
            }
            members
                .iter()
                .map(|member| self.super_member(member))
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn super_member(&self, member: &super::SuperMember) -> String {
            let file = member.target.file();
            let source = source_text(&self.db, file);
            let line = LineIndex::from_source_text(&source)
                .line_index(member.target.focus_range().start());
            format!(
                "{}.{} {}:{}{}{}",
                member.superclass,
                member.name,
                file.path(&self.db).as_str(),
                line,
                if member.synthesized {
                    " (synthesized)"
                } else {
                    ""
                },
                if member.is_abstract {
                    " (abstract)"
                } else {
                    ""
                },
            )
        }

        /// one line per overriding member of the cursor's file: its name, the
        /// line and column its name is on, `(abstract)` when it is abstract
        /// itself, and after `->` what it overrides, as [`Self::super_members`]
        /// writes each
        fn document_super_members(&self) -> String {
            let file = self.cursor.file;
            let source = source_text(&self.db, file);
            let index = LineIndex::from_source_text(&source);
            let members =
                super::document_super_members(&self.db, self.program_file(self.cursor.file));
            if members.is_empty() {
                return "[]".to_string();
            }
            members
                .iter()
                .map(|member| {
                    let start = member.name_range.start();
                    let line = index.line_index(start);
                    let column = start - index.line_start(line, &source);
                    format!(
                        "{}.{} {}:{}{} -> {}",
                        member.class,
                        member.name,
                        line,
                        column.to_u32(),
                        if member.is_abstract {
                            " (abstract)"
                        } else {
                            ""
                        },
                        member
                            .super_members
                            .iter()
                            .map(|member| self.super_member(member))
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    #[test]
    fn an_abstract_method_is_said_to_be_abstract() {
        let test = cursor_test(
            "
from abc import ABC, abstractmethod

class A(ABC):
    @abstractmethod
    def f(self) -> None: ...

class B(A):
    def <CURSOR>f(self) -> None:
        pass
",
        );
        assert_snapshot!(test.super_members(), @"A.f /main.py:6 (abstract)");
    }

    #[test]
    fn a_document_lists_each_member_that_overrides_something() {
        let test = cursor_test(
            "<CURSOR>
from typing import Protocol

class Greeter(Protocol):
    def greet(self) -> str: ...

class A:
    size: int = 1

    def f(self) -> None: ...

    @property
    def x(self) -> int:
        return 1

    @x.setter
    def x(self, value: int) -> None: ...

class B(A):
    size = 2

    def __init__(self) -> None: ...

    def f(self) -> None: ...

    def g(self) -> None: ...

    @property
    def x(self) -> int:
        return 2

    @x.setter
    def x(self, value: int) -> None: ...

class C(B, Greeter):
    def f(self) -> None:
        f = 1
        class Inner(A):
            def f(self) -> None: ...

    def greet(self) -> str:
        return 'hi'

    names = [f for f in 'ab']
",
        );
        assert_snapshot!(test.document_super_members(), @"
        B.size 20:4 -> A.size /main.py:8
        B.f 24:8 -> A.f /main.py:10
        B.x 29:8 -> A.x /main.py:13
        B.x 33:8 -> A.x /main.py:17
        C.f 36:8 -> B.f /main.py:24
        Inner.f 39:16 -> A.f /main.py:10
        C.greet 41:8 -> Greeter.greet /main.py:5 (abstract)
        ");
    }

    #[test]
    fn a_document_member_that_overrides_through_two_bases_lists_both() {
        let test = cursor_test(
            "<CURSOR>
class Root:
    def f(self) -> None: ...

class Left(Root):
    def f(self) -> None: ...

class Right(Root):
    def f(self) -> None: ...

class Both(Left, Right):
    def f(self) -> None: ...
",
        );
        assert_snapshot!(test.document_super_members(), @"
        Left.f 6:8 -> Root.f /main.py:3
        Right.f 9:8 -> Root.f /main.py:3
        Both.f 12:8 -> Left.f /main.py:6, Right.f /main.py:9
        ");
    }

    #[test]
    fn a_protocol_member_redeclared_in_a_protocol_is_abstract_itself() {
        let test = cursor_test(
            "<CURSOR>
from typing import Protocol

class Shape(Protocol):
    def area(self) -> float: ...

class Polygon(Shape, Protocol):
    def area(self) -> float: ...
",
        );
        assert_snapshot!(
            test.document_super_members(),
            @"Polygon.area 8:8 (abstract) -> Shape.area /main.py:5 (abstract)"
        );
    }

    #[test]
    fn a_document_with_no_overrides_is_empty() {
        let test = cursor_test(
            "<CURSOR>
class A:
    def __init__(self) -> None: ...

    def f(self) -> None: ...

def g() -> None: ...
",
        );
        assert_snapshot!(test.document_super_members(), @"[]");
    }

    #[test]
    fn a_method_goes_to_the_method_it_overrides() {
        let test = cursor_test(
            "
class A:
    def f(self) -> None: ...

class B(A):
    def <CURSOR>f(self) -> None: ...
",
        );
        assert_snapshot!(test.super_members(), @"A.f /main.py:3");
    }

    #[test]
    fn two_levels_up_goes_to_the_nearer() {
        let test = cursor_test(
            "
class A:
    def f(self) -> None: ...

class B(A):
    def f(self) -> None: ...

class C(B):
    def <CURSOR>f(self) -> None: ...
",
        );
        assert_snapshot!(test.super_members(), @"B.f /main.py:6");
    }

    #[test]
    fn two_levels_up_past_a_class_that_does_not_declare_it() {
        let test = cursor_test(
            "
class A:
    def f(self) -> None: ...

class B(A):
    pass

class C(B):
    def <CURSOR>f(self) -> None: ...
",
        );
        assert_snapshot!(test.super_members(), @"A.f /main.py:3");
    }

    #[test]
    fn several_bases_that_each_declare_it_are_each_overridden_in_mro_order() {
        let test = cursor_test(
            "
class Root:
    def f(self) -> None: ...

class Left(Root):
    def f(self) -> None: ...

class Right(Root):
    def f(self) -> None: ...

class Both(Left, Right):
    def <CURSOR>f(self) -> None: ...
",
        );
        assert_snapshot!(test.super_members(), @"
        Left.f /main.py:6
        Right.f /main.py:9
        ");
    }

    #[test]
    fn a_diamond_whose_sides_do_not_declare_it_goes_to_the_root_once() {
        let test = cursor_test(
            "
class Root:
    def f(self) -> None: ...

class Left(Root): ...

class Right(Root): ...

class Both(Left, Right):
    def <CURSOR>f(self) -> None: ...
",
        );
        assert_snapshot!(test.super_members(), @"Root.f /main.py:3");
    }

    #[test]
    fn a_protocol_member() {
        let test = cursor_test(
            "
from typing import Protocol

class Greeter(Protocol):
    def greet(self) -> str: ...

class English(Greeter):
    def <CURSOR>greet(self) -> str:
        return 'hello'
",
        );
        assert_snapshot!(test.super_members(), @"Greeter.greet /main.py:5 (abstract)");
    }

    #[test]
    fn a_property_getter_goes_to_the_getter() {
        let test = cursor_test(
            "
class A:
    @property
    def x(self) -> int:
        return 1

    @x.setter
    def x(self, value: int) -> None: ...

class B(A):
    @property
    def <CURSOR>x(self) -> int:
        return 2
",
        );
        assert_snapshot!(test.super_members(), @"A.x /main.py:4");
    }

    #[test]
    fn a_class_attribute() {
        let test = cursor_test(
            "
class A:
    size: int = 1

class B(A):
    <CURSOR>size = 2
",
        );
        assert_snapshot!(test.super_members(), @"A.size /main.py:3");
    }

    #[test]
    fn a_member_the_superclass_synthesizes_goes_to_the_superclass() {
        let test = cursor_test(
            "
from dataclasses import dataclass

@dataclass
class A:
    x: int

class B(A):
    def <CURSOR>__init__(self) -> None:
        super().__init__(1)
",
        );
        assert_snapshot!(test.super_members(), @"A.__init__ /main.py:5 (synthesized)");
    }

    #[test]
    fn a_method_that_overrides_nothing() {
        let test = cursor_test(
            "
class A:
    def f(self) -> None: ...

class B(A):
    def <CURSOR>g(self) -> None: ...
",
        );
        assert_snapshot!(test.super_members(), @"[]");
    }

    #[test]
    fn a_function_that_is_not_a_member() {
        let test = cursor_test(
            "
def <CURSOR>f() -> None: ...
",
        );
        assert_snapshot!(test.super_members(), @"None");
    }

    #[test]
    fn basedpython_override_and_protocol() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
protocol Shape:
    def area(self) -> float: ...

class Square(Shape):
    def __init__(self, side: float) -> None:
        self.side = side

    override def <CURSOR>area(self) -> float:
        return self.side * self.side
",
            )
            .build();
        assert_snapshot!(test.super_members(), @"Shape.area /main.by:3 (abstract)");
    }

    #[test]
    fn a_private_member_overrides_nothing() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
class A:
    private def f(self) -> None: ...

class B(A):
    private def <CURSOR>f(self) -> None: ...
",
            )
            .build();
        assert_snapshot!(test.super_members(), @"[]");
    }

    /// a decorator can replace a class with anything, and the members are the
    /// class's own all the same: those of the class as written, not of what the
    /// decorator returns
    #[test]
    fn a_member_of_a_decorated_class() {
        let test = cursor_test(
            "<CURSOR>
class Base:
    def f(self) -> None: ...

class Other(Base): ...

def to_int(cls: type) -> int:
    return 1

def to_other(cls: type) -> Other:
    return Other()

class A:
    def f(self) -> None: ...

@to_int
class C(A):
    def f(self) -> None: ...

@to_other
class D(A):
    def f(self) -> None: ...
",
        );
        assert_snapshot!(test.document_super_members(), @"
        C.f 18:8 -> A.f /main.py:14
        D.f 22:8 -> A.f /main.py:14
        ");
    }

    #[test]
    fn an_accessor_block_is_listed_once() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "<CURSOR>
class A:
    var age: int = 0
        get() = field
        set(value):
            field = value

class B(A):
    override var age: int = 0
        get() = field
        set(value):
            field = value
",
            )
            .build();
        assert_snapshot!(test.document_super_members(), @"B.age 9:17 -> A.age /main.by:3");
    }

    #[test]
    fn an_accessor_block_is_asked_about_once() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
class A:
    var age: int = 0
        get() = field
        set(value):
            field = value

class B(A):
    override var <CURSOR>age: int = 0
        get() = field
        set(value):
            field = value
",
            )
            .build();
        assert_snapshot!(test.super_members(), @"A.age /main.by:3");
    }

    #[test]
    fn a_setter_overriding_a_property_with_only_a_getter_goes_to_the_getter() {
        let test = cursor_test(
            "<CURSOR>
class A:
    @property
    def x(self) -> int:
        return 1

class B(A):
    @property
    def x(self) -> int:
        return 2

    @x.setter
    def x(self, value: int) -> None: ...
",
        );
        assert_snapshot!(test.document_super_members(), @"
        B.x 9:8 -> A.x /main.py:4
        B.x 13:8 -> A.x /main.py:4
        ");
    }

    #[test]
    fn a_property_overriding_an_attribute() {
        let test = cursor_test(
            "<CURSOR>
class A:
    x: int = 1

class B(A):
    @property
    def x(self) -> int:
        return 2
",
        );
        assert_snapshot!(test.document_super_members(), @"B.x 7:8 -> A.x /main.py:3");
    }

    /// a base whose members cannot be known is passed over, and the walk goes
    /// on to the bases after it
    #[test]
    fn a_dynamic_base_is_passed_over() {
        let test = cursor_test(
            "<CURSOR>
from typing import Any

class A:
    def f(self) -> None: ...

class B(Any, A):
    def f(self) -> None: ...
",
        );
        assert_snapshot!(test.document_super_members(), @"B.f 8:8 -> A.f /main.py:5");
    }

    /// the keys of a typed dict are not members a class body overrides
    #[test]
    fn a_typed_dict_key_overrides_nothing() {
        let test = cursor_test(
            "<CURSOR>
from typing import TypedDict

class A(TypedDict):
    x: int

class B(A):
    x: int
",
        );
        assert_snapshot!(test.document_super_members(), @"[]");
    }

    #[test]
    fn a_member_of_a_generic_class() {
        let test = cursor_test(
            "<CURSOR>
class A[T]:
    def f(self, x: T) -> T:
        return x

class B(A[int]):
    def f(self, x: int) -> int:
        return x

class C[T](A[T]):
    def f(self, x: T) -> T:
        return x
",
        );
        assert_snapshot!(test.document_super_members(), @"
        B.f 7:8 -> A.f /main.py:3
        C.f 11:8 -> A.f /main.py:3
        ");
    }

    #[test]
    fn a_nested_class_an_import_and_a_walrus_in_a_decorator_are_members() {
        let test = cursor_test(
            "<CURSOR>
import os

def deco(f: object) -> object:
    return f

class A:
    import os
    class Meta: ...
    d = deco
    match 1:
        case captured: ...

    @deco
    def f(self) -> None: ...

class B(A):
    import os
    class Meta: ...

    @(d := deco)
    def f(self) -> None: ...

    match 1:
        case captured: ...
",
        );
        assert_snapshot!(test.document_super_members(), @"
        B.os 18:11 -> A.os /main.py:8
        B.Meta 19:10 -> A.Meta /main.py:9
        B.d 21:6 -> A.d /main.py:10
        B.f 22:8 -> A.f /main.py:15
        B.captured 25:13 -> A.captured /main.py:12
        ");
    }

    #[test]
    fn a_member_of_object() {
        let test = cursor_test(
            "
class A:
    def <CURSOR>__repr__(self) -> str:
        return 'A'
",
        );
        assert_snapshot!(test.super_members(), @"object.__repr__ stdlib/builtins.byi:93");
    }
}
