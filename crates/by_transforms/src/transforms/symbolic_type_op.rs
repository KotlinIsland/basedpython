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
use ruff_python_ast::{CmpOp, Expr, ModModule, Operator, Stmt, UnaryOp};
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

    /// each fold as a `(range, rendered)` substitution. a pass whose own edit
    /// *subsumes* a folded operation — the `TypeAliasType` polyfill rewrites the
    /// whole `type X = …` statement — would otherwise re-emit the operand from
    /// source and drop the fold, leaving `_T.a` / `_Dim + 1` to be evaluated at
    /// runtime. such a pass splices these in instead
    pub(crate) fn substitutions(&self) -> Vec<(TextRange, String)> {
        self.folds
            .iter()
            .map(|(range, fold)| (*range, fold.rendered.clone()))
            .collect()
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
            // a single rich comparison in a type position (`I < 10`) folds to the
            // type ty gives it; identity/membership operators keep their own
            // lowerings (`a is T` → `TypeIs[T]`)
            Expr::Compare(c) => {
                c.ops.len() == 1
                    && matches!(
                        c.ops[0],
                        CmpOp::Eq | CmpOp::NotEq | CmpOp::Lt | CmpOp::LtE | CmpOp::Gt | CmpOp::GtE
                    )
            }
            // a positional method call in a type position (`S.startswith("foo")`)
            // folds to the type ty gives it, so the annotation is runtime-safe
            Expr::Call(call) => {
                matches!(call.func.as_ref(), Expr::Attribute(_))
                    && call.arguments.keywords.is_empty()
                    && !call.arguments.args.iter().any(Expr::is_starred_expr)
            }
            // basedpython: `F[bool]` where `F` is a `type def` folds to the type
            // the type function returned, so the emitted python names a real type
            // and carries no trace of the type function
            Expr::Subscript(_) => self.types.is_type_fn_application(expr),
            // basedpython: an attribute type (`T.a`) folds to the member's type on the
            // type parameter's bound — python cannot express the dependency on `T`, and
            // the bound's member type is the guarantee every specialization satisfies
            Expr::Attribute(_) => self.types.is_attribute_type(expr),
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
        // the rendered type must itself parse as a type expression, and `Unknown`
        // is not a runtime name either
        let (rendered, parsed) = match parse_expression(&rendered) {
            Ok(parsed) if rendered != "Unknown" => (rendered, parsed),
            // some forms *must* be replaced, because their source spelling names
            // nothing at runtime: a `type def` application (its declaration is
            // erased, so the name would dangle) and an attribute type (`T.a` is an
            // attribute access on a `TypeVar` object). `Any` is the honest
            // spelling when the resolved type has none — a deferred application,
            // or a member python cannot write down such as a bound method or a
            // callable, which ty renders in arrow form. anything else keeps its
            // source so ty's own diagnostic stands.
            //
            // the widening is deliberately silent: it is the same trade every
            // deferred operation already makes when it lowers to its reduced form
            // (`Array[Dim + 1]` → `Array[int]`), the `.by` file keeps the precise
            // type either way, and the transpiler has no warning channel — only
            // hard errors, which would reject perfectly good source
            _ => {
                if !(self.types.is_type_fn_application(expr) || self.types.is_attribute_type(expr))
                {
                    return Recurse::Descend;
                }
                let Ok(parsed) = parse_expression("Any") else {
                    return Recurse::Descend;
                };
                ("Any".to_string(), parsed)
            }
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
    fn rich_comparison_folds() {
        // a comparison in a type position folds to the type ty gives it, so the
        // annotation is runtime-safe (`I < 10` with a type parameter folds to `bool`;
        // a concrete comparison folds to the literal result)
        check(
            "c: 1 < 2\n",
            indoc! {"
                from typing import Literal
                c: Literal[True]
            "},
        );
    }

    #[test]
    fn method_call_folds() {
        // a method call in a type position folds to the type ty gives it, rather than
        // leaking a call that would crash at runtime
        check(
            "d: \"ab\".startswith(\"a\")\n",
            indoc! {"
                from typing import Literal
                d: Literal[True]
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
    fn attribute_type_folds_to_the_bound_member() {
        check_py312(
            indoc! {"
                class A:
                    a: int

                class B[T: A]:
                    x: T.a
            "},
            indoc! {"
                class A:
                    a: int

                class B[T: A]:
                    x: int
            "},
        );
    }

    #[test]
    fn attribute_type_in_signature_and_subscript() {
        check_py312(
            indoc! {"
                class A:
                    a: int

                def f[T: A](t: T, v: T.a) -> list[T.a]:
                    return [v]
            "},
            indoc! {"
                class A:
                    a: int

                def f[T: A](t: T, v: int) -> list[int]:
                    return [v]
            "},
        );
    }

    #[test]
    fn ordinary_dotted_annotation_unchanged() {
        // a dotted name that already denotes a type is not an attribute type —
        // folding it would emit a name that is not in scope
        check_py312(
            indoc! {"
                class Outer:
                    class Inner:
                        pass

                x: Outer.Inner
            "},
            indoc! {"
                class Outer:
                    class Inner:
                        pass

                x: Outer.Inner
            "},
        );
    }

    #[test]
    fn attribute_type_at_the_typevar_lowering() {
        // the default target polyfills the type parameter to `_T = TypeVar(...)`;
        // an unfolded `T.a` would emit `_T.a`, an `AttributeError` on the `TypeVar`
        // object when the class body runs
        check(
            indoc! {"
                class A:
                    a: int

                class B[T: A]:
                    x: T.a

                def f[T: A](v: T.a) -> T.a:
                    return v
            "},
            indoc! {"
                from typing import TypeVar, Generic
                class A:
                    a: int

                _T = TypeVar(\"_T\", bound=A)
                class B(Generic[_T]):
                    x: int

                def f(v: int) -> int:
                    return v
            "},
        );
    }

    #[test]
    fn attribute_type_over_a_specialized_receiver() {
        // a ground receiver folds in ty the moment it is written, so there is no
        // `Deferred` left for the type-driven test — the shape is what marks it. an
        // unfolded `X[A].x` is an `AttributeError` on the generic alias at runtime
        check_py312(
            indoc! {"
                class A:
                    a: int

                class X[T: A]:
                    x: T
                    y: T.a

                class Z:
                    plain: X[A].x
                    composed: X[A].y
                    chained: X[A].x.a
                    nested: list[X[A].y]
            "},
            indoc! {"
                class A:
                    a: int

                class X[T: A]:
                    x: T
                    y: int

                class Z:
                    plain: A
                    composed: int
                    chained: int
                    nested: list[int]
            "},
        );
    }

    #[test]
    fn attribute_type_in_a_generic_type_alias() {
        // the `TypeAliasType` polyfill replaces the whole statement, so it has to
        // splice the fold in — otherwise `_T.a` is evaluated at import
        check(
            indoc! {"
                class A:
                    a: int

                type Alias[T: A] = T.a
            "},
            indoc! {"
                from typing import TypeVar
                from typing_extensions import TypeAliasType
                class A:
                    a: int

                _T = TypeVar(\"_T\", bound=A)
                Alias = TypeAliasType(\"Alias\", int, type_params=(_T,))
            "},
        );
    }

    #[test]
    fn type_alias_composes_a_fold_with_a_typevar_rename() {
        // the alias polyfill renames `T` to `_T` *and* splices the `T.a` fold; before
        // these shared one substitution set it could only apply one of the two
        check(
            indoc! {"
                class A:
                    a: int

                type Alias[T: A] = dict[T, T.a]
            "},
            indoc! {"
                from typing import TypeVar
                from typing_extensions import TypeAliasType
                class A:
                    a: int

                _T = TypeVar(\"_T\", bound=A)
                Alias = TypeAliasType(\"Alias\", dict[_T, int], type_params=(_T,))
            "},
        );
    }

    #[test]
    fn arithmetic_type_alias_is_folded_too() {
        // the same subsumption applied to the sibling deferred-operation feature:
        // `_D + 1` would be a `TypeError` at import
        check(
            indoc! {"
                type Arith[D: int] = D + 1
            "},
            indoc! {"
                from typing import TypeVar
                from typing_extensions import TypeAliasType
                _D = TypeVar(\"_D\", bound=int)
                Arith = TypeAliasType(\"Arith\", int, type_params=(_D,))
            "},
        );
    }

    #[test]
    fn attribute_type_without_a_python_spelling_degrades_to_any() {
        // a method member's type is a bound method, which python cannot write
        // down. leaving `T.m` in place would evaluate an attribute access on a
        // `TypeVar` object when the class body runs
        check_py312(
            indoc! {"
                class A:
                    def m(self) -> str:
                        return \"\"

                class B[T: A]:
                    z: T.m
            "},
            indoc! {"
                from typing import Any
                class A:
                    def m(self) -> str:
                        return \"\"

                class B[T: A]:
                    z: Any
            "},
        );
    }

    #[test]
    fn attribute_type_value_position_unchanged() {
        // outside a type position `T.a` is an ordinary attribute access
        check_py312(
            indoc! {"
                class A:
                    a: int

                def f[T: A](t: T):
                    return t.a
            "},
            indoc! {"
                class A:
                    a: int

                def f[T: A](t: T):
                    return t.a
            "},
        );
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
