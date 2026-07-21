//! type-aware pass: synthesize a declared type for a bare class-body
//! assignment from ty's inferred type.
//!
//! ```by
//! class A:
//!     a = 1
//! ```
//!
//! lowers to `a: int = 1` — the attribute's inferred type is promoted
//! (`Literal[1]` → `int`) and spliced in as an annotation, so the class's
//! implicit attribute types become declared.
//!
//! only a bare `<name> = <value>` directly in a class body is rewritten.
//! multi-target (`a = b = 1`) and unpacking (`a, b = ...`) assignments are
//! left alone, as are assignments whose value has no faithful annotation
//! (an `Unknown` / `Any` part). classes where an annotation would change
//! runtime semantics — dataclasses, framework models, `NamedTuple`s,
//! `TypedDict`s, enums — are skipped entirely: in those a bare assignment is
//! a class variable or member, and annotating it would turn it into a field.

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtAssign, StmtClassDef};
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

pub(crate) struct InferredAnnotationPass;

impl InferredAnnotationPass {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl TypeAwarePass for InferredAnnotationPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut state = State {
            types,
            edits: Vec::new(),
        };
        for stmt in stmts {
            state.visit_stmt(stmt);
        }
        ctx.text_edits.extend(state.edits);
    }
}

struct State<'a> {
    types: &'a dyn TypeInfo,
    edits: Vec<(TextRange, String)>,
}

impl State<'_> {
    fn process_class(&mut self, class: &StmtClassDef) {
        if self.types.class_body_annotation_is_semantic(class) {
            return;
        }
        for stmt in &class.body {
            if let Stmt::Assign(assign) = stmt {
                self.process_assign(assign);
            }
        }
    }

    fn process_assign(&mut self, assign: &StmtAssign) {
        // only a single `<name> = <value>` — chained (`a = b = 1`) and
        // unpacking (`a, b = ...`) targets have no single declared type
        let [Expr::Name(name)] = assign.targets.as_slice() else {
            return;
        };
        // dunders (`__slots__`, `__match_args__`, ...) are class machinery, not
        // typed attributes — annotating them is noise (and `__slots__ = ()` is
        // exactly what the enum lowering re-feeds through this pipeline)
        if is_dunder(name.id.as_str()) {
            return;
        }
        let Some(annotation) = self.types.inferred_annotation(&assign.value) else {
            return;
        };
        let pos = name.range().end();
        self.edits
            .push((TextRange::new(pos, pos), format!(": {annotation}")));
    }
}

fn is_dunder(name: &str) -> bool {
    name.len() > 4 && name.starts_with("__") && name.ends_with("__")
}

impl<'ast> Visitor<'ast> for State<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::ClassDef(class) = stmt {
            self.process_class(class);
        }
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn int_attribute() {
        check(
            indoc! {"
                class A:
                    a = 1
            "},
            indoc! {"
                class A:
                    a: int = 1
            "},
        );
    }

    #[test]
    fn str_and_bool_attributes() {
        check(
            indoc! {"
                class A:
                    name = \"x\"
                    flag = True
            "},
            indoc! {"
                class A:
                    name: str = \"x\"
                    flag: bool = True
            "},
        );
    }

    #[test]
    fn list_literal_attribute() {
        check(
            indoc! {"
                class A:
                    xs = [1, 2]
            "},
            indoc! {"
                class A:
                    xs: list[int] = [1, 2]
            "},
        );
    }

    #[test]
    fn instance_attribute() {
        check(
            indoc! {"
                class B: ...
                class A:
                    b = B()
            "},
            indoc! {"
                class B: ...
                class A:
                    b: B = B()
            "},
        );
    }

    #[test]
    fn empty_list_left_bare() {
        // `[]` infers `list[Unknown]` — a dynamic part with no faithful
        // annotation, so the assignment stays as-is
        unchanged(indoc! {"
            class A:
                xs = []
        "});
    }

    #[test]
    fn already_annotated_unchanged() {
        unchanged(indoc! {"
            class A:
                a: int = 1
        "});
    }

    #[test]
    fn chained_target_unchanged() {
        unchanged(indoc! {"
            class A:
                a = b = 1
        "});
    }

    #[test]
    fn tuple_target_unchanged() {
        unchanged(indoc! {"
            class A:
                a, b = 1, 2
        "});
    }

    #[test]
    fn module_level_assignment_unchanged() {
        // the feature is scoped to class attributes; a module-level binding
        // may be reassigned with a different type, so it stays inferred
        unchanged("a = 1\n");
    }

    #[test]
    fn method_local_unchanged() {
        unchanged(indoc! {"
            class A:
                def f(self):
                    x = 1
                    return x
        "});
    }

    #[test]
    fn nested_class_attribute() {
        check(
            indoc! {"
                class Outer:
                    class Inner:
                        a = 1
            "},
            indoc! {"
                class Outer:
                    class Inner:
                        a: int = 1
            "},
        );
    }

    #[test]
    fn dunder_slots_left_bare() {
        unchanged(indoc! {"
            class A:
                __slots__ = (\"x\",)
        "});
    }

    #[test]
    fn dataclass_field_left_bare() {
        // in a dataclass a bare assignment is a plain class variable, not a
        // field; annotating it would turn it into a constructor parameter
        unchanged(indoc! {"
            from dataclasses import dataclass
            @dataclass
            class A:
                a = 1
        "});
    }

    #[test]
    fn enum_member_left_bare() {
        // a bare assignment in an enum is a member, not a typed attribute
        unchanged(indoc! {"
            from enum import Enum
            class Color(Enum):
                RED = 1
                GREEN = 2
        "});
    }
}
