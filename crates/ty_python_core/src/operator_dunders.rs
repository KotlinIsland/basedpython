//! the operator dunders python hands an operand that they may hand back

use ruff_python_ast as ast;

/// the operators whose dunders python hands an operand it may hand back: each binary operator's
/// dunder, its reflected dunder and its in-place dunder
const OPERATORS: [ast::Operator; 13] = [
    ast::Operator::Add,
    ast::Operator::Sub,
    ast::Operator::Mult,
    ast::Operator::MatMult,
    ast::Operator::Div,
    ast::Operator::Mod,
    ast::Operator::Pow,
    ast::Operator::LShift,
    ast::Operator::RShift,
    ast::Operator::BitOr,
    ast::Operator::BitXor,
    ast::Operator::BitAnd,
    ast::Operator::FloorDiv,
];

/// whether python moves on from a method called `name` that returns `NotImplemented`: an
/// operator dunder, a reflected or in-place one, or a rich comparison
pub fn hands_back_operands(name: &str) -> bool {
    matches!(
        name,
        "__eq__" | "__ne__" | "__lt__" | "__le__" | "__gt__" | "__ge__"
    ) || OPERATORS.iter().any(|operator| {
        [
            operator.dunder(),
            operator.reflected_dunder(),
            operator.in_place_dunder(),
        ]
        .contains(&name)
    })
}
