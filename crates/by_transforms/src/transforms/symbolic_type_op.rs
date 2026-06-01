//! AST pass that folds symbolic operations in type positions to the type ty
//! resolves them to.
//!
//! `c: 1 + 1`        → `c: Literal[2]`
//! `c: A + B`        → `c: Literal[3]`   (`A`, `B` literal type aliases)
//! `e: 1 + typeof d` → `e: Literal[3]`
//! `x: list[1 + 1]`  → `x: list[Literal[2]]`
//!
//! ty already evaluates arithmetic on literal types in a type expression (see
//! `infer_type_expression`); this pass reads that resolved type back via
//! [`TypeInfo::symbolic_type_fold`] and rewrites the source to it.
//!
//! the output is driven by a *text edit* per operation, so it composes with
//! sibling rewrites — e.g. `1 + 1 | 4` folds the `1 + 1` arm while
//! `literal_types` still wraps the `4`. but the pass *also* replaces the node
//! in the working AST without marking the statement changed. that mutation is
//! what lets it run before `typeof` lowering: a `typeof` operand (`1 + typeof
//! d`) disappears from the AST here, so the `typeof` pass never sees it and
//! never claims the statement out from under the text edit. if some *other*
//! pass does end up re-rendering the statement, the AST already carries the
//! resolved type, so the result stays correct either way.

use std::cell::RefCell;
use std::collections::HashMap;

use ruff_python_ast::visitor::transformer::{Transformer, walk_expr};
use ruff_python_ast::{Expr, ModModule, Operator, Stmt, UnaryOp};
use ruff_python_parser::parse_expression;
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{AstPass, PassContext};
use super::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_type_positions};
use crate::type_info::TypeInfo;

/// One resolved operation: the replacement node (spliced into the working AST)
/// and its rendered text (emitted as the output edit).
struct Fold {
    node: Expr,
    rendered: String,
}

/// The replacements computed for one module, keyed by each operation's original
/// source range.
pub(crate) struct SymbolicFolds {
    folds: HashMap<TextRange, Fold>,
    /// whether any replacement references `typing.Literal`, so the driver can
    /// add the import
    pub(crate) needs_literal_import: bool,
    /// whether any replacement is `Any` (e.g. `dynamic + 1` folds to `Any`), so
    /// the driver can add `from typing import Any`
    pub(crate) needs_any_import: bool,
}

impl SymbolicFolds {
    /// the source range of every operation this fold replaces. later type-aware
    /// passes skip these ranges so they don't re-process (and emit stale edits
    /// or imports for) an operation that no longer appears in the output
    pub(crate) fn claimed_ranges(&self) -> Vec<TextRange> {
        self.folds.keys().copied().collect()
    }
}

/// Walk every type position and resolve each non-union/non-intersection binary
/// operation to the type ty inferred for it, parsed into a replacement node.
pub(crate) fn collect_symbolic_folds(stmts: &[Stmt], types: &dyn TypeInfo) -> SymbolicFolds {
    let mut collector = FoldCollector {
        types,
        folds: HashMap::new(),
        needs_literal_import: false,
        needs_any_import: false,
    };
    walk_type_positions(stmts, Some(types), &mut collector);
    SymbolicFolds {
        folds: collector.folds,
        needs_literal_import: collector.needs_literal_import,
        needs_any_import: collector.needs_any_import,
    }
}

struct FoldCollector<'a> {
    types: &'a dyn TypeInfo,
    folds: HashMap<TextRange, Fold>,
    needs_literal_import: bool,
    needs_any_import: bool,
}

impl TypeExprVisitor for FoldCollector<'_> {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        // which nodes are symbolic operations to fold:
        // - binary ops other than `|` (union) and `&` (intersection), which
        //   have dedicated lowerings
        // - *arithmetic* unary ops (`~`, and a multiply-negated literal like
        //   `- -3`). `not` is the `Not[]` feature and `?` / `^` / `!` are the
        //   wrapped-optional operators — none are arithmetic, so they keep their
        //   own lowerings. a bare signed numeric literal (`-3`, `-3.0j`) is a
        //   literal value owned by `literal_types`, not an operation.
        let foldable = match expr {
            Expr::BinOp(b) => !matches!(b.op, Operator::BitOr | Operator::BitAnd),
            Expr::UnaryOp(u) => {
                matches!(u.op, UnaryOp::Invert | UnaryOp::USub | UnaryOp::UAdd)
                    && !matches!(
                        (u.op, u.operand.as_ref()),
                        (UnaryOp::USub | UnaryOp::UAdd, Expr::NumberLiteral(_))
                    )
            }
            _ => false,
        };
        if !foldable {
            return Recurse::Descend;
        }
        let Some(rendered) = self.types.symbolic_type_fold(expr) else {
            return Recurse::Descend;
        };
        // the special float-literal types render as the bare names `inf` /
        // `-inf` / `nan`, which have no python literal syntax — leave them for
        // `float_const` to erase to `float` rather than folding to an undefined
        // name (only `float.inf` etc. produce these, so this never shadows a
        // real arithmetic fold)
        if matches!(rendered.as_str(), "inf" | "-inf" | "nan") {
            return Recurse::Descend;
        }
        // the rendered type must itself parse as a type expression; if ty
        // produced something we can't splice back (unexpected for arithmetic),
        // leave the source untouched
        let Ok(parsed) = parse_expression(&rendered) else {
            return Recurse::Descend;
        };
        if rendered.contains("Literal[") {
            self.needs_literal_import = true;
        }
        if rendered == "Any" {
            self.needs_any_import = true;
        }
        self.folds.insert(
            expr.range(),
            Fold {
                node: *parsed.into_syntax().body,
                rendered,
            },
        );
        // the whole operation is replaced; its operands are gone from the output
        Recurse::Stop
    }
}

pub(crate) struct SymbolicTypeOp {
    folds: HashMap<TextRange, Fold>,
    edits: RefCell<Vec<(TextRange, String)>>,
}

impl SymbolicTypeOp {
    pub(crate) fn new(folds: SymbolicFolds) -> Self {
        Self {
            folds: folds.folds,
            edits: RefCell::new(Vec::new()),
        }
    }
}

impl Transformer for SymbolicTypeOp {
    fn visit_expr(&self, expr: &mut Expr) {
        // the module is a fresh parse of the same source the folds were keyed
        // against, so ranges line up exactly. match before descending so a
        // folded operand (e.g. a nested `typeof`) is consumed with its parent
        if let Some(fold) = self.folds.get(&expr.range()) {
            self.edits
                .borrow_mut()
                .push((expr.range(), fold.rendered.clone()));
            *expr = fold.node.clone();
            return;
        }
        walk_expr(self, expr);
    }
}

impl AstPass for SymbolicTypeOp {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        // mutate the working AST (so `typeof` and other AST passes never see the
        // consumed operands) but drive the output through text edits, leaving the
        // statement off `ctx.changed` so sibling rewrites still apply
        for stmt in &mut module.body {
            self.visit_stmt(stmt);
        }
        ctx.text_edits.extend(self.edits.borrow_mut().drain(..));
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, PythonVersion, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    fn check_py312(input: &str, expected: &str) {
        let config = Config {
            min_version: PythonVersion::PY312,
            ..Config::test_default()
        };
        assert_eq!(
            transpile(input, &config).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn plain_int_addition() {
        check(
            "c: 1 + 1\n",
            indoc! {"
                from typing import Literal
                c: Literal[2]
            "},
        );
    }

    #[test]
    fn unary_operations() {
        // `~` and a multiply-negated literal are genuine unary operations that
        // ty folds — they must be rewritten like binary ops, not left verbatim
        check(
            "a: ~0\n",
            indoc! {"
                from typing import Literal
                a: Literal[-1]
            "},
        );
        check(
            "a: - - 3\n",
            indoc! {"
                from typing import Literal
                a: Literal[3]
            "},
        );
        check(
            "a: ~~5\n",
            indoc! {"
                from typing import Literal
                a: Literal[5]
            "},
        );
    }

    #[test]
    fn dynamic_operand_folds_to_any() {
        // `dynamic` is `Any`, so `dynamic + 1` resolves to `Any` — fold it (with
        // the import) rather than leaking `dynamic + 1`, which crashes at runtime
        check(
            "a: dynamic + 1\n",
            indoc! {"
                from typing import Any
                a: Any
            "},
        );
    }

    #[test]
    fn bare_negative_literal_left_to_literal_types() {
        // a single signed numeric literal is a literal value, not an operation;
        // it is still promoted, just not by the symbolic-op pass
        check(
            "a: -3\n",
            indoc! {"
                from typing import Literal
                a: Literal[-3]
            "},
        );
    }

    #[test]
    fn type_alias_operands_312() {
        check_py312(
            indoc! {"
                type A = 1
                type B = 2
                c: A + B
            "},
            indoc! {"
                from typing import Literal
                type A = Literal[1]
                type B = Literal[2]
                c: Literal[3]
            "},
        );
    }

    #[test]
    fn typeof_operand() {
        // the `typeof` is consumed by the fold — no `TypeOf` import survives
        check(
            indoc! {"
                d = 2
                e: 1 + typeof d
            "},
            indoc! {"
                from typing import Literal
                d = 2
                e: Literal[3]
            "},
        );
    }

    #[test]
    fn user_example_end_to_end() {
        // the full example from the feature request, at the default version
        // (type aliases polyfilled). both `c` and `e` resolve to `Literal[3]`
        // and no dead `TypeOf` import survives the consumed `typeof`
        check(
            indoc! {"
                type A = 1
                type B = 2

                c: A + B

                let d = 2

                e: 1 + typeof d
            "},
            indoc! {"
                from typing import Final, Literal
                from typing_extensions import TypeAliasType
                A = TypeAliasType(\"A\", Literal[1])
                B = TypeAliasType(\"B\", Literal[2])

                c: Literal[3]

                d: Final = 2

                e: Literal[3]
            "},
        );
    }

    #[test]
    fn let_and_typeof() {
        check(
            indoc! {"
                let d = 2
                e: 1 + typeof d
            "},
            indoc! {"
                from typing import Final, Literal
                d: Final = 2
                e: Literal[3]
            "},
        );
    }

    #[test]
    fn variety_of_operators() {
        check(
            indoc! {"
                a: 5 - 2
                b: 3 * 4
                c: 2 ** 8
            "},
            indoc! {"
                from typing import Literal
                a: Literal[3]
                b: Literal[12]
                c: Literal[256]
            "},
        );
    }

    #[test]
    fn negative_operand() {
        check(
            "x: -3 * 2\n",
            indoc! {"
                from typing import Literal
                x: Literal[-6]
            "},
        );
    }

    #[test]
    fn union_arm_folds_and_sibling_literal_wraps() {
        // the `1 + 1` arm folds while `literal_types` still wraps the `4` —
        // text-edit output composes where node replacement would not
        check(
            "x: 1 + 1 | 4\n",
            indoc! {"
                from typing import Literal
                x: Literal[2] | Literal[4]
            "},
        );
    }

    #[test]
    fn string_concatenation() {
        check(
            "s: \"foo\" + \"bar\"\n",
            indoc! {"
                from typing import Literal
                s: Literal[\"foobar\"]
            "},
        );
    }

    #[test]
    fn nested_in_subscript() {
        check(
            "x: list[1 + 1]\n",
            indoc! {"
                from typing import Literal
                x: list[Literal[2]]
            "},
        );
    }

    #[test]
    fn typeof_operand_nested_in_subscript() {
        // the fold consumes the `typeof` operand even inside a subscript slice,
        // so no `TypeOf` survives and the whole operation collapses to its type
        check(
            "let d = 2\nx: list[1 + typeof d]\n",
            indoc! {"
                from typing import Final, Literal
                d: Final = 2
                x: list[Literal[3]]
            "},
        );
    }

    #[test]
    fn chained_addition() {
        check(
            "a: 1 + 2 + 3\n",
            indoc! {"
                from typing import Literal
                a: Literal[6]
            "},
        );
    }

    #[test]
    fn function_parameter_and_return() {
        check(
            indoc! {"
                def f(x: 2 * 3) -> 4 + 4:
                    return 8
            "},
            indoc! {"
                from typing import Literal
                def f(x: Literal[6]) -> Literal[8]:
                    return 8
            "},
        );
    }

    #[test]
    fn unsupported_operation_left_untouched() {
        // `A + B` between two classes is not a meaningful type; ty resolves it
        // to `Unknown`, so the fold leaves the source alone (ty still errors)
        check(
            indoc! {"
                class A: ...
                class B: ...
                bad: A + B
            "},
            indoc! {"
                class A: ...
                class B: ...
                bad: A + B
            "},
        );
    }

    #[test]
    fn value_position_unchanged() {
        // a binary operation in value position is ordinary arithmetic
        check("x = 1 + 1\n", "x = 1 + 1\n");
    }

    #[test]
    fn existing_literal_import_not_duplicated() {
        check(
            indoc! {"
                from typing import Literal
                c: 1 + 1
            "},
            indoc! {"
                from typing import Literal
                c: Literal[2]
            "},
        );
    }

    #[test]
    fn python_passthrough_unchanged() {
        unchanged("c: 1 + 1\n");
    }
}
