//! basedpython: deferred type-level operations
//!
//! basedpython folds operations on literals at the type level: `Array[1 + 1]` is
//! `Array[2]`, `1 < 2` is `Literal[True]`, `"ab".startswith("a")` is `Literal[True]`.
//! but when an operand still mentions a type parameter — as in the return annotation
//!
//! ```by
//! def extend[Dim: int](a: Array[Dim]) -> Array[Dim + 1]
//! ```
//!
//! the operation cannot be evaluated yet: `Dim` is unknown until the call is
//! specialized. eagerly reducing it would collapse `Dim + 1` to `int` and throw the
//! relationship away, so `extend(x)` for `x: Array[5]` would infer `Array[int]`
//! rather than `Array[6]`.
//!
//! [`DeferredType`] keeps such an operation symbolic. it behaves like its
//! [reduced](DeferredType::reduced) form (`int` for `Dim + 1`, `bool` for a
//! comparison) for every purpose — subtyping, display, member lookup — except
//! type-mapping: when a specialization substitutes the type parameter, the operation
//! is re-run against the substituted operands with the very same fold the value-level
//! inferrer uses, so `Dim + 1` with `Dim = 5` folds to `Literal[6]`.
//!
//! this is one mechanism spanning every foldable operation kind (see
//! [`DeferredOperation`]); the fold for each kind is shared with value inference
//! rather than reimplemented here.

use ruff_python_ast as ast;

use super::Type;
use super::infer::{deferred_comparison, literal_binary_op, literal_unary_op};
use super::visitor::{self, any_over_type};
use crate::Db;
use crate::types::call::CallArguments;
use crate::types::{KnownClass, TypeContext};

/// The kind of a [`DeferredType`]'s pending operation. Each variant fixes how many
/// operands the deferral carries and which value-level fold re-evaluates it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DeferredOperation {
    /// `left op right`; operands are `[left, right]`.
    Binary(ast::Operator),
    /// `op operand`; operands are `[operand]`.
    Unary(ast::UnaryOp),
    /// `left op right`; operands are `[left, right]`.
    Compare(ast::CmpOp),
    /// `callee(arg0, arg1, ...)`; operands are `[callee, arg0, arg1, ...]`. Covers
    /// method calls too — the callee is the (typevar-receiver) bound method.
    Call,
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for DeferredOperation {}

#[salsa::interned(debug, heap_size = ruff_memory_usage::heap_size)]
pub struct DeferredType<'db> {
    #[returns(copy)]
    pub(crate) operation: DeferredOperation,
    #[returns(deref)]
    pub(crate) operands: Box<[Type<'db>]>,
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for DeferredType<'_> {}

pub(super) fn walk_deferred_type<'db, V: visitor::TypeVisitor<'db> + ?Sized>(
    db: &'db dyn Db,
    deferred: DeferredType<'db>,
    visitor: &V,
) {
    for operand in deferred.operands(db) {
        visitor.visit_type(db, *operand);
    }
}

impl<'db> DeferredType<'db> {
    /// Build the type of an operation in a type expression.
    ///
    /// Fully concrete operands are folded immediately (`1 + 1` → `Literal[2]`). If any
    /// operand still mentions a type parameter, the operation is kept symbolic so it
    /// can be re-evaluated on specialization. Otherwise it reduces to the ordinary
    /// (non-literal) result (`int + Literal[1]` → `int`).
    pub(crate) fn build(
        db: &'db dyn Db,
        operation: DeferredOperation,
        operands: Box<[Type<'db>]>,
    ) -> Type<'db> {
        if operands
            .iter()
            .any(|operand| operand_is_symbolic(db, *operand))
        {
            return Type::Deferred(Self::new(db, operation, operands));
        }
        evaluate(db, operation, &operands).unwrap_or_else(Type::unknown)
    }

    /// Whether an operation over these operands must be deferred, i.e. some operand
    /// still mentions a type parameter.
    pub(crate) fn is_deferred(db: &'db dyn Db, operands: &[Type<'db>]) -> bool {
        operands
            .iter()
            .any(|operand| operand_is_symbolic(db, *operand))
    }

    /// The non-symbolic meaning of the operation: what the result type would be if
    /// each operand were replaced by its upper bound. `Dim + 1` (with `Dim: int`)
    /// reduces to `int`, a comparison reduces to `bool`. Every non-mapping operation
    /// delegates here, so a deferred operation is indistinguishable from its reduced
    /// form everywhere except under type-mapping.
    pub(crate) fn reduced(self, db: &'db dyn Db) -> Type<'db> {
        let operands: Box<[Type<'db>]> = self
            .operands(db)
            .iter()
            .map(|operand| operand.reduce_deferred(db))
            .collect();
        evaluate(db, self.operation(db), &operands).unwrap_or_else(Type::unknown)
    }

    /// Re-evaluate the operation against operands to which a type-mapping has already
    /// been applied. Folds when the operands became concrete, stays symbolic while
    /// still unresolved.
    pub(crate) fn re_evaluate(self, db: &'db dyn Db, operands: Box<[Type<'db>]>) -> Type<'db> {
        Self::build(db, self.operation(db), operands)
    }
}

impl<'db> Type<'db> {
    /// Collapse a top-level [`DeferredType`] to its reduced form; any other type is
    /// returned unchanged.
    pub(crate) fn reduce_deferred(self, db: &'db dyn Db) -> Type<'db> {
        match self {
            Type::Deferred(deferred) => deferred.reduced(db),
            _ => self,
        }
    }
}

/// Evaluate a deferred operation against operands that no longer mention a type
/// parameter, reusing value inference's fold. Returns `None` only when the operation
/// is genuinely unsupported between the operands.
fn evaluate<'db>(
    db: &'db dyn Db,
    operation: DeferredOperation,
    operands: &[Type<'db>],
) -> Option<Type<'db>> {
    match operation {
        DeferredOperation::Binary(op) => {
            let [left, right] = operands else { return None };
            literal_binary_op(db, *left, *right, op, true)
                .or_else(|| Type::try_call_bin_op_return_type(db, *left, op, *right))
        }
        DeferredOperation::Unary(op) => {
            let [operand] = operands else { return None };
            if let Type::LiteralValue(literal) = operand
                && let Some(folded) = literal_unary_op(db, op, *literal)
            {
                return Some(folded);
            }
            let dunder = match op {
                ast::UnaryOp::USub => "__neg__",
                ast::UnaryOp::UAdd => "__pos__",
                ast::UnaryOp::Invert => "__invert__",
                _ => return None,
            };
            operand
                .try_call_dunder(db, dunder, CallArguments::none(), TypeContext::default())
                .ok()
                .map(|bindings| bindings.return_type(db))
        }
        DeferredOperation::Call => {
            let [callee, args @ ..] = operands else {
                return None;
            };
            // re-resolve a bound method against its (now possibly concrete) receiver,
            // so a known literal-folding method — `str.startswith`, ... — re-folds once
            // the receiver is a literal. member lookup on `Literal["ab"]` yields the
            // folding `KnownBoundMethod`, whereas the stored method was bound to the
            // still-abstract type parameter
            let callee = match callee {
                Type::BoundMethod(method) => method
                    .self_instance(db)
                    .member(db, method.function(db).name(db))
                    .ignore_possibly_undefined()
                    .unwrap_or(*callee),
                _ => *callee,
            };
            callee
                .try_call(db, &CallArguments::positional(args.iter().copied()))
                .ok()
                .map(|bindings| bindings.return_type(db))
        }
        DeferredOperation::Compare(op) => {
            let [left, right] = operands else { return None };
            // rich comparisons fold to a `Literal[bool]` for literal operands and to
            // `bool` otherwise; identity/membership operators fall back to `bool`
            Some(
                deferred_comparison(db, *left, op, *right)
                    .unwrap_or_else(|| KnownClass::Bool.to_instance(db)),
            )
        }
    }
}

/// Whether `ty` still mentions a type parameter (or a nested deferred operation),
/// meaning an operation over it cannot be evaluated yet.
fn operand_is_symbolic<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    any_over_type(db, ty, false, |t| {
        t.as_typevar().is_some() || matches!(t, Type::Deferred(_))
    })
}
