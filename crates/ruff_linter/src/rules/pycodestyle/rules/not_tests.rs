use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::helpers::generate_comparison;
use ruff_python_ast::{self as ast, CmpOp, Expr};
use ruff_text_size::Ranged;

use crate::checkers::ast::Checker;
use crate::codes::Category;
use crate::fix::edits::pad;
use crate::registry::Rule;
use crate::{AlwaysFixableViolation, Edit, Fix};

/// ## What it does
/// Checks for membership tests using `not {element} in {collection}`.
///
/// ## Why is this bad?
/// Testing membership with `{element} not in {collection}` is more readable.
///
/// ## Example
/// ```python
/// Z = not X in Y
/// if not X.B in Y:
///     pass
/// ```
///
/// Use instead:
/// ```python
/// Z = X not in Y
/// if X.B not in Y:
///     pass
/// ```
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "v0.0.28", category = Category::Pedantic)]
pub(crate) struct NotInTest;

impl AlwaysFixableViolation for NotInTest {
    #[derive_message_formats]
    fn message(&self) -> String {
        "Test for membership should be `not in`".to_string()
    }

    fn fix_title(&self) -> String {
        "Convert to `not in`".to_string()
    }
}

/// ## What it does
/// Checks for identity comparisons using `not {foo} is {bar}`.
///
/// ## Why is this bad?
/// According to [PEP8], testing for an object's identity with `is not` is more
/// readable.
///
/// ## Example
/// ```python
/// if not X is Y:
///     pass
/// Z = not X.B is Y
/// ```
///
/// Use instead:
/// ```python
/// if X is not Y:
///     pass
/// Z = X.B is not Y
/// ```
///
/// [PEP8]: https://peps.python.org/pep-0008/#programming-recommendations
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "v0.0.28", category = Category::Pedantic)]
pub(crate) struct NotIsTest {
    test: IsTest,
}

impl AlwaysFixableViolation for NotIsTest {
    #[derive_message_formats]
    fn message(&self) -> String {
        let negated = self.test.negated();
        match self.test {
            IsTest::Identity | IsTest::SpelledIdentity => {
                format!("Test for object identity should be `{negated}`")
            }
            IsTest::TypeTest => format!("Test for type should be `{negated}`"),
        }
    }

    fn fix_title(&self) -> String {
        format!("Convert to `{}`", self.test.negated())
    }
}

/// what the negated `is` asks: python's identity, basedpython's identity spelled
/// `===`, or basedpython's type test spelled with the `is` keyword
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IsTest {
    Identity,
    SpelledIdentity,
    TypeTest,
}

impl IsTest {
    const fn negated(self) -> &'static str {
        match self {
            IsTest::Identity | IsTest::TypeTest => "is not",
            IsTest::SpelledIdentity => "!==",
        }
    }
}

/// E713, E714
pub(crate) fn not_tests(checker: &Checker, unary_op: &ast::ExprUnaryOp) {
    if !unary_op.op.is_not() {
        return;
    }

    let Expr::Compare(compare) = unary_op.operand.as_ref() else {
        return;
    };

    let left = compare.first_operand();
    let comparators = compare.comparators();

    match &*compare.ops {
        [CmpOp::In] if checker.is_rule_enabled(Rule::NotInTest) => {
            let mut diagnostic = checker.report_diagnostic(NotInTest, unary_op.operand.range());
            diagnostic.set_fix(Fix::safe_edit(Edit::range_replacement(
                pad(
                    generate_comparison(
                        left,
                        &[CmpOp::NotIn],
                        comparators,
                        compare.identity_ops.as_deref(),
                        unary_op.into(),
                        checker.tokens(),
                        checker.source(),
                    ),
                    unary_op.range(),
                    checker.locator(),
                ),
                unary_op.range(),
            )));
        }
        [CmpOp::Is] if checker.is_rule_enabled(Rule::NotIsTest) => {
            let test = if compare.is_type_test(0, checker.source_type) {
                IsTest::TypeTest
            } else if compare.is_identity_operator(0) {
                IsTest::SpelledIdentity
            } else {
                IsTest::Identity
            };
            let mut diagnostic =
                checker.report_diagnostic(NotIsTest { test }, unary_op.operand.range());
            diagnostic.set_fix(Fix::safe_edit(Edit::range_replacement(
                pad(
                    generate_comparison(
                        left,
                        &[CmpOp::IsNot],
                        comparators,
                        compare.identity_ops.as_deref(),
                        unary_op.into(),
                        checker.tokens(),
                        checker.source(),
                    ),
                    unary_op.range(),
                    checker.locator(),
                ),
                unary_op.range(),
            )));
        }
        _ => {}
    }
}
