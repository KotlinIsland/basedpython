//! basedpython: which operands an operator dunder inspects, and which it hands back
//!
//! python calls an operator dunder with whatever the other operand is. the method then either
//! *rejects* the operand by returning `NotImplemented`, and python moves on to the other
//! operand's reflected dunder (or to `__add__` after `__iadd__`), or it *accepts* the operand and
//! runs with it. what a method can tell apart at that point is only what a runtime check can see:
//! the operand's class, never its type arguments
//!
//! ```python
//! class Foo:
//!     def __iadd__(self, x: int) -> int: ...   # rejects a `str`: `foo += "a"` reaches `__add__`
//!
//! xs: list[int] = []
//! xs += ["a"]   # `list.__iadd__(Iterable[int])` can't see the `str`, it extends the list
//! ```
//!
//! so a parameter type describes two things. its *runtime domain* — the parameter with type
//! arguments taken off — is the set of operands the method accepts rather than rejects, and the
//! full type is what an accepted operand must be. an operand outside the domain is rejected; one
//! inside it that the call doesn't accept is a type error, with no fallback

use super::subclass_of::SubclassOfInner;
use super::typevar::TypeVarBoundOrConstraints;
use super::{
    IntersectionType, KnownClass, SubclassOfType, Type, UnionType, call::CallDunderError,
    call::bind::Bindings, callable::CallableType,
};
use crate::Db;
use crate::ProgramEnvironment;

impl<'db> Type<'db> {
    /// the runtime domain of a parameter annotated with this type: what a runtime check of the
    /// annotation can tell apart, which is the annotation with every type argument taken off
    ///
    /// it matches what `isinstance` narrows to: `list[int]` and `Iterable[T]` become every
    /// `list` and every `Iterable`, a literal stays the literal an equality check can test, and a
    /// type variable becomes the domain of what it stands for
    fn runtime_domain(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Self {
        match self {
            Type::Union(union) => union.map(db, env, |element| element.runtime_domain(db, env)),
            // a negative element only ever shrinks the domain, and leaving it out keeps the
            // domain an over-approximation: an operand is only reported for being inside it
            Type::Intersection(intersection) => IntersectionType::from_elements(
                db,
                env,
                intersection
                    .positive_elements_or_object(db)
                    .map(|element| element.runtime_domain(db, env)),
            ),
            Type::NominalInstance(instance) => Type::instance(
                db,
                env,
                instance
                    .class(db, env)
                    .class_literal(db)
                    .top_materialization(db),
            )
            .top_materialization(db, env),
            Type::ProtocolInstance(protocol) => match protocol.class_origin(db) {
                Some(origin) => {
                    Type::instance(db, env, origin.class_literal(db).top_materialization(db))
                        .top_materialization(db, env)
                }
                None => self.top_materialization(db, env),
            },
            Type::LiteralValue(literal) if literal.is_literal_string() => {
                KnownClass::Str.to_instance(db, env)
            }
            Type::TypeVar(typevar) => match typevar.typevar(db).bound_or_constraints(db, env) {
                Some(TypeVarBoundOrConstraints::UpperBound(bound)) => bound.runtime_domain(db, env),
                Some(TypeVarBoundOrConstraints::Constraints(constraints)) => {
                    constraints.as_type(db, env).runtime_domain(db, env)
                }
                None => Type::object(),
            },
            Type::SubclassOf(subclass_of) => match subclass_of.subclass_of() {
                SubclassOfInner::Class(class) => {
                    SubclassOfType::from(db, env, class.class_literal(db).top_materialization(db))
                }
                SubclassOfInner::Dynamic(_)
                | SubclassOfInner::Protocol(_)
                | SubclassOfInner::TypeVar(_) => KnownClass::Type.to_instance(db, env),
            },
            Type::TypedDict(_) => KnownClass::Dict
                .to_instance(db, env)
                .top_materialization(db, env),
            Type::NewTypeInstance(newtype) => {
                newtype.concrete_base_type(db).runtime_domain(db, env)
            }
            Type::Callable(_) => {
                Type::Callable(CallableType::unknown(db)).top_materialization(db, env)
            }
            Type::TypeAlias(alias) => alias.value_type(db).runtime_domain(db, env),
            Type::Overlapping(overlapping) => {
                overlapping.value_type(db, env).runtime_domain(db, env)
            }
            Type::Restricted(restricted) => restricted.value_type(db).runtime_domain(db, env),
            Type::Dynamic(_) => Type::object(),
            _ => self,
        }
    }
}

impl<'db> Bindings<'db> {
    /// whether a one-argument dunder call that failed rejected `operand`, so that python moves
    /// on, rather than going ahead with an operand it doesn't accept
    ///
    /// each callee that failed is asked on its own: a receiver typed `int | float` calls
    /// `int.__add__` or `float.__add__`, never both, so `1.0` is rejected by the `int` that
    /// fails on it whatever the `float` would have done. a callee whose overloads never matched
    /// the operand to a parameter has nothing a runtime check could have looked at, and rejects
    pub(crate) fn rejects_operand(
        &self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        operand: Type<'db>,
    ) -> bool {
        self.failed_operand_parameter_types().all(|parameters| {
            parameters.is_empty()
                || !operand.is_assignable_to(
                    db,
                    env,
                    UnionType::from_elements(
                        db,
                        env,
                        parameters
                            .iter()
                            .map(|parameter| parameter.runtime_domain(db, env)),
                    ),
                )
        })
    }
}

impl<'db> Bindings<'db> {
    /// whether the dunder this call resolved to may still hand back an operand it takes: the
    /// return annotation it wrote includes `NotImplementedType`
    ///
    /// ```python
    /// class A:
    ///     def __add__(self, other: int) -> int | NotImplementedType:
    ///         if other > 1:
    ///             return NotImplemented
    ///         return 0
    /// ```
    ///
    /// python then moves on exactly as it does for an operand outside the domain, so the
    /// operation also has to be answered by whatever comes next. only a written annotation says
    /// this: a return type inferred from the body also counts the `return NotImplemented` that
    /// rejects an operand of the wrong class, which dispatch already moves past
    pub(crate) fn may_reject_by_value(&self, db: &'db dyn Db) -> bool {
        self.iter_flat().any(|binding| {
            let function = match binding.callable_type {
                Type::BoundMethod(bound) => bound.function(db),
                Type::FunctionLiteral(function) => Some(function),
                _ => None,
            };
            let Some(function) = function else {
                return false;
            };
            function
                .literal(db)
                .last_definition
                .header(db)
                .writes_return
                && binding
                    .matching_overloads()
                    .any(|(_, overload)| overload.return_ty.includes_not_implemented(db))
        })
    }
}

impl<'db> Type<'db> {
    /// whether this is, or is a union that includes, `NotImplementedType`
    fn includes_not_implemented(self, db: &'db dyn Db) -> bool {
        match self {
            Type::Union(union) => union
                .elements(db)
                .iter()
                .any(|element| element.is_notimplemented(db)),
            _ => self.is_notimplemented(db),
        }
    }

    /// the type an operation has when a dunder it dispatched to returned this: a
    /// `NotImplemented` the dunder hands back is the operation moving on, never its result
    pub(crate) fn without_not_implemented(self, db: &'db dyn Db) -> Type<'db> {
        match self {
            Type::Union(union) => union.filter(db, |element| !element.is_notimplemented(db)),
            _ if self.is_notimplemented(db) => Type::Never,
            _ => self,
        }
    }
}

/// what one operator dunder does with the operand python hands it
pub(crate) enum DunderStep<'db, T> {
    /// the method takes the operand. `may_reject` when it may still hand it back; see
    /// [`Bindings::may_reject_by_value`]
    Accepts { answer: T, may_reject: bool },
    /// the method is missing, or rejects the operand's class
    Rejects,
    /// the method takes the operand but doesn't accept it, so the operation is a type error
    NotAccepted(CallDunderError<'db>),
}

/// the dunders that may answer an operation, and whether it may end with every one of them
/// having handed the operand back
pub(crate) struct Dispatch<T> {
    pub(crate) answers: Vec<T>,
    pub(crate) falls_through: bool,
}

impl<'db, T> DunderStep<'db, T> {
    /// whether python may move on from this step to the next
    fn declines(&self) -> bool {
        matches!(
            self,
            DunderStep::Rejects
                | DunderStep::Accepts {
                    may_reject: true,
                    ..
                }
        )
    }

    /// python asks `first`, and asks `second` only when `first` hands the operand back
    pub(crate) fn in_order(
        first: Self,
        second: impl FnOnce() -> Self,
    ) -> Result<Dispatch<T>, CallDunderError<'db>> {
        let mut answers = Vec::new();
        match first {
            DunderStep::NotAccepted(error) => return Err(error),
            DunderStep::Accepts { answer, may_reject } => {
                answers.push(answer);
                if !may_reject {
                    return Ok(Dispatch {
                        answers,
                        falls_through: false,
                    });
                }
            }
            DunderStep::Rejects => {}
        }
        match second() {
            DunderStep::NotAccepted(error) => Err(error),
            DunderStep::Accepts { answer, may_reject } => {
                answers.push(answer);
                Ok(Dispatch {
                    answers,
                    falls_through: may_reject,
                })
            }
            DunderStep::Rejects => Ok(Dispatch {
                answers,
                falls_through: true,
            }),
        }
    }

    /// python may ask either step first, because the operands' runtime classes decide it
    pub(crate) fn either_order(a: Self, b: Self) -> Result<Dispatch<T>, CallDunderError<'db>> {
        let falls_through = a.declines() && b.declines();
        let mut answers = Vec::new();
        for step in [a, b] {
            match step {
                DunderStep::NotAccepted(error) => return Err(error),
                DunderStep::Accepts { answer, .. } => answers.push(answer),
                DunderStep::Rejects => {}
            }
        }
        Ok(Dispatch {
            answers,
            falls_through,
        })
    }
}

impl<'db, T> DunderStep<'db, T> {
    /// the step a dunder call made with `operand`, from its answer and whether the method may
    /// still hand the operand back
    pub(crate) fn of(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        outcome: Result<(T, bool), CallDunderError<'db>>,
        operand: Type<'db>,
    ) -> Self {
        match outcome {
            Ok((answer, may_reject)) => DunderStep::Accepts { answer, may_reject },
            Err(error) if error.rejects(db, env, operand) => DunderStep::Rejects,
            Err(error) => DunderStep::NotAccepted(error),
        }
    }
}

impl<'db> CallDunderError<'db> {
    /// whether this failed operator dunder call is python moving past `operand`, rather than a
    /// call that goes ahead with an operand it shouldn't have been given
    ///
    /// a missing method rejects every operand. a method that is there rejects only an operand
    /// outside its parameter's runtime domain: one inside it reaches the method, which can't tell
    /// the operand's type arguments apart and runs with it, so the call is a type error and
    /// nothing falls back. see [`Type::runtime_domain`]
    fn rejects(
        &self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        operand: Type<'db>,
    ) -> bool {
        match self {
            CallDunderError::MethodNotAvailable | CallDunderError::PossiblyUnbound { .. } => true,
            CallDunderError::CallError(_, bindings, _) => {
                bindings.rejects_operand(db, env, operand)
            }
        }
    }
}
