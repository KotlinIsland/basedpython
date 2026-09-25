//! Finds the superclass members a class member overrides — the direction
//! [`crate::goto_implementation()`] does not go, which finds the members that
//! override one.
//!
//! What counts as an override is the override checks' own answer, through
//! [`overridden_members`], so a member goes to exactly what
//! `invalid-method-override` compares it with and `missing-override-decorator`
//! asks it to be marked as overriding.

use ruff_db::parsed::parsed_module;
use ruff_python_ast::ExprRef;
use ruff_python_ast::name::Name;
use ruff_text_size::TextSize;
use ty_python_core::ProgramFile;
use ty_python_semantic::SemanticModel;
use ty_python_semantic::types::Type;
use ty_python_semantic::types::ide_support::{ClassMemberNode, overridden_members};

use crate::goto::{Definitions, GotoTarget, find_goto_target};
use crate::{Db, HasNavigationTargets, NavigationTarget};

/// A superclass member that the member at a position overrides.
#[derive(Debug, Clone)]
pub struct SuperMember {
    /// The member's name.
    pub name: Name,
    /// The superclass that declares it.
    pub superclass: Name,
    /// Where the superclass declares it — or, for a member the superclass
    /// synthesizes rather than writes, the superclass itself.
    pub target: NavigationTarget,
    /// Whether the superclass synthesizes the member, as a dataclass does its
    /// `__init__`, so that [`Self::target`] is the class.
    pub synthesized: bool,
}

/// The superclass members the class member declared at `offset` overrides
/// directly, in MRO order.
///
/// `offset` is on the member's name where it is declared: the name of a `def`,
/// or a name a class body assigns or annotates. `None` when there is no such
/// member there; empty when there is one and it overrides nothing.
pub fn super_members(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
) -> Option<Vec<SuperMember>> {
    let module = parsed_module(db, file.python_file(db)).load(db);
    let model = SemanticModel::new(db, file);
    let (member, name) = match find_goto_target(&model, &module, offset)? {
        GotoTarget::FunctionDef(function) => (
            ClassMemberNode::Function(function),
            function.name.id.clone(),
        ),
        GotoTarget::Expression(ExprRef::Name(name)) if name.ctx.is_store() => {
            (ClassMemberNode::Name(name), name.id.clone())
        }
        _ => return None,
    };

    let env = model.program_environment();
    let members = overridden_members(&model, member)?;
    Some(
        members
            .into_iter()
            .filter_map(|member| {
                let declared = Definitions::new(member.definitions)
                    .into_navigation_targets(db)
                    .into_iter()
                    .next();
                let synthesized = declared.is_none();
                let target = declared.or_else(|| {
                    Type::ClassLiteral(member.superclass)
                        .navigation_targets(db, &env)
                        .into_iter()
                        .next()
                })?;
                Some(SuperMember {
                    name: name.clone(),
                    superclass: member.superclass_name,
                    target,
                    synthesized,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use crate::tests::{CursorTest, cursor_test};
    use insta::assert_snapshot;
    use ruff_db::source::source_text;
    use ruff_source_file::LineIndex;

    impl CursorTest {
        /// One line per member: `Superclass.name` and the line it is declared
        /// on, `(synthesized)` when the target is the class.
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
                .map(|member| {
                    let file = member.target.file();
                    let source = source_text(&self.db, file);
                    let line = LineIndex::from_source_text(&source)
                        .line_index(member.target.focus_range().start());
                    format!(
                        "{}.{} {}:{}{}",
                        member.superclass,
                        member.name,
                        file.path(&self.db).as_str(),
                        line,
                        if member.synthesized {
                            " (synthesized)"
                        } else {
                            ""
                        },
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
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
        assert_snapshot!(test.super_members(), @"Greeter.greet /main.py:5");
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
    fn basedpython_override_and_protocol_class() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
protocol class Shape:
    def area(self) -> float: ...

class Square(Shape):
    def __init__(self, side: float) -> None:
        self.side = side

    override def <CURSOR>area(self) -> float:
        return self.side * self.side
",
            )
            .build();
        assert_snapshot!(test.super_members(), @"Shape.area /main.by:3");
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
