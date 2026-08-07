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
//! the same reasoning covers an *attribute type* — `T.a`, the type of member `a` on
//! whatever `T` turns out to be:
//!
//! ```by
//! class B[T: A1]:
//!     x: T.a
//! ```
//!
//! reducing it eagerly to `A1`'s `a` would make `B[A2]().x` read as `A1.a`'s type even
//! when `A2` redeclares `a`.
//!
//! this is one mechanism spanning every foldable operation kind (see
//! [`DeferredOperation`]); the fold for each kind is shared with value inference
//! rather than reimplemented here.

use ruff_python_ast as ast;
use ruff_python_ast::name::Name;

use super::Type;
use super::infer::{
    deferred_comparison, fold_tuple_concat, fold_tuple_repeat, literal_binary_op, literal_unary_op,
};
use super::visitor::{self, any_over_type};
use crate::types::call::CallArguments;
use crate::types::match_type::{MatchTypeOutcome, evaluate_match_type};
use crate::types::type_fn::{
    TypeFnArguments, TypeFnOutcome, declared_return_type, evaluate_type_fn,
};
use crate::types::{KnownClass, KnownInstanceType, TypeContext};
use crate::{Db, FxOrderMap};

/// The kind of a [`DeferredType`]'s pending operation. Each variant fixes how many
/// operands the deferral carries and which value-level fold re-evaluates it.
// Unlike the Salsa handles in this module, this is a plain payload stored *inside*
// the interned struct, so its heap (an attribute type's member name) is the interned
// value's own and is derived rather than tracked separately.
#[derive(Clone, PartialEq, Eq, Hash, Debug, get_size2::GetSize)]
pub enum DeferredOperation {
    /// `left op right`; operands are `[left, right]`.
    Binary(ast::Operator),
    /// `op operand`; operands are `[operand]`.
    Unary(ast::UnaryOp),
    /// `left op right`; operands are `[left, right]`.
    Compare(ast::CmpOp),
    /// basedpython: `receiver.name` — an *attribute type*; operands are `[receiver]`.
    /// Unlike the arithmetic kinds this one is only ever built for a symbolic receiver,
    /// because that is the only receiver whose members cannot be resolved at definition
    /// time. It is what a type expression names as `T.a`, and also what a method member
    /// of a structural protocol reads back as, so that a call on it keeps its receiver.
    Attribute(Name),
    /// `callee(arg0, arg1, ...)`; operands are `[callee, arg0, arg1, ...]`. Covers
    /// method calls too — the callee is the (typevar-receiver) bound method.
    Call,
    /// basedpython: `F[arg0, arg1, ...]` where `F` is a `type def`; operands are
    /// `[Type::FunctionLiteral(F), arg0, arg1, ...]`. Unlike the other kinds this
    /// one cannot be reduced by evaluating against upper bounds — running the
    /// function is the only way to know its result — so its reduced form is the
    /// function's declared return type instead (see `DeferredType::reduced`).
    TypeFn,
    /// basedpython: `M[arg0, arg1, ...]` where `M` is a match type; operands are
    /// `[Type::KnownInstance(TypeAliasType(M)), arg0, arg1, ...]`, with `M` left
    /// unspecialized so that only the arguments are substituted. Which `case`
    /// applies is undecidable until the arguments are known, so this one has no
    /// reduced form at all (see `DeferredType::reduced`).
    MatchType,
}

impl DeferredOperation {
    /// basedpython: whether [`LinearForm`] takes this operation apart rather than treating
    /// it as an atom — which is also what makes it worth building at the value level.
    ///
    /// Flattening decides when two integer expressions name the same value, which covers
    /// `+`, `-`, `*` and the unary operators.
    pub(crate) const fn is_checked_arithmetic(&self) -> bool {
        matches!(
            self,
            DeferredOperation::Binary(
                ast::Operator::Add | ast::Operator::Sub | ast::Operator::Mult
            ) | DeferredOperation::Unary(
                ast::UnaryOp::USub | ast::UnaryOp::UAdd | ast::UnaryOp::Invert
            )
        )
    }

    /// basedpython: whether a body written against this operation is checked against the
    /// operation itself rather than against its reduced form.
    ///
    /// An arithmetic operation is decided by flattening; a call is decided by standing for
    /// itself, so the only source that names the same value is the very same call. The
    /// remaining kinds are deliberately weaker: an attribute type is *defined* to read as
    /// the bound's member before specialization, a `type def` reduces to its declared return
    /// type — the annotation its author wrote to make it checkable — and a match type
    /// reduces to a gradual type because which case applies is the whole question.
    pub(crate) const fn is_checked(&self) -> bool {
        self.is_checked_arithmetic() || matches!(self, DeferredOperation::Call)
    }

    /// The operands that decide whether the operation can be evaluated yet.
    ///
    /// For most kinds that is every operand. A [`DeferredOperation::MatchType`] carries the
    /// match type itself as its first operand, which is not something a specialization
    /// substitutes and must not keep the operation symbolic on its own.
    fn deferring_operands<'a, 'db>(&self, operands: &'a [Type<'db>]) -> &'a [Type<'db>] {
        match self {
            DeferredOperation::MatchType => operands.get(1..).unwrap_or_default(),
            _ => operands,
        }
    }
}

#[salsa::interned(debug, heap_size = ruff_memory_usage::heap_size)]
pub struct DeferredType<'db> {
    #[returns(ref)]
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
        operation: &DeferredOperation,
        operands: Box<[Type<'db>]>,
    ) -> Type<'db> {
        if operation
            .deferring_operands(&operands)
            .iter()
            .any(|operand| operand_is_symbolic(db, *operand))
        {
            return Type::Deferred(Self::new(db, operation.clone(), operands));
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
        // a `type def` cannot be reduced by substituting bounds: running the body is
        // the only way to learn its result, and running it against a bound would
        // answer a question nobody asked. its declared return type is the reduced
        // form, which is why annotating a type function is what makes generic code
        // using it checkable
        // a match type has no bound to substitute either: every case body is a different
        // type, and picking one is the whole question. an unresolved application is gradual
        if matches!(self.operation(db), DeferredOperation::MatchType) {
            return Type::unknown();
        }

        if matches!(self.operation(db), DeferredOperation::TypeFn) {
            let [Type::FunctionLiteral(function), ..] = self.operands(db) else {
                return Type::unknown();
            };
            return declared_return_type(db, *function).unwrap_or_else(Type::unknown);
        }

        let operands: Box<[Type<'db>]> = self
            .operands(db)
            .iter()
            .map(|operand| operand.reduce_deferred(db))
            .collect();

        // an attribute type is the one operation whose receiver has to be substituted rather
        // than merely reduced: looking the member up on the type parameter itself is what
        // *produces* this very deferral for a structural bound, so the reduction would answer
        // with the question. the bound is what the parameter stands for before specialization
        if let DeferredOperation::Attribute(name) = self.operation(db) {
            let [receiver] = &*operands else {
                return Type::unknown();
            };
            let receiver = match receiver.as_typevar() {
                Some(bound_typevar) => bound_typevar
                    .typevar(db)
                    .require_bound_or_constraints(db)
                    .as_type(db),
                None => *receiver,
            };
            return receiver
                .member(db, name)
                .ignore_possibly_undefined()
                .unwrap_or_else(Type::unknown);
        }

        evaluate(db, self.operation(db), &operands).unwrap_or_else(Type::unknown)
    }

    /// Re-evaluate the operation against operands to which a type-mapping has already
    /// been applied. Folds when the operands became concrete, stays symbolic while
    /// still unresolved.
    pub(crate) fn re_evaluate(self, db: &'db dyn Db, operands: Box<[Type<'db>]>) -> Type<'db> {
        Self::build(db, self.operation(db), operands)
    }

    /// basedpython: whether this deferral is an attribute type (`T.a`).
    pub(crate) fn is_attribute(self, db: &'db dyn Db) -> bool {
        matches!(self.operation(db), DeferredOperation::Attribute(_))
    }

    /// basedpython: see [`DeferredOperation::is_checked_arithmetic`].
    pub(crate) fn is_checked_arithmetic(self, db: &'db dyn Db) -> bool {
        self.operation(db).is_checked_arithmetic()
    }

    /// basedpython: see [`DeferredOperation::is_checked`].
    pub(crate) fn is_checked(self, db: &'db dyn Db) -> bool {
        self.operation(db).is_checked()
    }
}

/// basedpython: whether a value-level operand is one whose value a specialization decides.
///
/// Unlike [`DeferredType::is_deferred`] this looks only at the operand itself instead of
/// walking into it. An arithmetic operand is a scalar, so a type parameter buried inside one
/// — the `T` in `list[T]` — contributes no value to keep symbolic; staying shallow also keeps
/// this off the hot path of every `+` in a basedpython file.
pub(crate) const fn is_symbolic_operand(ty: Type<'_>) -> bool {
    matches!(ty, Type::TypeVar(_) | Type::Deferred(_))
}

/// basedpython: whether a value-level operand is an integer, and so has a [`LinearForm`] that
/// can be compared against a declared return type. A non-integer operand — `str`
/// concatenation, say — has no such form, so keeping its operation symbolic would buy a
/// weaker relation and nothing else.
pub(crate) fn is_integer_operand<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    ty.reduce_deferred(db)
        .is_subtype_of(db, KnownClass::Int.to_instance(db))
}

/// basedpython: an integer expression over type parameters flattened to
/// `constant + Σ coefficient × atom`.
///
/// A symbolic return type names one specific value per specialization, so a body must be
/// checked against the operation itself rather than against its reduced form — otherwise
/// `-> I + 1` accepts every `int`, including `i`. Two expressions that name the same value
/// need not be the same tree, though: `1 + i` differs from `i + 1` only in operand order,
/// and two applications of `-> I + 1` build `(I + 1) + 1` where the author wrote `I + 2`.
/// Flattening decides those agreements without needing a canonical form, which would have
/// to rank types against each other and make the answer depend on that ranking.
///
/// Only the operations [`DeferredOperation::is_checked_arithmetic`] admits are taken apart.
/// Anything else — a call, an attribute type, a bare type parameter — is an *atom*: it
/// stands for itself and agrees only with an identical type.
#[derive(Debug)]
pub(crate) struct LinearForm<'db> {
    constant: i64,
    /// A zero coefficient is dropped rather than stored, so that two forms are equal exactly
    /// when they agree on every term.
    terms: FxOrderMap<Type<'db>, i64>,
}

impl PartialEq for LinearForm<'_> {
    /// Two forms are compared term by term rather than in the order the terms happen to have
    /// been collected, so `A + B` and `B + A` are the same form. The map's own `Eq` would
    /// answer by insertion order, which is an artefact of the expression's shape — the very
    /// thing flattening exists to look past.
    fn eq(&self, other: &Self) -> bool {
        self.constant == other.constant
            && self.terms.len() == other.terms.len()
            && self
                .terms
                .iter()
                .all(|(atom, coefficient)| other.terms.get(atom) == Some(coefficient))
    }
}

impl Eq for LinearForm<'_> {}

impl<'db> LinearForm<'db> {
    fn constant(constant: i64) -> Self {
        Self {
            constant,
            terms: FxOrderMap::default(),
        }
    }

    fn atom(ty: Type<'db>) -> Self {
        Self {
            constant: 0,
            terms: std::iter::once((ty, 1)).collect(),
        }
    }

    fn as_constant(&self) -> Option<i64> {
        self.terms.is_empty().then_some(self.constant)
    }

    fn add(mut self, other: Self) -> Option<Self> {
        self.constant = self.constant.checked_add(other.constant)?;
        for (atom, coefficient) in other.terms {
            let combined = self
                .terms
                .get(&atom)
                .copied()
                .unwrap_or_default()
                .checked_add(coefficient)?;
            if combined == 0 {
                self.terms.remove(&atom);
            } else {
                self.terms.insert(atom, combined);
            }
        }
        Some(self)
    }

    fn scale(mut self, factor: i64) -> Option<Self> {
        if factor == 0 {
            return Some(Self::constant(0));
        }
        self.constant = self.constant.checked_mul(factor)?;
        for coefficient in self.terms.values_mut() {
            *coefficient = coefficient.checked_mul(factor)?;
        }
        Some(self)
    }

    fn negate(self) -> Option<Self> {
        self.scale(-1)
    }

    /// Flatten `ty`. `None` means the question cannot be decided — the arithmetic overflowed
    /// — rather than that the type has no form; the caller must not read that as disagreement.
    fn of(db: &'db dyn Db, ty: Type<'db>) -> Option<Self> {
        if let Some(value) = ty
            .as_literal_value()
            .and_then(super::literal::LiteralValueType::as_int)
        {
            return Some(Self::constant(value));
        }

        let Type::Deferred(deferred) = ty else {
            return Some(Self::atom(ty));
        };
        if !deferred.is_checked_arithmetic(db) {
            return Some(Self::atom(ty));
        }

        match (deferred.operation(db), deferred.operands(db)) {
            (DeferredOperation::Binary(op), [left, right]) => {
                let left = Self::of(db, *left)?;
                let right = Self::of(db, *right)?;
                match op {
                    ast::Operator::Add => left.add(right),
                    ast::Operator::Sub => left.add(right.negate()?),
                    // a product of two unknowns is not linear, so it stands for itself
                    ast::Operator::Mult => match (left.as_constant(), right.as_constant()) {
                        (Some(factor), _) => right.scale(factor),
                        (_, Some(factor)) => left.scale(factor),
                        _ => Some(Self::atom(ty)),
                    },
                    _ => Some(Self::atom(ty)),
                }
            }
            (DeferredOperation::Unary(op), [operand]) => match op {
                ast::UnaryOp::USub => Self::of(db, *operand)?.negate(),
                ast::UnaryOp::UAdd => Self::of(db, *operand),
                // `~I` is `-I - 1`, but only for an integer, and the operand's bound is not
                // consulted here. leaving it whole costs nothing: it still agrees with itself
                _ => Some(Self::atom(ty)),
            },
            _ => Some(Self::atom(ty)),
        }
    }

    /// Whether `source` and `target` name the same value. `None` when undecidable, in which
    /// case the caller falls back to the reduced relation rather than inventing an answer.
    pub(crate) fn same_value(
        db: &'db dyn Db,
        source: Type<'db>,
        target: Type<'db>,
    ) -> Option<bool> {
        Some(Self::of(db, source)? == Self::of(db, target)?)
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
    operation: &DeferredOperation,
    operands: &[Type<'db>],
) -> Option<Type<'db>> {
    match *operation {
        DeferredOperation::Binary(op) => {
            let [left, right] = operands else { return None };
            literal_binary_op(db, *left, *right, op, true)
                // the same tuple folds the value inferrer applies: without them
                // `(X,) * Dim` would re-evaluate through typeshed's `tuple.__mul__` and
                // widen to `tuple[X, ...]`, throwing away the length the fold just learned
                .or_else(|| match op {
                    ast::Operator::Mult => fold_tuple_repeat(db, *left, *right)
                        .or_else(|| fold_tuple_repeat(db, *right, *left)),
                    ast::Operator::Add => fold_tuple_concat(db, *left, *right),
                    _ => None,
                })
                .or_else(|| Type::try_call_bin_op_return_type(db, *left, op, *right))
        }
        DeferredOperation::Attribute(ref name) => {
            let [receiver] = operands else { return None };
            receiver.member(db, name).ignore_possibly_undefined()
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
        DeferredOperation::TypeFn => {
            let [Type::FunctionLiteral(function), arguments @ ..] = operands else {
                return None;
            };
            let arguments = TypeFnArguments::new(db, arguments.to_vec().into_boxed_slice());
            match evaluate_type_fn(db, *function, arguments) {
                TypeFnOutcome::Type(ty) => Some(*ty),
                // a re-evaluated application has no diagnostic sink — the error was
                // either already reported at a ground application site or belongs to a
                // specialization that cannot host a diagnostic. degrade to the
                // declared return rather than reporting nothing and inferring a type
                TypeFnOutcome::TypeError(_) | TypeFnOutcome::Failed(_) => None,
            }
        }
        DeferredOperation::MatchType => {
            let [
                Type::KnownInstance(KnownInstanceType::TypeAliasType(alias)),
                arguments @ ..,
            ] = operands
            else {
                return None;
            };
            let alias = alias.as_pep_695_type_alias()?;
            let specialized = alias.apply_specialization(db, |generic_context| {
                generic_context.specialize(db, arguments.to_vec())
            });
            match evaluate_match_type(db, specialized)? {
                MatchTypeOutcome::Matched(ty) => Some(*ty),
                // a specialization that decides no case has no value; `Unknown` keeps it
                // gradual rather than inventing one. the mismatch is reported where the
                // application is written
                MatchTypeOutcome::Unresolved
                | MatchTypeOutcome::NoCaseMatched
                | MatchTypeOutcome::TooLarge => None,
            }
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
