//! Lowers basedpython `sentinel A` to `A = Sentinel("A")`.
//!
//! The parser emits the surface form as a synthetic
//! `AnnAssign { target: A, annotation: __sentinel__, value: None }`.
//! The annotation's range covers the `sentinel` keyword text.
//!
//! `typing_extensions.Sentinel` exists since `typing_extensions` 4.13. PEP 661
//! proposes a builtin `sentinel` for a future Python release; no shipping
//! version provides it yet, so the polyfill always imports from
//! `typing_extensions`.

use std::cell::Cell;

use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::transformer::{Transformer, walk_stmt};
use ruff_python_ast::{
    Arguments, AtomicNodeIndex, Expr, ExprCall, ExprContext, ExprName, ExprStringLiteral, Stmt,
    StmtAssign, StringLiteral, StringLiteralFlags, StringLiteralValue,
};
use ruff_text_size::TextRange;

pub(crate) struct Sentinel {
    changed: Cell<bool>,
    ever_changed: Cell<bool>,
}

impl Sentinel {
    pub(crate) fn new() -> Self {
        Self {
            changed: Cell::new(false),
            ever_changed: Cell::new(false),
        }
    }

    pub(crate) fn changed_cell(&self) -> &Cell<bool> {
        &self.changed
    }

    pub(crate) fn ever_changed(&self) -> bool {
        self.ever_changed.get()
    }
}

impl Transformer for Sentinel {
    fn visit_stmt(&self, stmt: &mut Stmt) {
        if let Stmt::AnnAssign(node) = stmt
            && node.value.is_none()
            && let Expr::Name(ann) = node.annotation.as_ref()
            && ann.id.as_str() == "__sentinel__"
            && let Expr::Name(target) = node.target.as_ref()
        {
            let name = target.id.as_str().to_owned();
            let target_expr = Expr::Name(ExprName {
                node_index: AtomicNodeIndex::NONE,
                range: TextRange::default(),
                id: Name::from(name.as_str()),
                ctx: ExprContext::Store,
            });
            let call = Expr::Call(ExprCall {
                node_index: AtomicNodeIndex::NONE,
                range_start: ruff_text_size::TextSize::default(),
                func: Box::new(Expr::Name(ExprName {
                    node_index: AtomicNodeIndex::NONE,
                    range: TextRange::default(),
                    id: Name::from("Sentinel"),
                    ctx: ExprContext::Load,
                })),
                arguments: Arguments {
                    node_index: AtomicNodeIndex::NONE,
                    range: TextRange::default(),
                    args: Box::new([Expr::StringLiteral(ExprStringLiteral {
                        node_index: AtomicNodeIndex::NONE,
                        range: TextRange::default(),
                        value: StringLiteralValue::single(StringLiteral {
                            node_index: AtomicNodeIndex::NONE,
                            range: TextRange::default(),
                            value: name.into_boxed_str(),
                            flags: StringLiteralFlags::empty()
                                .with_quote_style(ruff_python_ast::str::Quote::Double),
                        }),
                    })]),
                    keywords: thin_vec::ThinVec::new(),
                },
                cast_kind: None,
                is_string_tag: false,
            });
            *stmt = Stmt::Assign(StmtAssign {
                node_index: AtomicNodeIndex::NONE,
                range: TextRange::default(),
                targets: vec![target_expr],
                value: Box::new(call),
            });
            self.changed.set(true);
            self.ever_changed.set(true);
            return;
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
        assert_eq!(transpile(input, &Config::test_default()).unwrap(), expected);
    }

    #[test]
    fn simple() {
        check(
            "sentinel A\n",
            indoc! {"
                from typing_extensions import Sentinel
                A = Sentinel(\"A\")
            "},
        );
    }

    #[test]
    fn multiple() {
        check(
            indoc! {"
                sentinel MISSING
                sentinel UNSET
            "},
            indoc! {"
                from typing_extensions import Sentinel
                MISSING = Sentinel(\"MISSING\")
                UNSET = Sentinel(\"UNSET\")
            "},
        );
    }

    #[test]
    fn sentinel_identifier_is_passthrough_in_python() {
        unchanged("sentinel = 5\n");
    }

    #[test]
    fn sentinel_call_is_passthrough_in_python() {
        unchanged("from typing_extensions import Sentinel\nA = Sentinel(\"A\")\n");
    }
}
