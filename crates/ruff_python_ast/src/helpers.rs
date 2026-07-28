use std::borrow::Cow;
use std::path::Path;

use rustc_hash::FxHashMap;

use ruff_python_trivia::{SimpleTokenKind, SimpleTokenizer, indentation_at_offset};
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};

use crate::name::{Name, QualifiedName, QualifiedNameBuilder};
use crate::statement_visitor::StatementVisitor;
use crate::token::Tokens;
use crate::token::parenthesized_range;
use crate::visitor::Visitor;
use crate::{
    self as ast, Arguments, AtomicNodeIndex, CmpOp, DictItem, ExceptHandler, Expr, ExprNoneLiteral,
    InterpolatedStringElement, MatchCase, Operator, Pattern, Stmt, TypeParam,
};
use crate::{AnyNodeRef, ExprContext};

/// Return `true` if the `Stmt` is a compound statement (as opposed to a simple statement).
pub const fn is_compound_statement(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::FunctionDef(_)
            | Stmt::ClassDef(_)
            | Stmt::While(_)
            | Stmt::For(_)
            | Stmt::Match(_)
            | Stmt::With(_)
            | Stmt::If(_)
            | Stmt::Try(_)
    )
}

fn is_iterable_initializer<F>(id: &str, is_builtin: F) -> bool
where
    F: Fn(&str) -> bool,
{
    matches!(id, "list" | "tuple" | "set" | "dict" | "frozenset") && is_builtin(id)
}

/// Whether an expression has no side effects, may have side effects,
/// or is assumed to have side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideEffect {
    /// The expression is definitely side-effect-free.
    Absent,
    /// The expression may have side effects (e.g., f-string interpolation
    /// may invoke `__format__` or `__str__`).
    Possible,
    /// The expression is assumed to have side effects.
    Present,
}

impl SideEffect {
    pub const fn is_present(self) -> bool {
        matches!(self, Self::Present)
    }

    pub const fn is_absent(self) -> bool {
        matches!(self, Self::Absent)
    }

    #[must_use]
    pub const fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Present, _) | (_, Self::Present) => Self::Present,
            (Self::Possible, _) | (_, Self::Possible) => Self::Possible,
            _ => Self::Absent,
        }
    }

    /// Classify a single expression node's side effect.
    fn from_expr(expr: &Expr, is_builtin: &dyn Fn(&str) -> bool) -> Self {
        match expr {
            // Empty initializers for known builtins are side-effect-free.
            Expr::Call(ast::ExprCall {
                func, arguments, ..
            }) if arguments.is_empty() => {
                if let Expr::Name(ast::ExprName { id, .. }) = func.as_ref() {
                    if is_iterable_initializer(id.as_str(), |id| is_builtin(id)) {
                        return Self::Absent;
                    }
                }
                Self::Present
            }

            // Overloaded operators: only side-effect-free if both sides are literals.
            Expr::BinOp(ast::ExprBinOp { left, right, .. }) => {
                if is_known_safe_binop_operand(left) && is_known_safe_binop_operand(right) {
                    Self::Absent
                } else {
                    Self::Present
                }
            }

            // Non-literal f-string interpolation may invoke `__format__`/`__str__`.
            Expr::FString(ast::ExprFString { value, .. }) => {
                if value.elements().any(has_uncertain_interpolation) {
                    Self::Possible
                } else {
                    Self::Absent
                }
            }
            Expr::TString(ast::ExprTString { value, .. }) => {
                if value.elements().any(has_uncertain_interpolation) {
                    Self::Possible
                } else {
                    Self::Absent
                }
            }

            // Named expressions (walrus operator) are assignments.
            Expr::Named(_) => Self::Present,

            // Complex expressions that are assumed to have side effects.
            Expr::Await(_)
            | Expr::Call(_)
            | Expr::DictComp(_)
            | Expr::Generator(_)
            | Expr::ListComp(_)
            | Expr::SetComp(_)
            | Expr::Subscript(_)
            | Expr::Yield(_)
            | Expr::YieldFrom(_)
            | Expr::IpyEscapeCommand(_) => Self::Present,
            Expr::CallableType(_) | Expr::ProtocolType(_) | Expr::ProtocolMethod(_) => Self::Absent,
            // a statement expression runs arbitrary statements
            Expr::Statement(_) => Self::Present,

            // Side-effect-free expressions — continue walking child nodes.
            Expr::BoolOp(_)
            | Expr::Compare(_)
            | Expr::Dict(_)
            | Expr::If(_)
            | Expr::Lambda(_)
            | Expr::List(_)
            | Expr::Set(_)
            | Expr::Slice(_)
            | Expr::Starred(_)
            | Expr::Tuple(_)
            | Expr::UnaryOp(_)
            | Expr::Attribute(_)
            | Expr::Name(_)
            | Expr::StringLiteral(_)
            | Expr::BytesLiteral(_)
            | Expr::NumberLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::NoneLiteral(_)
            | Expr::EllipsisLiteral(_) => Self::Absent,
        }
    }
}

const fn is_known_safe_binop_operand(expr: &Expr) -> bool {
    match expr {
        Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::NumberLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::EllipsisLiteral(_)
        | Expr::FString(_)
        | Expr::List(_)
        | Expr::Tuple(_)
        | Expr::Set(_)
        | Expr::Dict(_)
        | Expr::ListComp(_)
        | Expr::SetComp(_)
        | Expr::DictComp(_) => true,

        Expr::BoolOp(_)
        | Expr::Named(_)
        | Expr::BinOp(_)
        | Expr::UnaryOp(_)
        | Expr::Lambda(_)
        | Expr::If(_)
        | Expr::Compare(_)
        | Expr::Call(_)
        | Expr::Generator(_)
        | Expr::Await(_)
        | Expr::Yield(_)
        | Expr::YieldFrom(_)
        | Expr::Attribute(_)
        | Expr::Subscript(_)
        | Expr::Starred(_)
        | Expr::Name(_)
        | Expr::Slice(_)
        | Expr::IpyEscapeCommand(_)
        | Expr::CallableType(_)
        | Expr::ProtocolType(_)
        | Expr::ProtocolMethod(_)
        | Expr::Statement(_)
        | Expr::TString(_) => false,
    }
}

fn is_definitely_side_effect_free_interpolation_expr(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::NumberLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::NoneLiteral(_)
            | Expr::EllipsisLiteral(_)
            | Expr::StringLiteral(_)
            | Expr::BytesLiteral(_)
    )
}

fn has_uncertain_interpolation(element: &InterpolatedStringElement) -> bool {
    match element {
        InterpolatedStringElement::Literal(_) => false,
        InterpolatedStringElement::Interpolation(interp) => {
            !is_definitely_side_effect_free_interpolation_expr(&interp.expression)
                || interp
                    .format_spec
                    .as_ref()
                    .is_some_and(|spec| spec.elements.iter().any(has_uncertain_interpolation))
        }
    }
}

/// Return `true` if the `Expr` contains an expression that appears to include a
/// side-effect (like a function call).
///
/// Accepts a closure that determines whether a given name (e.g., `"list"`) is a Python builtin.
pub fn contains_effect<F>(expr: &Expr, is_builtin: F) -> bool
where
    F: Fn(&str) -> bool,
{
    side_effect(expr, is_builtin).is_present()
}

/// Return whether `expr` has no side effects, maybe has side effects, or definitely
/// has side effects.
///
/// Unlike [`contains_effect`], which returns a simple `bool`, this function distinguishes
/// between expressions that are definitely side-effect-free, definitely side-effectful,
/// and those that may invoke user-defined code (e.g., formatting a non-literal f-string
/// interpolation can call `__format__` or `__str__`).
pub fn side_effect<F>(expr: &Expr, is_builtin: F) -> SideEffect
where
    F: Fn(&str) -> bool,
{
    let mut effect = SideEffect::Absent;
    any_over_expr(expr, |expr| {
        match SideEffect::from_expr(expr, &is_builtin) {
            SideEffect::Present => {
                effect = SideEffect::Present;
                true
            }
            SideEffect::Possible => {
                effect = effect.merge(SideEffect::Possible);
                false
            }
            SideEffect::Absent => false,
        }
    });
    effect
}

/// Call `func` over every `Expr` in `expr`, returning `true` if any expression
/// returns `true`..
pub fn any_over_expr<F>(expr: &Expr, mut func: F) -> bool
where
    F: FnMut(&Expr) -> bool,
{
    fn inner(expr: &Expr, func: &mut dyn FnMut(&Expr) -> bool) -> bool {
        if func(expr) {
            return true;
        }
        match expr {
            Expr::BoolOp(ast::ExprBoolOp { values, .. }) => {
                values.iter().any(|expr| any_over_expr(expr, &mut *func))
            }
            Expr::FString(ast::ExprFString { value, .. }) => value
                .elements()
                .any(|expr| any_over_interpolated_string_element(expr, &mut *func)),
            Expr::TString(ast::ExprTString { value, .. }) => value
                .elements()
                .any(|expr| any_over_interpolated_string_element(expr, &mut *func)),
            Expr::Named(ast::ExprNamed {
                target,
                value,
                range: _,
                node_index: _,
            }) => any_over_expr(target, &mut *func) || any_over_expr(value, &mut *func),
            Expr::BinOp(ast::ExprBinOp { left, right, .. }) => {
                any_over_expr(left, &mut *func) || any_over_expr(right, &mut *func)
            }
            Expr::UnaryOp(ast::ExprUnaryOp { operand, .. }) => any_over_expr(operand, func),
            Expr::Lambda(ast::ExprLambda { body, .. }) => any_over_expr(body, func),
            Expr::If(ast::ExprIf {
                test,
                body,
                orelse,
                range: _,
                node_index: _,
            }) => {
                any_over_expr(test, &mut *func)
                    || any_over_expr(body, &mut *func)
                    || any_over_expr(orelse, &mut *func)
            }
            Expr::Dict(ast::ExprDict {
                items,
                range: _,
                node_index: _,
            }) => items.iter().any(|ast::DictItem { key, value }| {
                any_over_expr(value, &mut *func)
                    || key
                        .as_ref()
                        .is_some_and(|key| any_over_expr(key, &mut *func))
            }),
            Expr::Set(ast::ExprSet {
                elts,
                range: _,
                node_index: _,
            })
            | Expr::List(ast::ExprList { elts, .. })
            | Expr::Tuple(ast::ExprTuple { elts, .. }) => {
                elts.iter().any(|expr| any_over_expr(expr, &mut *func))
            }
            Expr::ListComp(ast::ExprListComp {
                elt,
                generators,
                range: _,
                node_index: _,
            })
            | Expr::SetComp(ast::ExprSetComp {
                elt,
                generators,
                range: _,
                node_index: _,
            })
            | Expr::Generator(ast::ExprGenerator {
                elt,
                generators,
                range: _,
                node_index: _,
                parenthesized: _,
            }) => {
                any_over_expr(elt, &mut *func)
                    || generators.iter().any(|generator| {
                        any_over_expr(&generator.target, &mut *func)
                            || any_over_expr(&generator.iter, &mut *func)
                            || generator
                                .ifs
                                .iter()
                                .any(|expr| any_over_expr(expr, &mut *func))
                    })
            }
            Expr::DictComp(ast::ExprDictComp {
                key,
                value,
                generators,
                range: _,
                node_index: _,
            }) => {
                key.as_deref()
                    .is_some_and(|key| any_over_expr(key, &mut *func))
                    || any_over_expr(value, &mut *func)
                    || generators.iter().any(|generator| {
                        any_over_expr(&generator.target, &mut *func)
                            || any_over_expr(&generator.iter, &mut *func)
                            || generator
                                .ifs
                                .iter()
                                .any(|expr| any_over_expr(expr, &mut *func))
                    })
            }
            Expr::Await(ast::ExprAwait {
                value,
                postfix: _,
                range: _,
                node_index: _,
            })
            | Expr::YieldFrom(ast::ExprYieldFrom {
                value,
                range: _,
                node_index: _,
            })
            | Expr::Attribute(ast::ExprAttribute { value, .. })
            | Expr::Starred(ast::ExprStarred { value, .. }) => any_over_expr(value, func),
            Expr::Yield(ast::ExprYield {
                value,
                range: _,
                node_index: _,
            }) => value
                .as_ref()
                .is_some_and(|value| any_over_expr(value, func)),
            Expr::Compare(ast::ExprCompare {
                left, comparators, ..
            }) => {
                any_over_expr(left, &mut *func)
                    || comparators
                        .iter()
                        .any(|expr| any_over_expr(expr, &mut *func))
            }
            Expr::Call(ast::ExprCall {
                func: call_func,
                arguments,
                range: _,
                node_index: _,
                is_cast: _,
                is_checked_cast: _,
                is_string_tag: _,
            }) => {
                any_over_expr(call_func, &mut *func)
                    // Note that this is the evaluation order but not necessarily the declaration order
                    // (e.g. for `f(*args, a=2, *args2, **kwargs)` it's not)
                    || arguments.args.iter().any(|expr| any_over_expr(expr, &mut *func))
                    || arguments.keywords
                        .iter()
                        .any(|keyword| any_over_expr(&keyword.value, &mut *func))
            }
            Expr::Subscript(ast::ExprSubscript { value, slice, .. }) => {
                any_over_expr(value, &mut *func) || any_over_expr(slice, &mut *func)
            }
            Expr::Slice(ast::ExprSlice {
                lower,
                upper,
                step,
                range: _,
                node_index: _,
            }) => {
                lower
                    .as_ref()
                    .is_some_and(|value| any_over_expr(value, &mut *func))
                    || upper
                        .as_ref()
                        .is_some_and(|value| any_over_expr(value, &mut *func))
                    || step
                        .as_ref()
                        .is_some_and(|value| any_over_expr(value, &mut *func))
            }
            Expr::CallableType(ast::ExprCallableType {
                receiver,
                args,
                returns,
                ..
            }) => {
                receiver
                    .as_ref()
                    .is_some_and(|receiver| any_over_expr(receiver, &mut *func))
                    || args.iter().any(|e| any_over_expr(e, &mut *func))
                    || any_over_expr(returns, &mut *func)
            }
            Expr::ProtocolType(ast::ExprProtocolType { members, .. }) => {
                members.iter().any(|e| any_over_expr(e, &mut *func))
            }
            Expr::ProtocolMethod(ast::ExprProtocolMethod { signature, .. }) => {
                any_over_expr(signature, &mut *func)
            }
            Expr::Statement(ast::ExprStatement { stmt, .. }) => any_over_stmt(stmt, &mut *func),
            Expr::Name(_)
            | Expr::StringLiteral(_)
            | Expr::BytesLiteral(_)
            | Expr::NumberLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::NoneLiteral(_)
            | Expr::EllipsisLiteral(_)
            | Expr::IpyEscapeCommand(_) => false,
        }
    }

    inner(expr, &mut func)
}

fn any_over_type_param(type_param: &TypeParam, func: &mut dyn FnMut(&Expr) -> bool) -> bool {
    match type_param {
        TypeParam::TypeVar(ast::TypeParamTypeVar { bound, default, .. }) => {
            bound
                .as_ref()
                .is_some_and(|value| any_over_expr(value, &mut *func))
                || default
                    .as_ref()
                    .is_some_and(|value| any_over_expr(value, &mut *func))
        }
        TypeParam::TypeVarTuple(ast::TypeParamTypeVarTuple { default, .. }) => default
            .as_ref()
            .is_some_and(|value| any_over_expr(value, &mut *func)),
        TypeParam::ParamSpec(ast::TypeParamParamSpec { default, .. }) => default
            .as_ref()
            .is_some_and(|value| any_over_expr(value, &mut *func)),
    }
}

fn any_over_pattern(pattern: &Pattern, func: &mut dyn FnMut(&Expr) -> bool) -> bool {
    match pattern {
        Pattern::MatchValue(ast::PatternMatchValue {
            value,
            range: _,
            node_index: _,
        }) => any_over_expr(value, func),
        Pattern::MatchSingleton(_) => false,
        Pattern::MatchSequence(ast::PatternMatchSequence {
            patterns,
            range: _,
            node_index: _,
        }) => patterns
            .iter()
            .any(|pattern| any_over_pattern(pattern, &mut *func)),
        Pattern::MatchMapping(ast::PatternMatchMapping { keys, patterns, .. }) => {
            keys.iter().any(|key| any_over_expr(key, &mut *func))
                || patterns
                    .iter()
                    .any(|pattern| any_over_pattern(pattern, &mut *func))
        }
        Pattern::MatchClass(ast::PatternMatchClass { cls, arguments, .. }) => {
            any_over_expr(cls, &mut *func)
                || arguments
                    .patterns
                    .iter()
                    .any(|pattern| any_over_pattern(pattern, &mut *func))
                || arguments
                    .keywords
                    .iter()
                    .any(|keyword| any_over_pattern(&keyword.pattern, &mut *func))
        }
        Pattern::MatchStar(_) => false,
        Pattern::MatchAs(ast::PatternMatchAs { pattern, .. }) => pattern
            .as_ref()
            .is_some_and(|pattern| any_over_pattern(pattern, func)),
        Pattern::MatchOr(ast::PatternMatchOr {
            patterns,
            range: _,
            node_index: _,
        }) => patterns
            .iter()
            .any(|pattern| any_over_pattern(pattern, &mut *func)),
    }
}

fn any_over_interpolated_string_element(
    element: &ast::InterpolatedStringElement,
    func: &mut dyn FnMut(&Expr) -> bool,
) -> bool {
    match element {
        ast::InterpolatedStringElement::Literal(_) => false,
        ast::InterpolatedStringElement::Interpolation(ast::InterpolatedElement {
            expression,
            format_spec,
            ..
        }) => {
            any_over_expr(expression, &mut *func)
                || format_spec.as_ref().is_some_and(|spec| {
                    spec.elements.iter().any(|spec_element| {
                        any_over_interpolated_string_element(spec_element, &mut *func)
                    })
                })
        }
    }
}

fn any_over_stmt<F>(stmt: &Stmt, mut func: F) -> bool
where
    F: FnMut(&Expr) -> bool,
{
    fn inner(stmt: &Stmt, func: &mut dyn FnMut(&Expr) -> bool) -> bool {
        match stmt {
            Stmt::FunctionDef(ast::StmtFunctionDef {
                parameters,
                type_params,
                body,
                decorator_list,
                returns,
                ..
            }) => {
                parameters.iter().any(|param| {
                    param
                        .default()
                        .is_some_and(|default| any_over_expr(default, &mut *func))
                        || param
                            .annotation()
                            .is_some_and(|annotation| any_over_expr(annotation, &mut *func))
                }) || type_params.as_ref().is_some_and(|type_params| {
                    type_params
                        .iter()
                        .any(|type_param| any_over_type_param(type_param, &mut *func))
                }) || body.iter().any(|stmt| any_over_stmt(stmt, &mut *func))
                    || decorator_list
                        .iter()
                        .any(|decorator| any_over_expr(&decorator.expression, &mut *func))
                    || returns
                        .as_ref()
                        .is_some_and(|value| any_over_expr(value, func))
            }
            Stmt::ClassDef(ast::StmtClassDef {
                arguments,
                type_params,
                body,
                decorator_list,
                ..
            }) => {
                // Note that e.g. `class A(*args, a=2, *args2, **kwargs): pass` is a valid class
                // definition
                arguments
                    .as_deref()
                    .is_some_and(|Arguments { args, keywords, .. }| {
                        args.iter().any(|expr| any_over_expr(expr, &mut *func))
                            || keywords
                                .iter()
                                .any(|keyword| any_over_expr(&keyword.value, &mut *func))
                    })
                    || type_params.as_ref().is_some_and(|type_params| {
                        type_params
                            .iter()
                            .any(|type_param| any_over_type_param(type_param, &mut *func))
                    })
                    || body.iter().any(|stmt| any_over_stmt(stmt, &mut *func))
                    || decorator_list
                        .iter()
                        .any(|decorator| any_over_expr(&decorator.expression, &mut *func))
            }
            Stmt::Return(ast::StmtReturn {
                value,
                range: _,
                node_index: _,
            }) => value
                .as_ref()
                .is_some_and(|value| any_over_expr(value, func)),
            Stmt::Delete(ast::StmtDelete {
                targets,
                range: _,
                node_index: _,
            }) => targets.iter().any(|expr| any_over_expr(expr, &mut *func)),
            Stmt::TypeAlias(ast::StmtTypeAlias {
                name,
                type_params,
                value,
                ..
            }) => {
                any_over_expr(name, &mut *func)
                    || type_params.as_ref().is_some_and(|type_params| {
                        type_params
                            .iter()
                            .any(|type_param| any_over_type_param(type_param, &mut *func))
                    })
                    || any_over_expr(value, func)
            }
            Stmt::Assign(ast::StmtAssign { targets, value, .. }) => {
                targets.iter().any(|expr| any_over_expr(expr, &mut *func))
                    || any_over_expr(value, func)
            }
            Stmt::AugAssign(ast::StmtAugAssign { target, value, .. }) => {
                any_over_expr(target, &mut *func) || any_over_expr(value, &mut *func)
            }
            Stmt::AnnAssign(ast::StmtAnnAssign {
                target,
                annotation,
                value,
                ..
            }) => {
                any_over_expr(target, &mut *func)
                    || any_over_expr(annotation, &mut *func)
                    || value
                        .as_ref()
                        .is_some_and(|value| any_over_expr(value, &mut *func))
            }
            Stmt::For(ast::StmtFor {
                target,
                iter,
                body,
                orelse,
                ..
            }) => {
                any_over_expr(target, &mut *func)
                    || any_over_expr(iter, &mut *func)
                    || any_over_body(body, &mut *func)
                    || any_over_body(orelse, &mut *func)
            }
            Stmt::While(ast::StmtWhile {
                test,
                body,
                orelse,
                range: _,
                node_index: _,
            }) => {
                any_over_expr(test, &mut *func)
                    || any_over_body(body, &mut *func)
                    || any_over_body(orelse, &mut *func)
            }
            Stmt::If(ast::StmtIf {
                pattern,
                test,
                body,
                elif_else_clauses,
                range: _,
                node_index: _,
            }) => {
                pattern
                    .as_deref()
                    .is_some_and(|pattern| any_over_pattern(pattern, &mut *func))
                    || any_over_expr(test, &mut *func)
                    || any_over_body(body, &mut *func)
                    || elif_else_clauses.iter().any(|clause| {
                        clause
                            .pattern
                            .as_deref()
                            .is_some_and(|pattern| any_over_pattern(pattern, &mut *func))
                            || clause
                                .test
                                .as_ref()
                                .is_some_and(|test| any_over_expr(test, &mut *func))
                            || any_over_body(&clause.body, &mut *func)
                    })
            }
            Stmt::With(ast::StmtWith { items, body, .. }) => {
                items.iter().any(|with_item| {
                    any_over_expr(&with_item.context_expr, &mut *func)
                        || with_item
                            .optional_vars
                            .as_ref()
                            .is_some_and(|expr| any_over_expr(expr, &mut *func))
                }) || any_over_body(body, &mut *func)
            }
            Stmt::Raise(ast::StmtRaise {
                exc,
                cause,
                range: _,
                node_index: _,
            }) => {
                exc.as_ref()
                    .is_some_and(|value| any_over_expr(value, &mut *func))
                    || cause
                        .as_ref()
                        .is_some_and(|value| any_over_expr(value, &mut *func))
            }
            Stmt::Try(ast::StmtTry {
                body,
                handlers,
                orelse,
                finalbody,
                is_star: _,
                range: _,
                node_index: _,
            }) => {
                any_over_body(body, &mut *func)
                    || handlers.iter().any(|handler| {
                        let ExceptHandler::ExceptHandler(ast::ExceptHandlerExceptHandler {
                            type_,
                            body,
                            ..
                        }) = handler;
                        type_
                            .as_ref()
                            .is_some_and(|expr| any_over_expr(expr, &mut *func))
                            || any_over_body(body, &mut *func)
                    })
                    || any_over_body(orelse, &mut *func)
                    || any_over_body(finalbody, &mut *func)
            }
            Stmt::Assert(ast::StmtAssert {
                test,
                msg,
                range: _,
                node_index: _,
            }) => {
                any_over_expr(test, &mut *func)
                    || msg
                        .as_ref()
                        .is_some_and(|value| any_over_expr(value, &mut *func))
            }
            Stmt::Match(ast::StmtMatch {
                subject,
                cases,
                range: _,
                node_index: _,
            }) => {
                any_over_expr(subject, &mut *func)
                    || cases.iter().any(|case| {
                        let MatchCase {
                            pattern,
                            guard,
                            body,
                            range: _,
                            node_index: _,
                        } = case;
                        any_over_pattern(pattern, &mut *func)
                            || guard
                                .as_ref()
                                .is_some_and(|expr| any_over_expr(expr, &mut *func))
                            || any_over_body(body, &mut *func)
                    })
            }
            Stmt::Import(_) => false,
            Stmt::ImportFrom(_) => false,
            Stmt::Global(_) => false,
            Stmt::Nonlocal(_) => false,
            Stmt::Expr(ast::StmtExpr {
                value,
                range: _,
                node_index: _,
            }) => any_over_expr(value, func),
            Stmt::Break(ast::StmtBreak { value, .. }) => value
                .as_ref()
                .is_some_and(|value| any_over_expr(value, func)),
            Stmt::Pass(_) | Stmt::Continue(_) => false,
            Stmt::IpyEscapeCommand(_) => false,
        }
    }

    inner(stmt, &mut func)
}

/// basedpython: one place that produces the value of a statement used as a
/// [statement expression](crate::ExprStatement).
#[derive(Debug, Clone, Copy)]
pub enum StatementExpressionValue<'a> {
    /// The tail expression of a branch, reached by falling off its end.
    Tail(&'a Expr),
    /// A `break <value>` targeting the statement's own loop, with the operand it
    /// carries. Both are needed: the operand is the value, and the statement is
    /// what a lowering has to rewrite.
    Break(&'a crate::StmtBreak, &'a Expr),
}

impl<'a> StatementExpressionValue<'a> {
    /// The expression whose type the statement expression takes on.
    pub fn expr(self) -> &'a Expr {
        match self {
            StatementExpressionValue::Tail(expr) | StatementExpressionValue::Break(_, expr) => expr,
        }
    }
}

/// basedpython: collects, in source order, the places that produce the value of
/// `stmt` when it is used as a [statement expression](crate::ExprStatement).
///
/// These are the tail expression of each branch and, for the loop forms, each
/// `break <value>` targeting that loop. A branch that cannot complete — it
/// raises, returns, or breaks out — contributes nothing, which is correct: it
/// never produces a value.
///
/// The value is *missing* exactly when some path through `stmt` reaches its end
/// without passing one of these, which is what makes a statement expression
/// non-exhaustive.
pub fn statement_expression_values(stmt: &Stmt) -> Vec<StatementExpressionValue<'_>> {
    let mut values = Vec::new();
    collect_statement_expression_values(stmt, &mut values);
    values
}

fn collect_statement_expression_values<'a>(
    stmt: &'a Stmt,
    values: &mut Vec<StatementExpressionValue<'a>>,
) {
    match stmt {
        Stmt::If(ast::StmtIf {
            body,
            elif_else_clauses,
            ..
        }) => {
            collect_tail_value(body, values);
            for clause in elif_else_clauses {
                collect_tail_value(&clause.body, values);
            }
        }
        Stmt::Match(ast::StmtMatch { cases, .. }) => {
            for case in cases {
                collect_tail_value(&case.body, values);
            }
        }
        Stmt::For(ast::StmtFor { body, orelse, .. })
        | Stmt::While(ast::StmtWhile { body, orelse, .. }) => {
            collect_break_values(body, values);
            collect_tail_value(orelse, values);
        }
        // `raise` and `return` never complete
        _ => {}
    }
}

/// Collects the value produced by falling off the end of `body`.
fn collect_tail_value<'a>(body: &'a [Stmt], values: &mut Vec<StatementExpressionValue<'a>>) {
    match body.last() {
        Some(Stmt::Expr(ast::StmtExpr { value, .. })) => {
            values.push(StatementExpressionValue::Tail(value));
        }
        // a trailing compound statement is itself the branch's value, so its own
        // branches are recursed into. this is what lets a nested `match` in tail
        // position be written without repeating the assignment
        Some(stmt) => collect_statement_expression_values(stmt, values),
        None => {}
    }
}

/// Collects every `break <value>` in `body` that targets the loop `body` belongs
/// to.
///
/// Nested loops are skipped: a `break` inside one targets that loop, not ours.
/// Function and class bodies are skipped for the same reason — a `break` cannot
/// cross them.
fn collect_break_values<'a>(body: &'a [Stmt], values: &mut Vec<StatementExpressionValue<'a>>) {
    for stmt in body {
        match stmt {
            Stmt::Break(break_stmt) => {
                if let Some(value) = &break_stmt.value {
                    values.push(StatementExpressionValue::Break(break_stmt, value));
                }
            }
            Stmt::If(ast::StmtIf {
                body,
                elif_else_clauses,
                ..
            }) => {
                collect_break_values(body, values);
                for clause in elif_else_clauses {
                    collect_break_values(&clause.body, values);
                }
            }
            Stmt::Match(ast::StmtMatch { cases, .. }) => {
                for case in cases {
                    collect_break_values(&case.body, values);
                }
            }
            Stmt::With(ast::StmtWith { body, .. }) => collect_break_values(body, values),
            Stmt::Try(ast::StmtTry {
                body,
                handlers,
                orelse,
                finalbody,
                ..
            }) => {
                collect_break_values(body, values);
                for handler in handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_break_values(&handler.body, values);
                }
                collect_break_values(orelse, values);
                collect_break_values(finalbody, values);
            }
            _ => {}
        }
    }
}

pub fn any_over_body<F>(body: &[Stmt], mut func: F) -> bool
where
    F: FnMut(&Expr) -> bool,
{
    body.iter().any(|stmt| any_over_stmt(stmt, &mut func))
}

pub fn is_dunder(id: &str) -> bool {
    id.starts_with("__") && id.ends_with("__")
}

/// Whether a name starts and ends with a single underscore.
///
/// `_a__` is considered neither a dunder nor a sunder name.
pub fn is_sunder(id: &str) -> bool {
    id.starts_with('_') && id.ends_with('_') && !id.starts_with("__") && !id.ends_with("__")
}

/// Return `true` if the [`Stmt`] is an assignment to a dunder (like `__all__`).
pub fn is_assignment_to_a_dunder(stmt: &Stmt) -> bool {
    // Check whether it's an assignment to a dunder, with or without a type
    // annotation. This is what pycodestyle (as of 2.9.1) does.
    match stmt {
        Stmt::Assign(ast::StmtAssign { targets, .. }) => {
            if let [Expr::Name(ast::ExprName { id, .. })] = targets.as_slice() {
                is_dunder(id)
            } else {
                false
            }
        }
        Stmt::AnnAssign(ast::StmtAnnAssign { target, .. }) => {
            if let Expr::Name(ast::ExprName { id, .. }) = target.as_ref() {
                is_dunder(id)
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Return `true` if the [`Expr`] is a singleton (`None`, `True`, `False`, or
/// `...`).
pub const fn is_singleton(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::NoneLiteral(_) | Expr::BooleanLiteral(_) | Expr::EllipsisLiteral(_)
    )
}

/// Return `true` if the [`Expr`] is a literal or tuple of literals.
pub fn is_constant(expr: &Expr) -> bool {
    if let Expr::Tuple(tuple) = expr {
        tuple.iter().all(is_constant)
    } else {
        expr.is_literal_expr()
    }
}

/// Return `true` if the [`Expr`] is a non-singleton constant.
pub fn is_constant_non_singleton(expr: &Expr) -> bool {
    is_constant(expr) && !is_singleton(expr)
}

/// Return `true` if an [`Expr`] is a literal `True`.
pub const fn is_const_true(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::BooleanLiteral(ast::ExprBooleanLiteral { value: true, .. }),
    )
}

/// Return `true` if an [`Expr`] is a literal `False`.
pub const fn is_const_false(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::BooleanLiteral(ast::ExprBooleanLiteral { value: false, .. }),
    )
}

/// Return `true` if the [`Expr`] is a mutable iterable initializer, like `{}` or `[]`.
pub const fn is_mutable_iterable_initializer(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Set(_)
            | Expr::SetComp(_)
            | Expr::List(_)
            | Expr::ListComp(_)
            | Expr::Dict(_)
            | Expr::DictComp(_)
    )
}

/// Extract the names of all handled exceptions.
pub fn extract_handled_exceptions(handlers: &[ExceptHandler]) -> Vec<&Expr> {
    let mut handled_exceptions = Vec::new();
    for handler in handlers {
        match handler {
            ExceptHandler::ExceptHandler(ast::ExceptHandlerExceptHandler { type_, .. }) => {
                if let Some(type_) = type_ {
                    if let Expr::Tuple(tuple) = &**type_ {
                        for type_ in tuple {
                            handled_exceptions.push(type_);
                        }
                    } else {
                        handled_exceptions.push(type_);
                    }
                }
            }
        }
    }
    handled_exceptions
}

/// Given an [`Expr`] that can be callable or not (like a decorator, which could
/// be used with or without explicit call syntax), return the underlying
/// callable.
pub fn map_callable(decorator: &Expr) -> &Expr {
    if let Expr::Call(ast::ExprCall { func, .. }) = decorator {
        // Ex) `@decorator()`
        func
    } else {
        // Ex) `@decorator`
        decorator
    }
}

/// Given an [`Expr`] that can be a [`ExprSubscript`][ast::ExprSubscript] or not
/// (like an annotation that may be generic or not), return the underlying expr.
pub fn map_subscript(expr: &Expr) -> &Expr {
    if let Expr::Subscript(ast::ExprSubscript { value, .. }) = expr {
        // Ex) `Iterable[T]`  => return `Iterable`
        value
    } else {
        // Ex) `Iterable`  => return `Iterable`
        expr
    }
}

/// basedpython: `local` / `once` lifetime modifiers detected on a parameter.
///
/// The parser consumes the keywords without an AST field (mirroring the `let`
/// init prefix), so they are recovered here from the source span between the
/// parameter's start and its name. `strip_ranges` is empty — and does not
/// allocate — for the common unmodified parameter.
#[derive(Debug, Default, Clone)]
pub struct ParameterModifiers {
    /// `local x` — a non-escaping (borrowed) parameter that must not outlive
    /// the call it is bound in
    pub local: bool,
    /// `once fn` — a callback that must be called exactly once
    pub once: bool,
    /// source range of each `local` / `once` keyword together with the
    /// whitespace up to the next token, for deletion when lowering to python
    pub strip_ranges: Vec<TextRange>,
}

impl ParameterModifiers {
    /// whether any lifetime modifier is present
    pub fn any(&self) -> bool {
        self.local || self.once
    }
}

/// basedpython: how a narrowing return annotation applies at a call site.
#[derive(Debug, Clone, Copy)]
pub enum ReturnGuardForm<'a> {
    /// `-> place is T`: narrows where the call evaluates truthy.
    Predicate { ty: &'a Expr },
    /// `-> asserts name` (`is_positive`) or `-> asserts not name`: narrows the place by
    /// truthiness once the call returns.
    Asserts { is_positive: bool },
    /// `-> asserts name is T` (`is_positive`) or `-> asserts name is not T`: narrows the
    /// place by `ty` once the call returns.
    AssertsType { is_positive: bool, ty: &'a Expr },
}

/// basedpython: one narrowing target named by a return annotation.
#[derive(Debug, Clone, Copy)]
pub struct ReturnGuard<'a> {
    /// The place the guard narrows: a name, or an attribute chain rooted at one.
    pub place: &'a Expr,
    pub form: ReturnGuardForm<'a>,
}

impl<'a> ReturnGuard<'a> {
    /// The root name of the narrowed place, and the attribute segments below it.
    ///
    /// `self.data` is the name `self` and the segment `data`.
    pub fn place_parts(&self) -> (&'a Name, Vec<&'a Name>) {
        let mut segments = Vec::new();
        let mut expr = self.place;
        while let Expr::Attribute(attribute) = expr {
            segments.push(&attribute.attr.id);
            expr = &attribute.value;
        }
        segments.reverse();
        let name = expr
            .as_name_expr()
            .map(|name| &name.id)
            .expect("a guard place is rooted at a name");
        (name, segments)
    }
}

/// Whether `expr` is a place a narrowing return annotation can name.
fn is_guard_place(expr: &Expr) -> bool {
    match expr {
        Expr::Name(_) => true,
        Expr::Attribute(attribute) => is_guard_place(&attribute.value),
        _ => false,
    }
}

/// basedpython: what a function's return annotation names as its narrowing targets.
///
/// `def f(x) -> x is int` and `def f(x) -> asserts x` both name the place a call narrows;
/// `-> asserts a is int and b` names several. This is the surface syntax alone: whether the
/// annotation really produced a narrowing type is up to the type checker.
///
/// Returns `None` when the annotation names no place at all — for an `asserts` annotation
/// that is an error, and for any other annotation it is just an ordinary type.
pub fn return_guards(function: &ast::StmtFunctionDef) -> Option<Vec<ReturnGuard<'_>>> {
    let returns = function.returns.as_deref()?;

    if function.is_asserts_return {
        // `asserts a is int and b` asserts every term of the conjunction
        let terms: Vec<&Expr> = match returns {
            Expr::BoolOp(ast::ExprBoolOp {
                op: ast::BoolOp::And,
                values,
                ..
            }) => values.iter().collect(),
            term => vec![term],
        };
        return terms.into_iter().map(asserted_guard).collect();
    }

    let Expr::Compare(compare) = returns else {
        return None;
    };
    if !matches!(&*compare.ops, [ast::CmpOp::Is]) || !is_guard_place(&compare.left) {
        return None;
    }
    let [ty] = &*compare.comparators else {
        return None;
    };
    Some(vec![ReturnGuard {
        place: &compare.left,
        form: ReturnGuardForm::Predicate { ty },
    }])
}

/// One term of an `asserts` annotation: a place, a negated place, or a place tested with `is`.
fn asserted_guard(term: &Expr) -> Option<ReturnGuard<'_>> {
    if let Expr::Compare(compare) = term
        && let [op @ (ast::CmpOp::Is | ast::CmpOp::IsNot)] = &*compare.ops
        && let [ty] = &*compare.comparators
        && is_guard_place(&compare.left)
    {
        return Some(ReturnGuard {
            place: &compare.left,
            form: ReturnGuardForm::AssertsType {
                is_positive: matches!(op, ast::CmpOp::Is),
                ty,
            },
        });
    }

    let (place, is_positive) = match term {
        Expr::UnaryOp(ast::ExprUnaryOp {
            op: ast::UnaryOp::Not,
            operand,
            ..
        }) => (operand.as_ref(), false),
        place => (place, true),
    };
    is_guard_place(place).then_some(ReturnGuard {
        place,
        form: ReturnGuardForm::Asserts { is_positive },
    })
}

/// basedpython: the source spans of a function's `raises` clause.
///
/// The AST holds only the declared type expression, whose range excludes both
/// the keyword and any parentheses wrapping it. Recovering those from the source
/// is what lets the lowering delete the whole clause and the IDE highlight
/// exactly the keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RaisesClauseSpans {
    /// the `raises` keyword alone
    pub keyword: TextRange,
    /// the keyword through the end of the declared type, parentheses included
    pub clause: TextRange,
}

/// basedpython: locate `function`'s `raises` clause in `source`.
///
/// Returns `None` when there is no clause, or when the source does not have the
/// expected shape — a caller that cannot find the clause must leave the source
/// alone rather than guess at a range.
pub fn raises_clause_spans(
    source: &str,
    function: &ast::StmtFunctionDef,
) -> Option<RaisesClauseSpans> {
    let raises = function.raises.as_deref()?;

    // the clause follows the return annotation, or the parameter list when there
    // is none. tokenizing the gap finds the keyword without being fooled by the
    // word appearing in a comment or in the annotation next to it
    let signature_end = function
        .returns
        .as_deref()
        .map_or_else(|| function.parameters.end(), Ranged::end);
    let gap = TextRange::new(signature_end, raises.start());

    let mut tokens = SimpleTokenizer::new(source, gap).skip_trivia();
    let keyword = tokens.next()?;
    if keyword.kind != SimpleTokenKind::Name || &source[keyword.range] != "raises" {
        return None;
    }

    // parentheses around the declared type are not part of its range — except
    // for a parenthesized tuple, which owns them. counting what the gap opens
    // and closing exactly that many handles both without special-casing either
    let depth = tokens
        .filter(|token| token.kind == SimpleTokenKind::LParen)
        .count();

    let mut end = raises.end();
    let mut closing =
        SimpleTokenizer::new(source, TextRange::new(end, function.end())).skip_trivia();
    for _ in 0..depth {
        let token = closing.next()?;
        if token.kind != SimpleTokenKind::RParen {
            return None;
        }
        end = token.range.end();
    }

    Some(RaisesClauseSpans {
        keyword: keyword.range,
        clause: TextRange::new(keyword.range.start(), end),
    })
}

/// basedpython: the range of the `let` keyword introducing a pattern-matching
/// `if let <pattern> := <subject>:` clause, read from `source`.
///
/// The AST holds only the pattern, so the keyword is recovered by tokenizing the
/// gap between the clause keyword (`if` / `elif`, at `clause_start`) and the
/// pattern. Returns `None` when the source does not have that shape.
pub fn if_let_keyword_range(
    source: &str,
    clause_start: TextSize,
    pattern: &Pattern,
) -> Option<TextRange> {
    if clause_start >= pattern.start() {
        return None;
    }
    SimpleTokenizer::new(source, TextRange::new(clause_start, pattern.start()))
        .skip_trivia()
        .find(|token| token.kind == SimpleTokenKind::Name && &source[token.range] == "let")
        .map(|token| token.range)
}

/// basedpython: detect `local` / `once` modifiers on `param`, read from `source`
/// (the file the parameter was parsed from).
///
/// The prefix between `param.range().start()` and the parameter name holds a run
/// of `let` / `local` / `once` keywords. This records the `local` / `once` ones
/// (a `let` is left in place for the init-method transform) along with the
/// ranges to strip when lowering.
pub fn parameter_modifiers(source: &str, param: &ast::Parameter) -> ParameterModifiers {
    let mut mods = ParameterModifiers::default();
    let prefix_start = param.range().start();
    let name_start = param.name.range().start();
    if prefix_start >= name_start {
        return mods;
    }
    let base = usize::from(prefix_start);
    // a parameter parsed from a string annotation's sub-AST is ranged against the
    // string's contents rather than `source`
    let Some(prefix) = source.get(base..usize::from(name_start)) else {
        return mods;
    };

    // walk the whitespace-separated keyword run left to right
    let mut rel = 0usize;
    while rel < prefix.len() {
        let after_ws = rel + leading_whitespace_len(&prefix[rel..]);
        if after_ws >= prefix.len() {
            break;
        }
        let word_end = prefix[after_ws..]
            .find(char::is_whitespace)
            .map_or(prefix.len(), |p| after_ws + p);
        let is_modifier = match &prefix[after_ws..word_end] {
            "local" => {
                mods.local = true;
                true
            }
            "once" => {
                mods.once = true;
                true
            }
            _ => false,
        };
        if is_modifier {
            // strip the keyword plus the whitespace up to the next token
            let del_end = word_end + leading_whitespace_len(&prefix[word_end..]);
            mods.strip_ranges.push(TextRange::new(
                TextSize::try_from(base + after_ws).expect("offset fits u32"),
                TextSize::try_from(base + del_end).expect("offset fits u32"),
            ));
        }
        rel = word_end;
    }
    mods
}

/// byte length of the leading run of whitespace in `s`
fn leading_whitespace_len(s: &str) -> usize {
    s.len() - s.trim_start().len()
}

/// whether `text` reads as one word — an identifier or a keyword, as opposed to
/// punctuation or a literal that merely starts with a letter (`f"…"`)
fn is_word(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_')
        && characters.all(|character| character.is_alphanumeric() || character == '_')
}

/// basedpython: the ranges of `keywords` the parser consumed without leaving an
/// AST node behind, recovered from the source `range` they were written in.
///
/// The `local` / `once` of a callable-type parameter and the `asserts` of a
/// narrowing return annotation are all dropped once parsed, so the only record
/// left is the gap ahead of the node they modify. Every name token in such a gap
/// is one of them, since nothing else there can be an identifier.
///
/// Yields nothing when `range` does not index into `source`, as for the sub-AST
/// of a string annotation, whose ranges belong to the string's own contents.
pub fn consumed_keywords<'src>(
    source: &'src str,
    range: TextRange,
    keywords: &'src [&'src str],
) -> impl Iterator<Item = TextRange> + 'src {
    word_token_ranges(source, range).filter(move |word| keywords.contains(&&source[*word]))
}

/// The ranges of the word tokens written in `range` of `source` — identifiers and
/// python's own keywords alike, since a basedpython keyword may be either
/// (`await` is a keyword to the tokenizer, `once` an identifier).
///
/// Yields nothing when `range` does not index into `source`, as for the sub-AST
/// of a string annotation, whose ranges belong to the string's own contents.
pub fn word_token_ranges(source: &str, range: TextRange) -> impl Iterator<Item = TextRange> + '_ {
    let tokenizer = (range.start() <= range.end() && usize::from(range.end()) <= source.len())
        .then(|| SimpleTokenizer::new(source, range).skip_trivia());
    tokenizer
        .into_iter()
        .flatten()
        .filter(|token| token.kind == SimpleTokenKind::Name || is_word(&source[token.range]))
        .map(|token| token.range)
}

/// basedpython: the parameter a trailing-lambda block binds to — the last
/// declared parameter in signature order (keyword-only included), skipping a
/// trailing `*args` / `**kwargs` (a block is never bound to a variadic).
///
/// This mirrors the signature-level "last parameter" that the `it` typing and
/// keyword binding use (`parameters().iter().next_back()`), so `local` / `once`
/// modifier detection agrees with where the block actually binds. Reading only
/// `args.last()` would miss a keyword-only callback (`def f(*, once cb)`).
pub fn last_bound_parameter(parameters: &ast::Parameters) -> Option<&ast::Parameter> {
    if parameters.kwarg.is_some() {
        return None;
    }
    if let Some(last) = parameters.kwonlyargs.last() {
        return Some(&last.parameter);
    }
    if parameters.vararg.is_some() {
        return None;
    }
    if let Some(last) = parameters.args.last() {
        return Some(&last.parameter);
    }
    parameters.posonlyargs.last().map(|param| &param.parameter)
}

/// basedpython: returns `true` if `expr` is a single parser-synthesized
/// `[*]` marker — `Starred(Name(id="", ctx=Invalid))`. An empty-id `Name`
/// with `Invalid` context is unique to this synthesis and cannot appear from
/// any normal parse, so detecting it is unambiguous.
pub fn is_top_star_marker(expr: &Expr) -> bool {
    let Expr::Starred(starred) = expr else {
        return false;
    };
    let Expr::Name(name) = starred.value.as_ref() else {
        return false;
    };
    name.id.is_empty() && matches!(name.ctx, ExprContext::Invalid)
}

/// basedpython: returns `true` if `expr` is the *top parameters* form `(*: *, **: *)` — an
/// anonymous variadic and an anonymous keyword-variadic, both admitting anything.
///
/// Every parameter list is a subtype of this one, so a type variable bound by it ranges over all
/// parameter lists: it is the surface spelling of a `ParamSpec`.
///
/// The encodings are those the parameter-shaped tuple parser produces: `*: T` is `Starred(T)`,
/// `**: T` is `Starred(Starred(T))`, and the `*` type is a
/// [top-star marker](is_top_star_marker).
pub fn is_top_parameters_form(expr: &Expr) -> bool {
    fn is_top_star_annotated(expr: &Expr, stars: usize) -> bool {
        let mut current = expr;
        for _ in 0..stars {
            let Expr::Starred(starred) = current else {
                return false;
            };
            current = starred.value.as_ref();
        }
        is_top_star_marker(current)
    }

    let Expr::Tuple(tuple) = expr else {
        return false;
    };
    tuple.parenthesized
        && tuple.has_parameter_shape()
        && matches!(
            tuple.elts.as_slice(),
            [variadic, keyword_variadic]
                if is_top_star_annotated(variadic, 1)
                    && is_top_star_annotated(keyword_variadic, 2)
        )
}

/// basedpython: returns `true` if `expr` is the parser-synthesized marker for a
/// declaration keyword on an *unannotated* assignment — `var a = 1`,
/// `override a = 1`, `final override a = 1`. Those parse as an `AnnAssign` whose
/// annotation is `Name(id="__modifier_assign__", ctx=Invalid)` spanning the
/// keyword prefix. The marker carries no type, so the statement declares nothing
/// and binds exactly like the `a = 1` it lowers to.
pub fn is_untyped_declaration_marker(expr: &Expr) -> bool {
    matches!(expr, Expr::Name(name) if name.id.as_str() == "__modifier_assign__")
}

/// basedpython: the inference-context marker an *unannotated* `field = <init>` in a
/// property accessor block carries — `__field__[T]`, where `T` is the property's
/// declared type. Returns `T`.
///
/// The field has no declared type of its own: `T` is only the context its
/// initialiser is inferred in, and the inferred type becomes the declaration. That
/// is what lets a bare `field = []` under a `Sequence[int]` property declare
/// `list[int]` while still keeping storage typed independently of the property.
pub fn untyped_declaration_context(expr: &Expr) -> Option<&Expr> {
    let Expr::Subscript(subscript) = expr else {
        return None;
    };
    matches!(subscript.value.as_ref(), Expr::Name(name) if name.id.as_str() == "__field__")
        .then(|| subscript.slice.as_ref())
}

/// basedpython: use-site variance marker, one of `out X`, `in X`, `in out X`
/// in a subscript element. Encoded by the parser as
/// `Subscript(Name(id, ctx=Invalid), inner)` where `id` is one of
/// `__variance_out__`, `__variance_in__`, `__variance_inout__`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "get-size", derive(get_size2::GetSize))]
pub enum UseSiteVariance {
    /// `out X` — covariant: read-only view of `X`
    Out,
    /// `in X` — contravariant: write-only view of `X`
    In,
    /// `in out X` — invariant: read-write view of `X` (same as no variance)
    InOut,
}

impl UseSiteVariance {
    /// the synthetic name id the parser uses for this variance marker
    pub fn marker_id(self) -> &'static str {
        match self {
            Self::Out => "__variance_out__",
            Self::In => "__variance_in__",
            Self::InOut => "__variance_inout__",
        }
    }

    /// parse from a marker name id; returns None if not a variance marker
    pub fn from_marker_id(id: &str) -> Option<Self> {
        match id {
            "__variance_out__" => Some(Self::Out),
            "__variance_in__" => Some(Self::In),
            "__variance_inout__" => Some(Self::InOut),
            _ => None,
        }
    }
}

/// basedpython: if `expr` is a parser-synthesized use-site variance marker —
/// `Subscript(Name(id=<marker>, ctx=Invalid), inner)` — return its variance
/// and inner expression. An invalid-context Name with one of the marker ids
/// is unique to parser synthesis and cannot appear from any normal parse.
pub fn use_site_variance_marker(expr: &Expr) -> Option<(UseSiteVariance, &Expr)> {
    let Expr::Subscript(sub) = expr else {
        return None;
    };
    let Expr::Name(name) = sub.value.as_ref() else {
        return None;
    };
    if !matches!(name.ctx, ExprContext::Invalid) {
        return None;
    }
    let variance = UseSiteVariance::from_marker_id(name.id.as_str())?;
    Some((variance, sub.slice.as_ref()))
}

/// basedpython: when `slice` contains at least one parser-synthesized
/// top-star marker (`Starred(Name(id="", ctx=Invalid))`), returns the slice
/// elements as a flat slice. The single-marker form is encoded as one
/// `Starred(Name(""))`; multi-element forms (pure or mixed) are an
/// unparenthesized tuple. Mixed slices like `dict[str, *]` are supported —
/// non-marker elements are returned as-is and consumers replace markers
/// with `Any` while keeping concrete elements
pub fn top_star_slice_elements(slice: &Expr) -> Option<&[Expr]> {
    if is_top_star_marker(slice) {
        return Some(std::slice::from_ref(slice));
    }
    let Expr::Tuple(tuple) = slice else {
        return None;
    };
    if tuple.parenthesized || tuple.elts.is_empty() {
        return None;
    }
    if tuple.elts.iter().any(is_top_star_marker) {
        Some(&tuple.elts)
    } else {
        None
    }
}

/// basedpython: collects ranges of every top-star marker reachable from
/// `slice` without descending into nested `Subscript` expressions. The
/// nested-subscript boundary preserves locality — a marker inside
/// `dict[int, *]` belongs to that inner subscript, not the outer one in
/// `list[dict[int, *]]`. Markers nested inside union/intersection binops or
/// tuples (`list[int | *]`, `list[int, *]`) are picked up
pub fn top_star_marker_ranges_in_slice(slice: &Expr) -> Vec<TextRange> {
    let mut out = Vec::new();
    collect_top_star_markers(slice, &mut out);
    out
}

fn collect_top_star_markers(expr: &Expr, out: &mut Vec<TextRange>) {
    if is_top_star_marker(expr) {
        out.push(expr.range());
        return;
    }
    match expr {
        Expr::Subscript(_) => {}
        Expr::BinOp(ast::ExprBinOp { left, right, .. }) => {
            collect_top_star_markers(left, out);
            collect_top_star_markers(right, out);
        }
        Expr::Tuple(ast::ExprTuple { elts, .. }) => {
            for elt in elts {
                collect_top_star_markers(elt, out);
            }
        }
        Expr::UnaryOp(ast::ExprUnaryOp { operand, .. }) => {
            collect_top_star_markers(operand, out);
        }
        Expr::BoolOp(ast::ExprBoolOp { values, .. }) => {
            for v in values {
                collect_top_star_markers(v, out);
            }
        }
        _ => {}
    }
}

/// Given an [`Expr`] that can be starred, return the underlying starred expression.
pub fn map_starred(expr: &Expr) -> &Expr {
    if let Expr::Starred(ast::ExprStarred { value, .. }) = expr {
        // Ex) `*args`
        value
    } else {
        // Ex) `args`
        expr
    }
}

/// Return `true` if the body uses `locals()`, `globals()`, `vars()`, `eval()`.
///
/// Accepts a closure that determines whether a given name (e.g., `"list"`) is a Python builtin.
pub fn uses_magic_variable_access<F>(body: &[Stmt], is_builtin: F) -> bool
where
    F: Fn(&str) -> bool,
{
    any_over_body(body, |expr| {
        if let Expr::Call(ast::ExprCall { func, .. }) = expr {
            if let Expr::Name(ast::ExprName { id, .. }) = func.as_ref() {
                if matches!(id.as_str(), "locals" | "globals" | "vars" | "exec" | "eval") {
                    if is_builtin(id.as_str()) {
                        return true;
                    }
                }
            }
        }
        false
    })
}

/// Format the module reference name for a relative import.
///
/// # Examples
///
/// ```rust
/// # use ruff_python_ast::helpers::format_import_from;
///
/// assert_eq!(format_import_from(0, None), "".to_string());
/// assert_eq!(format_import_from(1, None), ".".to_string());
/// assert_eq!(format_import_from(1, Some("foo")), ".foo".to_string());
/// ```
pub fn format_import_from(level: u32, module: Option<&str>) -> Cow<'_, str> {
    match (level, module) {
        (0, Some(module)) => Cow::Borrowed(module),
        (level, module) => {
            let mut module_name =
                String::with_capacity((level as usize) + module.map_or(0, str::len));
            for _ in 0..level {
                module_name.push('.');
            }
            if let Some(module) = module {
                module_name.push_str(module);
            }
            Cow::Owned(module_name)
        }
    }
}

/// Format the member reference name for a relative import.
///
/// # Examples
///
/// ```rust
/// # use ruff_python_ast::helpers::format_import_from_member;
///
/// assert_eq!(format_import_from_member(0, None, "bar"), "bar".to_string());
/// assert_eq!(format_import_from_member(1, None, "bar"), ".bar".to_string());
/// assert_eq!(format_import_from_member(1, Some("foo"), "bar"), ".foo.bar".to_string());
/// ```
pub fn format_import_from_member(level: u32, module: Option<&str>, member: &str) -> String {
    let mut qualified_name =
        String::with_capacity((level as usize) + module.map_or(0, str::len) + 1 + member.len());
    if level > 0 {
        for _ in 0..level {
            qualified_name.push('.');
        }
    }
    if let Some(module) = module {
        qualified_name.push_str(module);
        qualified_name.push('.');
    }
    qualified_name.push_str(member);
    qualified_name
}

/// Create a module path from a (package, path) pair.
///
/// For example, if the package is `foo/bar` and the path is `foo/bar/baz.py`,
/// the call path is `["baz"]`.
pub fn to_module_path(package: &Path, path: &Path) -> Option<Vec<String>> {
    path.strip_prefix(package.parent()?)
        .ok()?
        .iter()
        .map(Path::new)
        .map(Path::file_stem)
        .map(|path| path.and_then(|path| path.to_os_string().into_string().ok()))
        .collect::<Option<Vec<String>>>()
}

/// Format the call path for a relative import.
///
/// # Examples
///
/// ```rust
/// # use ruff_python_ast::helpers::collect_import_from_member;
///
/// assert_eq!(collect_import_from_member(0, None, "bar").segments(), ["bar"]);
/// assert_eq!(collect_import_from_member(1, None, "bar").segments(), [".", "bar"]);
/// assert_eq!(collect_import_from_member(1, Some("foo"), "bar").segments(), [".", "foo", "bar"]);
/// ```
pub fn collect_import_from_member<'a>(
    level: u32,
    module: Option<&'a str>,
    member: &'a str,
) -> QualifiedName<'a> {
    let mut qualified_name_builder = QualifiedNameBuilder::with_capacity(
        level as usize
            + module
                .map(|module| module.split('.').count())
                .unwrap_or_default()
            + 1,
    );

    // Include the dots as standalone segments.
    if level > 0 {
        for _ in 0..level {
            qualified_name_builder.push(".");
        }
    }

    // Add the remaining segments.
    if let Some(module) = module {
        qualified_name_builder.extend(module.split('.'));
    }

    // Add the member.
    qualified_name_builder.push(member);

    qualified_name_builder.build()
}

/// Format the call path for a relative import, or `None` if the relative import extends beyond
/// the root module.
pub fn from_relative_import<'a>(
    // The path from which the import is relative.
    module: &'a [String],
    // The path of the import itself (e.g., given `from ..foo import bar`, `[".", ".", "foo", "bar]`).
    import: &[&'a str],
    // The remaining segments to the call path (e.g., given `bar.baz`, `["baz"]`).
    tail: &[&'a str],
) -> Option<QualifiedName<'a>> {
    let mut qualified_name_builder =
        QualifiedNameBuilder::with_capacity(module.len() + import.len() + tail.len());

    // Start with the module path.
    qualified_name_builder.extend(module.iter().map(String::as_str));

    // Remove segments based on the number of dots.
    for segment in import {
        if *segment == "." {
            if qualified_name_builder.is_empty() {
                return None;
            }
            qualified_name_builder.pop();
        } else {
            qualified_name_builder.push(segment);
        }
    }

    // Add the remaining segments.
    qualified_name_builder.extend_from_slice(tail);

    Some(qualified_name_builder.build())
}

/// Given an imported module (based on its relative import level and module name), return the
/// fully-qualified module path.
pub fn resolve_imported_module_path<'a>(
    level: u32,
    module: Option<&'a str>,
    module_path: Option<&[String]>,
) -> Option<Cow<'a, str>> {
    if level == 0 {
        return Some(Cow::Borrowed(module.unwrap_or("")));
    }

    let module_path = module_path?;

    if level as usize >= module_path.len() {
        return None;
    }

    let mut qualified_path = module_path[..module_path.len() - level as usize].join(".");
    if let Some(module) = module {
        if !qualified_path.is_empty() {
            qualified_path.push('.');
        }
        qualified_path.push_str(module);
    }
    Some(Cow::Owned(qualified_path))
}

/// A [`Visitor`] to collect all [`Expr::Name`] nodes in an AST.
#[derive(Debug, Default)]
pub struct NameFinder<'a> {
    /// A map from identifier to defining expression.
    pub names: FxHashMap<&'a str, &'a ast::ExprName>,
}

impl<'a> Visitor<'a> for NameFinder<'a> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Name(name) = expr {
            self.names.insert(&name.id, name);
        }
        crate::visitor::walk_expr(self, expr);
    }
}

/// A [`Visitor`] to collect all stored [`Expr::Name`] nodes in an AST.
#[derive(Debug, Default)]
pub struct StoredNameFinder<'a> {
    /// A map from identifier to defining expression.
    pub names: FxHashMap<&'a str, &'a ast::ExprName>,
}

impl<'a> Visitor<'a> for StoredNameFinder<'a> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Name(name) = expr {
            if name.ctx.is_store() {
                self.names.insert(&name.id, name);
            }
        }
        crate::visitor::walk_expr(self, expr);
    }
}

/// A [`Visitor`] that collects all `return` statements in a function or method.
#[derive(Default)]
pub struct ReturnStatementVisitor<'a> {
    pub returns: Vec<&'a ast::StmtReturn>,
    pub is_generator: bool,
}

impl<'a> Visitor<'a> for ReturnStatementVisitor<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {
                // Don't recurse.
            }
            Stmt::Return(stmt) => self.returns.push(stmt),
            _ => crate::visitor::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Yield(_) | Expr::YieldFrom(_) = expr {
            self.is_generator = true;
        } else {
            crate::visitor::walk_expr(self, expr);
        }
    }
}

/// A [`StatementVisitor`] that collects all `raise` statements in a function or method.
#[derive(Default)]
pub struct RaiseStatementVisitor<'a> {
    pub raises: Vec<(TextRange, Option<&'a Expr>, Option<&'a Expr>)>,
}

impl<'a> StatementVisitor<'a> for RaiseStatementVisitor<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::Raise(ast::StmtRaise {
                exc,
                cause,
                range: _,
                node_index: _,
            }) => {
                self.raises
                    .push((stmt.range(), exc.as_deref(), cause.as_deref()));
            }
            Stmt::ClassDef(_) | Stmt::FunctionDef(_) | Stmt::Try(_) => {}
            Stmt::If(ast::StmtIf {
                body,
                elif_else_clauses,
                ..
            }) => {
                crate::statement_visitor::walk_body(self, body);
                for clause in elif_else_clauses {
                    self.visit_elif_else_clause(clause);
                }
            }
            Stmt::While(ast::StmtWhile { body, .. })
            | Stmt::With(ast::StmtWith { body, .. })
            | Stmt::For(ast::StmtFor { body, .. }) => {
                crate::statement_visitor::walk_body(self, body);
            }
            Stmt::Match(ast::StmtMatch { cases, .. }) => {
                for case in cases {
                    crate::statement_visitor::walk_body(self, &case.body);
                }
            }
            _ => {}
        }
    }
}

/// A [`Visitor`] that detects the presence of `await` expressions in the current scope.
#[derive(Debug, Default)]
pub struct AwaitVisitor {
    pub seen_await: bool,
}

impl Visitor<'_> for AwaitVisitor {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => (),
            Stmt::With(ast::StmtWith { is_async: true, .. }) => {
                self.seen_await = true;
            }
            Stmt::For(ast::StmtFor { is_async: true, .. }) => {
                self.seen_await = true;
            }
            _ => crate::visitor::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if let Expr::Await(ast::ExprAwait { .. }) = expr {
            self.seen_await = true;
        } else {
            crate::visitor::walk_expr(self, expr);
        }
    }

    fn visit_comprehension(&mut self, comprehension: &'_ crate::Comprehension) {
        if comprehension.is_async {
            self.seen_await = true;
        } else {
            crate::visitor::walk_comprehension(self, comprehension);
        }
    }
}

/// Return `true` if a `Stmt` is a docstring.
pub fn is_docstring_stmt(stmt: &Stmt) -> bool {
    if let Stmt::Expr(ast::StmtExpr {
        value,
        range: _,
        node_index: _,
    }) = stmt
    {
        value.is_string_literal_expr()
    } else {
        false
    }
}

/// Returns `true` if all statements in `body` are `pass` or `...` (ellipsis)
///
/// An empty body (`[]`) returns `false`
pub fn is_stub_body(body: &[Stmt]) -> bool {
    !body.is_empty()
        && body.iter().all(|stmt| match stmt {
            Stmt::Pass(_) => true,
            Stmt::Expr(ast::StmtExpr { value, .. }) => value.is_ellipsis_literal_expr(),
            _ => false,
        })
}

/// Returns `body` without its leading docstring statement, if present.
pub fn body_without_leading_docstring(body: &[Stmt]) -> &[Stmt] {
    match body.split_first() {
        Some((first, rest)) if is_docstring_stmt(first) => rest,
        _ => body,
    }
}

/// Check if a node is part of a conditional branch.
pub fn on_conditional_branch<'a>(parents: &mut impl Iterator<Item = &'a Stmt>) -> bool {
    parents.any(|parent| {
        if matches!(parent, Stmt::If(_) | Stmt::While(_) | Stmt::Match(_)) {
            return true;
        }
        if let Stmt::Expr(ast::StmtExpr {
            value,
            range: _,
            node_index: _,
        }) = parent
        {
            if value.is_if_expr() {
                return true;
            }
        }
        false
    })
}

/// Check if a node is in a nested block.
pub fn in_nested_block<'a>(mut parents: impl Iterator<Item = &'a Stmt>) -> bool {
    parents.any(|parent| {
        matches!(
            parent,
            Stmt::Try(_) | Stmt::If(_) | Stmt::With(_) | Stmt::Match(_)
        )
    })
}

/// Check if a node represents an unpacking assignment.
pub fn is_unpacking_assignment(parent: &Stmt, child: &Expr) -> bool {
    match parent {
        Stmt::With(ast::StmtWith { items, .. }) => items.iter().any(|item| {
            if let Some(optional_vars) = &item.optional_vars {
                if optional_vars.is_tuple_expr() {
                    if any_over_expr(optional_vars, |expr| expr == child) {
                        return true;
                    }
                }
            }
            false
        }),
        Stmt::Assign(ast::StmtAssign { targets, value, .. }) => {
            // In `(a, b) = (1, 2)`, `(1, 2)` is the target, and it is a tuple.
            let value_is_tuple = matches!(
                value.as_ref(),
                Expr::Set(_) | Expr::List(_) | Expr::Tuple(_)
            );
            // In `(a, b) = coords = (1, 2)`, `(a, b)` and `coords` are the targets, and
            // `(a, b)` is a tuple. (We use "tuple" as a placeholder for any
            // unpackable expression.)
            let targets_are_tuples = targets
                .iter()
                .all(|item| matches!(item, Expr::Set(_) | Expr::List(_) | Expr::Tuple(_)));
            // If we're looking at `a` in `(a, b) = coords = (1, 2)`, then we should
            // identify that the current expression is in a tuple.
            let child_in_tuple = targets_are_tuples
                || targets.iter().any(|item| {
                    matches!(item, Expr::Set(_) | Expr::List(_) | Expr::Tuple(_))
                        && any_over_expr(item, |expr| expr == child)
                });

            // If our child is a tuple, and value is not, it's always an unpacking
            // expression. Ex) `x, y = tup`
            if child_in_tuple && !value_is_tuple {
                return true;
            }

            // If our child isn't a tuple, but value is, it's never an unpacking expression.
            // Ex) `coords = (1, 2)`
            if !child_in_tuple && value_is_tuple {
                return false;
            }

            // If our target and the value are both tuples, then it's an unpacking
            // expression assuming there's at least one non-tuple child.
            // Ex) Given `(x, y) = coords = 1, 2`, `(x, y)` is considered an unpacking
            // expression. Ex) Given `(x, y) = (a, b) = 1, 2`, `(x, y)` isn't
            // considered an unpacking expression.
            if child_in_tuple && value_is_tuple {
                return !targets_are_tuples;
            }

            false
        }
        _ => false,
    }
}

#[derive(Copy, Clone, Debug, PartialEq, is_macro::Is)]
pub enum Truthiness {
    /// The expression is `True`.
    True,
    /// The expression is `False`.
    False,
    /// The expression evaluates to a `False`-like value (e.g., `None`, `0`, `[]`, `""`).
    Falsey,
    /// The expression evaluates to a `True`-like value (e.g., `1`, `"foo"`).
    Truthy,
    /// The expression evaluates to `None`.
    None,
    /// The expression evaluates to an unknown value (e.g., a variable `x` of unknown type).
    Unknown,
}

impl Truthiness {
    /// Return the truthiness of an expression.
    pub fn from_expr<F>(expr: &Expr, is_builtin: F) -> Self
    where
        F: Fn(&str) -> bool,
    {
        match expr {
            Expr::Lambda(_) => Self::Truthy,
            Expr::Generator(_) => Self::Truthy,
            Expr::StringLiteral(ast::ExprStringLiteral { value, .. }) => {
                if value.is_empty() {
                    Self::Falsey
                } else {
                    Self::Truthy
                }
            }
            Expr::BytesLiteral(ast::ExprBytesLiteral { value, .. }) => {
                if value.is_empty() {
                    Self::Falsey
                } else {
                    Self::Truthy
                }
            }
            Expr::NumberLiteral(ast::ExprNumberLiteral { value, .. }) => match value {
                ast::Number::Int(int) => {
                    if *int == 0 {
                        Self::Falsey
                    } else {
                        Self::Truthy
                    }
                }
                ast::Number::Float(float) => {
                    if *float == 0.0 {
                        Self::Falsey
                    } else {
                        Self::Truthy
                    }
                }
                ast::Number::Complex { real, imag, .. } => {
                    if *real == 0.0 && *imag == 0.0 {
                        Self::Falsey
                    } else {
                        Self::Truthy
                    }
                }
            },
            Expr::BooleanLiteral(ast::ExprBooleanLiteral { value, .. }) => {
                if *value {
                    Self::True
                } else {
                    Self::False
                }
            }
            Expr::NoneLiteral(_) => Self::None,
            Expr::EllipsisLiteral(_) => Self::Truthy,
            Expr::FString(f_string) => {
                if is_empty_f_string(f_string) {
                    Self::Falsey
                } else if is_non_empty_f_string(f_string) {
                    Self::Truthy
                } else {
                    Self::Unknown
                }
            }
            Expr::TString(_) => Self::Truthy,
            Expr::List(ast::ExprList { elts, .. })
            | Expr::Set(ast::ExprSet { elts, .. })
            | Expr::Tuple(ast::ExprTuple { elts, .. }) => {
                if elts.is_empty() {
                    return Self::Falsey;
                }

                if elts.iter().all(Expr::is_starred_expr) {
                    // [*foo] / [*foo, *bar]
                    Self::Unknown
                } else {
                    Self::Truthy
                }
            }
            Expr::Dict(dict) => {
                if dict.is_empty() {
                    return Self::Falsey;
                }

                // If the dict consists only of double-starred items (e.g., {**x, **y}),
                // consider its truthiness unknown. This matches lists/sets/tuples containing
                // only starred elements, which are also Unknown.
                if dict
                    .items
                    .iter()
                    .all(|item| matches!(item, DictItem { key: None, .. }))
                {
                    // {**foo} / {**foo, **bar}
                    Self::Unknown
                } else {
                    Self::Truthy
                }
            }
            Expr::Call(ast::ExprCall {
                func, arguments, ..
            }) => {
                if let Expr::Name(ast::ExprName { id, .. }) = func.as_ref() {
                    if is_iterable_initializer(id.as_str(), |id| is_builtin(id)) {
                        if arguments.is_empty() {
                            // Ex) `list()`
                            Self::Falsey
                        } else if let [argument] = &*arguments.args
                            && arguments.keywords.is_empty()
                        {
                            // Ex) `list([1, 2, 3])`
                            match argument {
                                // Return Unknown for types with definite truthiness that might
                                // result in empty iterables (t-strings and generators) or will
                                // raise a type error (non-iterable types like numbers, booleans,
                                // None, etc.).
                                Expr::NumberLiteral(_)
                                | Expr::BooleanLiteral(_)
                                | Expr::NoneLiteral(_)
                                | Expr::EllipsisLiteral(_)
                                | Expr::TString(_)
                                | Expr::Lambda(_)
                                | Expr::Generator(_) => Self::Unknown,
                                // Recurse for all other types - collections, comprehensions, variables, etc.
                                // StringLiteral, FString, and BytesLiteral recurse because Self::from_expr
                                // correctly handles their truthiness (checking if empty or not).
                                _ => Self::from_expr(argument, is_builtin),
                            }
                        } else {
                            Self::Unknown
                        }
                    } else {
                        Self::Unknown
                    }
                } else {
                    Self::Unknown
                }
            }
            _ => Self::Unknown,
        }
    }

    pub fn into_bool(self) -> Option<bool> {
        match self {
            Self::True | Self::Truthy => Some(true),
            Self::False | Self::Falsey => Some(false),
            Self::None => Some(false),
            Self::Unknown => None,
        }
    }
}

/// Returns `true` if the expression definitely resolves to a non-empty string, when used as an
/// f-string expression, or `false` if the expression may resolve to an empty string.
fn is_non_empty_f_string(expr: &ast::ExprFString) -> bool {
    fn inner(expr: &Expr) -> bool {
        match expr {
            // When stringified, these expressions are always non-empty.
            Expr::Lambda(_) => true,
            Expr::Dict(_) => true,
            Expr::Set(_) => true,
            Expr::ListComp(_) => true,
            Expr::SetComp(_) => true,
            Expr::DictComp(_) => true,
            Expr::NumberLiteral(_) => true,
            Expr::BooleanLiteral(_) => true,
            Expr::NoneLiteral(_) => true,
            Expr::EllipsisLiteral(_) => true,
            Expr::List(_) => true,
            Expr::Tuple(_) => true,
            Expr::TString(_) => true,

            // These expressions must resolve to the inner expression.
            Expr::If(ast::ExprIf { body, orelse, .. }) => inner(body) && inner(orelse),
            Expr::Named(ast::ExprNamed { value, .. }) => inner(value),

            // These expressions are complex. We can't determine whether they're empty or not.
            Expr::BoolOp(ast::ExprBoolOp { .. }) => false,
            Expr::BinOp(ast::ExprBinOp { .. }) => false,
            Expr::UnaryOp(ast::ExprUnaryOp { .. }) => false,
            // Rich comparison methods can return arbitrary objects.
            Expr::Compare(_) => false,
            Expr::Generator(_) => false,
            Expr::Await(_) => false,
            Expr::Yield(_) => false,
            Expr::YieldFrom(_) => false,
            Expr::Call(_) => false,
            Expr::Attribute(_) => false,
            Expr::Subscript(_) => false,
            Expr::Starred(_) => false,
            Expr::Name(_) => false,
            Expr::Slice(_) => false,
            Expr::IpyEscapeCommand(_) => false,
            Expr::CallableType(_) => false,
            Expr::ProtocolType(_) => false,
            Expr::ProtocolMethod(_) => false,
            Expr::Statement(_) => false,

            // These literals may or may not be empty.
            Expr::FString(f_string) => is_non_empty_f_string(f_string),
            // These literals may or may not be empty.
            Expr::StringLiteral(ast::ExprStringLiteral { value, .. }) => !value.is_empty(),
            // Confusingly, f"{b""}" renders as the string 'b""', which is non-empty.
            // Therefore, any bytes interpolation is guaranteed non-empty when stringified.
            Expr::BytesLiteral(_) => true,
        }
    }

    expr.value.iter().any(|part| match part {
        ast::FStringPart::Literal(string_literal) => !string_literal.is_empty(),
        ast::FStringPart::FString(f_string) => {
            // The part is a concatenation of elements, so it's guaranteed non-empty if any element is
            f_string.elements.iter().any(|element| match element {
                InterpolatedStringElement::Literal(string_literal) => !string_literal.is_empty(),
                InterpolatedStringElement::Interpolation(f_string) => {
                    f_string.debug_text.is_some()
                        || (f_string.format_spec.is_none() && inner(&f_string.expression))
                }
            })
        }
    })
}

/// Returns `true` if the expression definitely resolves to the empty string, when used as an f-string
/// expression.
pub fn is_empty_f_string(expr: &ast::ExprFString) -> bool {
    fn inner(expr: &Expr) -> bool {
        match expr {
            Expr::StringLiteral(ast::ExprStringLiteral { value, .. }) => value.is_empty(),
            // Confusingly, `bool(f"{b""}") == True` even though
            // `bool(b"") == False`. This is because `f"{b""}"`
            // evaluates as the string `'b""'` of length 3.
            Expr::BytesLiteral(_) => false,
            Expr::FString(ast::ExprFString { value, .. }) => {
                is_empty_interpolated_elements(value.elements())
            }
            _ => false,
        }
    }

    fn is_empty_interpolated_elements<'a>(
        mut elements: impl Iterator<Item = &'a InterpolatedStringElement>,
    ) -> bool {
        elements.all(|element| match element {
            InterpolatedStringElement::Literal(ast::InterpolatedStringLiteralElement {
                value,
                ..
            }) => value.is_empty(),
            InterpolatedStringElement::Interpolation(f_string) => {
                f_string.debug_text.is_none()
                    && f_string.conversion.is_none()
                    && f_string.format_spec.is_none()
                    && inner(&f_string.expression)
            }
        })
    }

    expr.value.iter().all(|part| match part {
        ast::FStringPart::Literal(string_literal) => string_literal.is_empty(),
        ast::FStringPart::FString(f_string) => {
            is_empty_interpolated_elements(f_string.elements.iter())
        }
    })
}

pub fn generate_comparison(
    left: &Expr,
    ops: &[CmpOp],
    comparators: &[Expr],
    parent: AnyNodeRef,
    tokens: &Tokens,
    source: &str,
) -> String {
    let start = left.start();
    let end = comparators.last().map_or_else(|| left.end(), Ranged::end);
    let mut contents = String::with_capacity(usize::from(end - start));

    // Add the left side of the comparison.
    contents.push_str(
        &source[parenthesized_range(left.into(), parent, tokens).unwrap_or(left.range())],
    );

    for (op, comparator) in ops.iter().zip(comparators) {
        // Add the operator.
        contents.push_str(match op {
            CmpOp::Eq => " == ",
            CmpOp::NotEq => " != ",
            CmpOp::Lt => " < ",
            CmpOp::LtE => " <= ",
            CmpOp::Gt => " > ",
            CmpOp::GtE => " >= ",
            CmpOp::In => " in ",
            CmpOp::NotIn => " not in ",
            CmpOp::Is => " is ",
            CmpOp::IsNot => " is not ",
        });

        // Add the right side of the comparison.
        contents.push_str(
            &source[parenthesized_range(comparator.into(), parent, tokens)
                .unwrap_or(comparator.range())],
        );
    }

    contents
}

/// Format the expression as a PEP 604-style optional.
pub fn pep_604_optional(expr: &Expr) -> Expr {
    ast::ExprBinOp {
        left: Box::new(expr.clone()),
        op: Operator::BitOr,
        right: Box::new(Expr::NoneLiteral(ExprNoneLiteral::default())),
        range: TextRange::default(),
        node_index: AtomicNodeIndex::NONE,
    }
    .into()
}

/// Format the expressions as a PEP 604-style union.
pub fn pep_604_union(elts: &[Expr]) -> Expr {
    match elts {
        [] => Expr::Tuple(ast::ExprTuple {
            elts: vec![],
            ctx: ExprContext::Load,
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
            parenthesized: true,
            is_anon_named_tuple: false,
            is_anon_named_tuple_value: false,
            callable_shape: None,
            is_parameter_shape: false,
        }),
        [Expr::Tuple(ast::ExprTuple { elts, .. })] => pep_604_union(elts),
        [elt] => elt.clone(),
        [rest @ .., elt] => Expr::BinOp(ast::ExprBinOp {
            left: Box::new(pep_604_union(rest)),
            op: Operator::BitOr,
            right: Box::new(pep_604_union(std::slice::from_ref(elt))),
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
        }),
    }
}

/// Format the expression as a `typing.Optional`-style optional.
pub fn typing_optional(elt: Expr, binding: Name) -> Expr {
    Expr::Subscript(ast::ExprSubscript {
        value: Box::new(Expr::Name(ast::ExprName {
            id: binding,
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
            ctx: ExprContext::Load,
        })),
        slice: Box::new(elt),
        ctx: ExprContext::Load,
        range: TextRange::default(),
        node_index: AtomicNodeIndex::NONE,
        is_typeof: false,
    })
}

/// Format the expressions as a `typing.Union`-style union.
///
/// Note: It is a syntax error to have `Union[]` so the caller
/// should ensure that the `elts` argument is nonempty.
pub fn typing_union(elts: &[Expr], binding: Name) -> Expr {
    Expr::Subscript(ast::ExprSubscript {
        value: Box::new(Expr::Name(ast::ExprName {
            id: binding,
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
            ctx: ExprContext::Load,
        })),
        slice: Box::new(Expr::Tuple(ast::ExprTuple {
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
            elts: elts.to_vec(),
            ctx: ExprContext::Load,
            parenthesized: false,
            is_anon_named_tuple: false,
            is_anon_named_tuple_value: false,
            callable_shape: None,
            is_parameter_shape: false,
        })),
        ctx: ExprContext::Load,
        range: TextRange::default(),
        node_index: AtomicNodeIndex::NONE,
        is_typeof: false,
    })
}

/// Determine the indentation level of an own-line comment, defined as the minimum indentation of
/// all comments between the preceding node and the comment, including the comment itself. In
/// other words, we don't allow successive comments to ident _further_ than any preceding comments.
///
/// For example, given:
/// ```python
/// if True:
///     pass
///     # comment
/// ```
///
/// The indentation would be 4, as the comment is indented by 4 spaces.
///
/// Given:
/// ```python
/// if True:
///     pass
/// # comment
/// else:
///     pass
/// ```
///
/// The indentation would be 0, as the comment is not indented at all.
///
/// Given:
/// ```python
/// if True:
///     pass
///     # comment
///         # comment
/// ```
///
/// Both comments would be marked as indented at 4 spaces, as the indentation of the first comment
/// is used for the second comment.
///
/// This logic avoids pathological cases like:
/// ```python
/// try:
///     if True:
///         if True:
///             pass
///
///         # a
///             # b
///         # c
/// except Exception:
///     pass
/// ```
///
/// If we don't use the minimum indentation of any preceding comments, we would mark `# b` as
/// indented to the same depth as `pass`, which could in turn lead to us treating it as a trailing
/// comment of `pass`, despite there being a comment between them that "resets" the indentation.
pub fn comment_indentation_after(
    preceding: AnyNodeRef,
    comment_range: TextRange,
    source: &str,
) -> TextSize {
    let tokenizer = SimpleTokenizer::new(
        source,
        TextRange::new(source.full_line_end(preceding.end()), comment_range.end()),
    );

    tokenizer
        .filter_map(|token| {
            if token.kind() == SimpleTokenKind::Comment {
                indentation_at_offset(token.start(), source).map(TextLen::text_len)
            } else {
                None
            }
        })
        .min()
        .unwrap_or_default()
}

pub fn is_dotted_name(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::Name(_) => true,
        ast::Expr::Attribute(ast::ExprAttribute { value, .. }) => is_dotted_name(value),
        _ => false,
    }
}

/// The synthetic marker decorator the parser attaches to a `type def`.
///
/// A `type def` is parsed as an ordinary [`crate::StmtFunctionDef`] tagged with
/// this marker — a zero-binding `Name` with [`crate::ExprContext::Invalid`], whose range
/// covers the `type` keyword text. Every consumer (the type checker, the
/// transpiler's erasure pass, the reification pass, the formatter) must agree on
/// the spelling, so they all go through [`is_type_def`] rather than matching the
/// string themselves.
pub const TYPE_FN_MARKER: &str = "type_fn";

/// Whether `function` came from basedpython's `type def` — a type function,
/// applied with `[]` in a type expression and evaluated by executing its body.
pub fn is_type_def(function: &crate::StmtFunctionDef) -> bool {
    function.decorator_list.iter().any(|decorator| {
        matches!(
            &decorator.expression,
            crate::Expr::Name(name)
                if name.id.as_str() == TYPE_FN_MARKER
                    && matches!(name.ctx, crate::ExprContext::Invalid)
        )
    })
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::cell::RefCell;
    use std::vec;

    use ruff_text_size::TextRange;

    use crate::helpers::{any_over_stmt, any_over_type_param, resolve_imported_module_path};
    use crate::{
        AtomicNodeIndex, Expr, ExprContext, ExprName, ExprNumberLiteral, Identifier, Int, Number,
        Stmt, StmtTypeAlias, TypeParam, TypeParamParamSpec, TypeParamTypeVar,
        TypeParamTypeVarTuple, TypeParams,
    };

    #[test]
    fn resolve_import() {
        // Return the module directly.
        assert_eq!(
            resolve_imported_module_path(0, Some("foo"), None),
            Some(Cow::Borrowed("foo"))
        );

        // Construct the module path from the calling module's path.
        assert_eq!(
            resolve_imported_module_path(
                1,
                Some("foo"),
                Some(&["bar".to_string(), "baz".to_string()])
            ),
            Some(Cow::Owned("bar.foo".to_string()))
        );

        // We can't return the module if it's a relative import, and we don't know the calling
        // module's path.
        assert_eq!(resolve_imported_module_path(1, Some("foo"), None), None);

        // We can't return the module if it's a relative import, and the path goes beyond the
        // calling module's path.
        assert_eq!(
            resolve_imported_module_path(1, Some("foo"), Some(&["bar".to_string()])),
            None,
        );
        assert_eq!(
            resolve_imported_module_path(2, Some("foo"), Some(&["bar".to_string()])),
            None
        );
    }

    #[test]
    fn any_over_stmt_type_alias() {
        let seen = RefCell::new(Vec::new());
        let name = Expr::Name(ExprName {
            id: "x".into(),
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
            ctx: ExprContext::Load,
        });
        let constant_one = Expr::NumberLiteral(ExprNumberLiteral {
            value: Number::Int(Int::from(1u8)),
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
        });
        let constant_two = Expr::NumberLiteral(ExprNumberLiteral {
            value: Number::Int(Int::from(2u8)),
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
        });
        let constant_three = Expr::NumberLiteral(ExprNumberLiteral {
            value: Number::Int(Int::from(3u8)),
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
        });
        let type_var_one = TypeParam::TypeVar(TypeParamTypeVar {
            lower_bound: None,
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
            bound: Some(Box::new(constant_one.clone())),
            default: None,
            name: Identifier::new("x", TextRange::default()),
            variance: None,
        });
        let type_var_two = TypeParam::TypeVar(TypeParamTypeVar {
            lower_bound: None,
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
            bound: None,
            default: Some(Box::new(constant_two.clone())),
            name: Identifier::new("x", TextRange::default()),
            variance: None,
        });
        let type_alias = Stmt::TypeAlias(StmtTypeAlias {
            cases: Vec::new(),
            name: Box::new(name.clone()),
            type_params: Some(Box::new(TypeParams {
                type_params: vec![type_var_one, type_var_two],
                range: TextRange::default(),
                node_index: AtomicNodeIndex::NONE,
                separators: crate::TypeParamSeparators::default(),
            })),
            value: Box::new(constant_three.clone()),
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
            is_private: false,
        });
        assert!(!any_over_stmt(&type_alias, |expr| {
            seen.borrow_mut().push(expr.clone());
            false
        }));
        assert_eq!(
            seen.take(),
            vec![name, constant_one, constant_two, constant_three]
        );
    }

    #[test]
    fn any_over_type_param_type_var() {
        let type_var_no_bound = TypeParam::TypeVar(TypeParamTypeVar {
            lower_bound: None,
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
            bound: None,
            default: None,
            name: Identifier::new("x", TextRange::default()),
            variance: None,
        });
        assert!(!any_over_type_param(&type_var_no_bound, &mut |_expr| true));

        let constant = Expr::NumberLiteral(ExprNumberLiteral {
            value: Number::Int(Int::ONE),
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
        });

        let type_var_with_bound = TypeParam::TypeVar(TypeParamTypeVar {
            lower_bound: None,
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
            bound: Some(Box::new(constant.clone())),
            default: None,
            name: Identifier::new("x", TextRange::default()),
            variance: None,
        });
        assert!(
            any_over_type_param(&type_var_with_bound, &mut |expr| {
                assert_eq!(
                    *expr, constant,
                    "the received expression should be the unwrapped bound"
                );
                true
            }),
            "if true is returned from `func` it should be respected"
        );

        let type_var_with_default = TypeParam::TypeVar(TypeParamTypeVar {
            lower_bound: None,
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
            default: Some(Box::new(constant.clone())),
            bound: None,
            name: Identifier::new("x", TextRange::default()),
            variance: None,
        });
        assert!(
            any_over_type_param(&type_var_with_default, &mut |expr| {
                assert_eq!(
                    *expr, constant,
                    "the received expression should be the unwrapped default"
                );
                true
            }),
            "if true is returned from `func` it should be respected"
        );
    }

    #[test]
    fn any_over_type_param_type_var_tuple() {
        let type_var_tuple = TypeParam::TypeVarTuple(TypeParamTypeVarTuple {
            bound: None,
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
            name: Identifier::new("x", TextRange::default()),
            default: None,
        });
        assert!(
            !any_over_type_param(&type_var_tuple, &mut |_expr| true),
            "this TypeVarTuple has no expressions to visit"
        );

        let constant = Expr::NumberLiteral(ExprNumberLiteral {
            value: Number::Int(Int::ONE),
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
        });

        let type_var_tuple_with_default = TypeParam::TypeVarTuple(TypeParamTypeVarTuple {
            bound: None,
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
            default: Some(Box::new(constant.clone())),
            name: Identifier::new("x", TextRange::default()),
        });
        assert!(
            any_over_type_param(&type_var_tuple_with_default, &mut |expr| {
                assert_eq!(
                    *expr, constant,
                    "the received expression should be the unwrapped default"
                );
                true
            }),
            "if true is returned from `func` it should be respected"
        );
    }

    #[test]
    fn any_over_type_param_param_spec() {
        let type_param_spec = TypeParam::ParamSpec(TypeParamParamSpec {
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
            name: Identifier::new("x", TextRange::default()),
            default: None,
        });
        assert!(
            !any_over_type_param(&type_param_spec, &mut |_expr| true),
            "this ParamSpec has no expressions to visit"
        );

        let constant = Expr::NumberLiteral(ExprNumberLiteral {
            value: Number::Int(Int::ONE),
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
        });

        let param_spec_with_default = TypeParam::TypeVarTuple(TypeParamTypeVarTuple {
            bound: None,
            range: TextRange::default(),
            node_index: AtomicNodeIndex::NONE,
            default: Some(Box::new(constant.clone())),
            name: Identifier::new("x", TextRange::default()),
        });
        assert!(
            any_over_type_param(&param_spec_with_default, &mut |expr| {
                assert_eq!(
                    *expr, constant,
                    "the received expression should be the unwrapped default"
                );
                true
            }),
            "if true is returned from `func` it should be respected"
        );
    }
}
