//! basedpython's `data class` keyword, added to a class or taken off one.
//!
//! `data class Foo` lowers to `@dataclass(slots=True)`, so writing the keyword
//! is a change of meaning the user asks for, not a rewrite that preserves it:
//! the class gains an `__init__`, equality and a `__repr__`. What is refused is
//! a class the keyword cannot apply to, and a class whose instances would stop
//! working once they have slots — one that assigns an attribute the class never
//! declares.

use ruff_diagnostics::Edit;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, TraversalSignal, walk_node};
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, ExprContext, Stmt};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::FxHashSet;
use ty_python_semantic::HasType;
use ty_python_semantic::types::ide_support::{
    data_class_conversion_obstacle, is_dataclass_field_function,
};

use super::{Plan, RefactorContext, Refusal};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    ToDataClass,
    FromDataClass,
}

/// The markers the parser records a `data class` or `frozen data class` with.
const DATA_CLASS_MARKERS: [&str; 2] = ["data_class", "frozen_data_class"];

pub(super) fn plan(
    context: &RefactorContext<'_>,
    range: TextRange,
    direction: Direction,
) -> Result<Plan, Refusal> {
    if !context.is_basedpython() {
        return Err(Refusal::NotApplicable);
    }
    let module = context.parsed.syntax();
    if !module.range().contains_range(range) {
        return Err(Refusal::NotApplicable);
    }
    let covering = covering_node(module.into(), range);
    let Some(class) = covering.ancestors().find_map(|node| match node {
        AnyNodeRef::StmtClassDef(class) if header(class).contains_range(range) => Some(class),
        _ => None,
    }) else {
        return Err(Refusal::NotApplicable);
    };

    let marker = class.decorator_list.iter().find(|decorator| {
        matches!(
            &decorator.expression,
            Expr::Name(name)
                if name.ctx == ExprContext::Invalid && DATA_CLASS_MARKERS.contains(&name.id.as_str())
        )
    });

    match (direction, marker) {
        (Direction::FromDataClass, Some(marker)) => {
            let title = "Convert to a plain class";
            // taking the keyword off takes the generated `__init__` with it, and that is the
            // change of meaning being asked for: a field then has no way to be filled, which
            // the checker reports at every call that relied on it. a `field(...)` default is
            // different — nothing reports it. the dataclass machinery reads that descriptor and
            // replaces it; without it the `Field` object simply *is* the attribute's value, on
            // every instance at once
            if let Some(field) = dataclass_only_default(context, class) {
                return Err(Refusal::refused(
                    title,
                    format!(
                        "`{field}` has a `field(...)` default, which only the dataclass \
                         machinery reads — a plain class would hold the descriptor itself"
                    ),
                ));
            }
            Ok(Plan {
                title: title.to_string(),
                edits: vec![Edit::range_deletion(marker.range())],
            })
        }
        (Direction::ToDataClass, None) => {
            let title = "Convert to a data class";
            if let Some(reason) = data_class_conversion_obstacle(&context.model, class) {
                return Err(Refusal::refused(title, reason));
            }
            if let Some(reason) = slots_obstacle(class) {
                return Err(Refusal::refused(title, reason));
            }
            let Some(keyword) = context
                .parsed
                .tokens()
                .iter()
                .filter(|token| {
                    token.kind() == TokenKind::Class
                        && TextRange::new(class.start(), class.name.start())
                            .contains_range(token.range())
                })
                .last()
            else {
                return Err(Refusal::NotApplicable);
            };
            Ok(Plan {
                title: title.to_string(),
                edits: vec![Edit::insertion("data ".to_string(), keyword.start())],
            })
        }
        _ => Err(Refusal::NotApplicable),
    }
}

/// The part of a class before its body.
fn header(class: &ast::StmtClassDef) -> TextRange {
    let end = class
        .arguments
        .as_deref()
        .map(Ranged::end)
        .or_else(|| class.type_params.as_deref().map(Ranged::end))
        .unwrap_or(class.name.end());
    TextRange::new(class.start(), end)
}

/// Why `class` would break once the dataclass machinery ran over it: the class writes its own
/// `__slots__`, a method assigns an attribute of the instance the class body does not declare,
/// or a method calls `super()` with no arguments.
///
/// The last of those is about slots rather than about `super`: building a slotted class means
/// building a *new* class object, and the `__class__` cell the methods closed over still points
/// at the original. `super()` reads that cell, so it would raise `TypeError` at runtime — which
/// is why `dataclasses` documents zero-argument `super()` as not working under `slots=True`.
fn slots_obstacle(class: &ast::StmtClassDef) -> Option<String> {
    let mut declared = FxHashSet::default();
    for statement in &class.body {
        let target = match statement {
            Stmt::AnnAssign(assign) => &*assign.target,
            Stmt::Assign(assign) => match assign.targets.as_slice() {
                [target] => target,
                _ => continue,
            },
            _ => continue,
        };
        if let Expr::Name(name) = target {
            if name.id == "__slots__" {
                return Some(
                    "the class writes its own `__slots__`, and a data class makes its own"
                        .to_string(),
                );
            }
            if statement.is_ann_assign_stmt() {
                declared.insert(name.id.to_string());
            }
        }
    }

    for statement in &class.body {
        let Stmt::FunctionDef(method) = statement else {
            continue;
        };
        if bare_super_call(&method.body) {
            return Some(format!(
                "`{}` calls `super()` with no arguments, and a slotted data class is a new class \
                 its methods do not close over — the call would raise at runtime",
                method.name
            ));
        }
        // a method with no receiver assigns no attribute of one. a static method's first
        // parameter is not a receiver however it is named, so it is not read as one
        if is_static_method(method) {
            continue;
        }
        let Some(receiver) = method
            .parameters
            .posonlyargs
            .iter()
            .chain(&method.parameters.args)
            .next()
        else {
            continue;
        };
        let mut finder = AttributeWrites {
            receiver: receiver.parameter.name.id.as_str(),
            declared: &declared,
            undeclared: None,
        };
        for body_statement in &method.body {
            walk_node(&mut finder, AnyNodeRef::from(body_statement));
        }
        if let Some(attribute) = finder.undeclared {
            return Some(format!(
                "`{}` assigns `{}.{attribute}`, which the class does not declare, and a slotted data class has no room for it",
                method.name, receiver.parameter.name
            ));
        }
    }
    None
}

/// The first field of `class` whose default is a `dataclasses.field(...)` call, which means
/// something only while the dataclass machinery is there to read it.
///
/// Which function is called is asked of the checker rather than read off the name, so a
/// `field` the project defines itself is not mistaken for this one, and
/// `dataclasses.field` is still recognised however it was imported or aliased.
fn dataclass_only_default<'a>(
    context: &RefactorContext<'_>,
    class: &'a ast::StmtClassDef,
) -> Option<&'a str> {
    class.body.iter().find_map(|statement| {
        let Stmt::AnnAssign(assign) = statement else {
            return None;
        };
        let Expr::Name(name) = &*assign.target else {
            return None;
        };
        let Expr::Call(call) = assign.value.as_deref()? else {
            return None;
        };
        let callee = call.func.inferred_type(&context.model)?;
        is_dataclass_field_function(context.db, callee).then(|| name.id.as_str())
    })
}

/// Whether `method` is declared without a receiver, by decorator or by basedpython modifier.
fn is_static_method(method: &ast::StmtFunctionDef) -> bool {
    method.decorator_list.iter().any(|decorator| {
        matches!(
            &decorator.expression,
            Expr::Name(name) if matches!(name.id.as_str(), "staticmethod" | "static")
        )
    })
}

/// Whether anything in `body` calls `super` with no arguments, outside a nested definition —
/// which has a `__class__` cell of its own only when it is written in another class.
fn bare_super_call(body: &[Stmt]) -> bool {
    struct Finder {
        found: bool,
    }
    impl<'a> SourceOrderVisitor<'a> for Finder {
        fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
            if self.found {
                return TraversalSignal::Skip;
            }
            // a class written inside the method binds its own `__class__`, so a `super()` in it
            // is not this class's
            if node.is_stmt_class_def() {
                return TraversalSignal::Skip;
            }
            if let AnyNodeRef::ExprCall(call) = node
                && call.arguments.is_empty()
                && matches!(&*call.func, Expr::Name(name) if name.id == "super")
            {
                self.found = true;
                return TraversalSignal::Skip;
            }
            TraversalSignal::Traverse
        }
    }
    let mut finder = Finder { found: false };
    for statement in body {
        walk_node(&mut finder, AnyNodeRef::from(statement));
    }
    finder.found
}

struct AttributeWrites<'a> {
    receiver: &'a str,
    declared: &'a FxHashSet<String>,
    undeclared: Option<String>,
}

impl<'a> SourceOrderVisitor<'a> for AttributeWrites<'_> {
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        if self.undeclared.is_some() {
            return TraversalSignal::Skip;
        }
        // a nested function or class has a receiver of its own
        if matches!(
            node,
            AnyNodeRef::StmtFunctionDef(_) | AnyNodeRef::StmtClassDef(_)
        ) {
            return TraversalSignal::Skip;
        }
        if let AnyNodeRef::ExprAttribute(attribute) = node
            && attribute.ctx.is_store()
            && matches!(&*attribute.value, Expr::Name(name) if name.id == self.receiver)
            && !self.declared.contains(attribute.attr.as_str())
        {
            self.undeclared = Some(attribute.attr.to_string());
        }
        TraversalSignal::Traverse
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use crate::refactor::RefactorKind;
    use crate::refactor::test_support::RefactorTest;

    fn to_data(source: &str) -> String {
        RefactorTest::basedpython(source).apply(RefactorKind::ConvertToDataClass)
    }

    fn from_data(source: &str) -> String {
        RefactorTest::basedpython(source).apply(RefactorKind::ConvertFromDataClass)
    }

    #[test]
    fn adds_the_keyword() {
        assert_snapshot!(to_data(
            "
            class <CURSOR>Point:
                x: int
                y: int
            ",
        ), @"
        Convert to a data class
        ---

        data class Point:
            x: int
            y: int
        ");
    }

    #[test]
    fn keyword_goes_after_other_modifiers() {
        assert_snapshot!(to_data(
            "
            final class <CURSOR>Point:
                x: int
            ",
        ), @"
        Convert to a data class
        ---

        final data class Point:
            x: int
        ");
    }

    #[test]
    fn data_class_is_not_offered_to_become_one() {
        assert_snapshot!(to_data(
            "
            data class <CURSOR>Point:
                x: int
            ",
        ), @"not offered");
    }

    #[test]
    fn removes_the_keyword() {
        assert_snapshot!(from_data(
            "
            data class <CURSOR>Point:
                x: int
            ",
        ), @"
        Convert to a plain class
        ---

        class Point:
            x: int
        ");
    }

    /// `@dataclass(slots=True)` builds a new class object, and the methods closed over the old
    /// one, so the `__class__` cell `super()` reads no longer matches the instance.
    #[test]
    fn a_class_calling_bare_super_is_refused() {
        assert_snapshot!(to_data(
            "
            class Base:
                def setup(self) -> None: ...

            class <CURSOR>Point(Base):
                x: int

                def setup(self) -> None:
                    super().setup()
            ",
        ), @"refused: Convert to a data class (`setup` calls `super()` with no arguments, and a slotted data class is a new class its methods do not close over — the call would raise at runtime)");
    }

    /// `super(Point, self)` reads no cell, so it survives the new class object.
    #[test]
    fn a_class_calling_super_with_arguments_is_converted() {
        assert_snapshot!(to_data(
            "
            class Base:
                def setup(self) -> None: ...

            class <CURSOR>Point(Base):
                x: int

                def setup(self) -> None:
                    super(Point, self).setup()
            ",
        ), @"
        Convert to a data class
        ---

        class Base:
            def setup(self) -> None: ...

        data class Point(Base):
            x: int

            def setup(self) -> None:
                super(Point, self).setup()
        ");
    }

    /// A static method's first parameter is not a receiver, so what it assigns is not an
    /// attribute of an instance of this class.
    #[test]
    fn a_static_method_is_not_read_as_having_a_receiver() {
        assert_snapshot!(to_data(
            "
            class <CURSOR>Point:
                x: int

                @staticmethod
                def reset(other) -> None:
                    other.anything = 1
            ",
        ), @"
        Convert to a data class
        ---

        data class Point:
            x: int

            @staticmethod
            def reset(other) -> None:
                other.anything = 1
        ");
    }

    /// Nothing reports the loss of a `field(...)` default: the plain class would simply hold the
    /// descriptor, shared by every instance.
    #[test]
    fn a_field_default_is_refused_going_back_to_a_plain_class() {
        assert_snapshot!(from_data(
            "
            from dataclasses import field

            data class <CURSOR>Bag:
                items: list[int] = field(default_factory=list)
            ",
        ), @"refused: Convert to a plain class (`items` has a `field(...)` default, which only the dataclass machinery reads — a plain class would hold the descriptor itself)");
    }

    /// A `field` the project defines itself is an ordinary call, and its value survives losing
    /// the keyword. Which function is called is asked of the checker, so the name alone does not
    /// decide it.
    #[test]
    fn a_projects_own_field_function_is_not_the_dataclass_one() {
        assert_snapshot!(from_data(
            "
            def field(default: int) -> int:
                return default

            data class <CURSOR>Bag:
                size: int = field(3)
            ",
        ), @"
        Convert to a plain class
        ---

        def field(default: int) -> int:
            return default

        class Bag:
            size: int = field(3)
        ");
    }

    #[test]
    fn removes_frozen_with_it() {
        assert_snapshot!(from_data(
            "
            frozen data class <CURSOR>Point:
                x: int
            ",
        ), @"
        Convert to a plain class
        ---

        class Point:
            x: int
        ");
    }

    #[test]
    fn plain_class_is_not_offered_to_become_plain() {
        assert_snapshot!(from_data(
            "
            class <CURSOR>Point:
                x: int
            ",
        ), @"not offered");
    }

    #[test]
    fn decorated_dataclass_is_refused() {
        assert_snapshot!(to_data(
            "
            from dataclasses import dataclass

            @dataclass
            class <CURSOR>Point:
                x: int
            ",
        ), @"refused: Convert to a data class (the class is already a data class)");
    }

    #[test]
    fn enum_is_refused() {
        assert_snapshot!(to_data(
            "
            from enum import Enum

            class <CURSOR>Color(Enum):
                RED = 1
            ",
        ), @"refused: Convert to a data class (an enum is not a data class)");
    }

    #[test]
    fn undeclared_attribute_write_is_refused() {
        assert_snapshot!(to_data(
            "
            class <CURSOR>Point:
                x: int

                def move(self):
                    self.moved = True
            ",
        ), @"refused: Convert to a data class (`move` assigns `self.moved`, which the class does not declare, and a slotted data class has no room for it)");
    }

    #[test]
    fn python_file_is_not_offered() {
        assert_snapshot!(
            RefactorTest::python(
                "
                class <CURSOR>Point:
                    x: int
                ",
            )
            .apply(RefactorKind::ConvertToDataClass),
            @"not offered"
        );
    }
}
