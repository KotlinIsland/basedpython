//! reverse of the based-enum lowering for the simple C-like case:
//!   `class A(Enum):\n    B = auto()` → `enum class A:\n    case B`
//!
//! conservative: only fires when the class subclasses exactly the unqualified
//! `Enum` and every member is `NAME = auto()` — the shape an all-unit based
//! `enum` lowers to. payload enums (sealed dataclass hierarchies) are not
//! reversed.

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtClassDef};
use ruff_text_size::{Ranged, TextRange};

#[derive(Default)]
pub(crate) struct EnumsReverse {
    pub(crate) edits: Vec<Fix>,
}

impl EnumsReverse {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn try_reverse(&mut self, class: &StmtClassDef) {
        if !class.decorator_list.is_empty() || class.type_params.is_some() {
            return;
        }
        // exactly one positional base, the bare name `Enum`, no keywords
        let Some(args) = &class.arguments else {
            return;
        };
        if !args.keywords.is_empty() || args.args.len() != 1 {
            return;
        }
        let Expr::Name(base) = &args.args[0] else {
            return;
        };
        if base.id.as_str() != "Enum" {
            return;
        }
        // every member must be `NAME = auto()`
        if class.body.is_empty() {
            return;
        }
        let mut member_edits = Vec::with_capacity(class.body.len());
        for stmt in &class.body {
            let Stmt::Assign(assign) = stmt else {
                return;
            };
            if assign.targets.len() != 1 || !matches!(&assign.targets[0], Expr::Name(_)) {
                return;
            }
            let Expr::Call(call) = assign.value.as_ref() else {
                return;
            };
            let Expr::Name(func) = call.func.as_ref() else {
                return;
            };
            if func.id.as_str() != "auto"
                || !call.arguments.args.is_empty()
                || !call.arguments.keywords.is_empty()
            {
                return;
            }
            // `NAME = auto()` → `case NAME`
            member_edits.push(Fix::safe_edit(Edit::insertion(
                "case ".to_owned(),
                assign.targets[0].range().start(),
            )));
            member_edits.push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                assign.targets[0].range().end(),
                call.range().end(),
            ))));
        }

        // rewrite the header `class X(Enum)` → `enum class X`
        let header = TextRange::new(class.range().start(), args.range().end());
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            format!("enum class {}", class.name.as_str()),
            header,
        )));
        self.edits.extend(member_edits);
    }
}

impl<'ast> Visitor<'ast> for EnumsReverse {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::ClassDef(class) = stmt {
            self.try_reverse(class);
        }
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    #[test]
    fn simple_auto_enum_reverses() {
        check(
            indoc! {"
                from enum import Enum, auto
                class Color(Enum):
                    Red = auto()
                    Green = auto()
                    Blue = auto()
            "},
            indoc! {"
                from enum import Enum, auto
                enum class Color:
                    case Red
                    case Green
                    case Blue
            "},
        );
    }

    #[test]
    fn enum_with_explicit_values_unchanged() {
        // not an all-`auto()` enum — left as-is
        check(
            indoc! {"
                from enum import Enum
                class Color(Enum):
                    Red = 1
                    Green = 2
            "},
            indoc! {"
                from enum import Enum
                class Color(Enum):
                    Red = 1
                    Green = 2
            "},
        );
    }

    #[test]
    fn enum_with_methods_not_reversed_to_enum() {
        // a member that isn't `NAME = auto()` blocks the rewrite; the class
        // keeps its `class …(Enum)` form
        check(
            indoc! {"
                from enum import Enum, auto
                class Color(Enum):
                    Red = auto()
                    def f(self): return 0
            "},
            indoc! {"
                from enum import Enum, auto
                class Color(Enum):
                    Red = auto()
                    def f(self): return 0
            "},
        );
    }
}
