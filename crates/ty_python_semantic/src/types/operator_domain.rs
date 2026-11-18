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

use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ty_python_core::operator_dunders::hands_back_operands;
use ty_python_core::semantic_index;

use super::check_order::InferredInCheckOrder;
use super::function::{FunctionDecorators, OverloadLiteral};
use super::inferred_narrowing::is_empty_body;
use super::passed_operand::body_rejection;
use super::passed_operand::is_protocol_member;
use super::reified_infer::{ErasedTargetReason, ParametricIsPlan, type_test_plan};
use super::runtime_spelling::RuntimeSite;
use super::signatures::{CallableSignature, Parameter, ReturnCallableTypeVarScope};
use super::subclass_of::SubclassOfInner;
use super::typevar::{TypeVarBoundOrConstraints, TypeVarKind};
use super::{
    ClassLiteral, IntersectionType, KnownClass, SubclassOfType, Type, UnionType, binding_type,
    call::CallDunderError, call::bind::Bindings, callable::CallableType,
};
use crate::place::{RuntimeBinding, RuntimeRead, binds_one_class_wherever_bound, runtime_binding};
use crate::{Db, ProgramEnvironment};

impl<'db> Type<'db> {
    /// the runtime domain of a parameter annotated with this type: what a runtime check of the
    /// annotation can tell apart, which is the annotation with every type argument taken off
    ///
    /// it matches what `isinstance` narrows to: `list[int]` and `Iterable[T]` become every
    /// `list` and every `Iterable`, a literal stays the literal an equality check can test, and a
    /// type variable becomes the domain of what it stands for
    pub(crate) fn runtime_domain(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Self {
        self.domain(db, env, Erasure::Top)
    }

    /// the domain under `erasure`: [`Self::runtime_domain`] keeps every specialization of a
    /// class, and the test a guard makes names the class as a runtime test does
    fn domain(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>, erasure: Erasure) -> Self {
        let class = |class: ClassLiteral<'db>| match erasure {
            Erasure::Top => {
                Type::instance(db, env, class.top_materialization(db)).top_materialization(db, env)
            }
            Erasure::Bare => Type::instance(db, env, class.unknown_specialization(db)),
        };
        match self {
            Type::Union(union) => union.map(db, env, |element| element.domain(db, env, erasure)),
            // a negative element only ever shrinks the domain, and leaving it out keeps the
            // domain an over-approximation: an operand is only reported for being inside it
            Type::Intersection(intersection) => IntersectionType::from_elements(
                db,
                env,
                intersection
                    .positive_elements_or_object(db)
                    .map(|element| element.domain(db, env, erasure)),
            ),
            Type::NominalInstance(instance) => class(instance.class(db, env).class_literal(db)),
            Type::ProtocolInstance(protocol) => match protocol.class_origin(db) {
                Some(origin) => class(origin.class_literal(db)),
                None => match erasure {
                    Erasure::Top => self.top_materialization(db, env),
                    Erasure::Bare => self,
                },
            },
            Type::LiteralValue(literal) if literal.is_literal_string() => {
                KnownClass::Str.to_instance(db, env)
            }
            // the bound of a parameter written without a type is what the body happens to ask of
            // it, which nothing tests an operand against: the method runs with every operand
            Type::TypeVar(typevar)
                if typevar.typevar(db).kind(db) == TypeVarKind::InferredParameter =>
            {
                Type::object()
            }
            Type::TypeVar(typevar) => match typevar.typevar(db).bound_or_constraints(db, env) {
                Some(TypeVarBoundOrConstraints::UpperBound(bound)) => {
                    bound.domain(db, env, erasure)
                }
                Some(TypeVarBoundOrConstraints::Constraints(constraints)) => {
                    constraints.as_type(db, env).domain(db, env, erasure)
                }
                None => Type::object(),
            },
            Type::SubclassOf(subclass_of) => match subclass_of.subclass_of() {
                SubclassOfInner::Class(class) => {
                    let literal = class.class_literal(db);
                    SubclassOfType::from(
                        db,
                        env,
                        match erasure {
                            Erasure::Top => literal.top_materialization(db),
                            Erasure::Bare => literal.unknown_specialization(db),
                        },
                    )
                }
                SubclassOfInner::Dynamic(_)
                | SubclassOfInner::Protocol(_)
                | SubclassOfInner::TypeVar(_) => KnownClass::Type.to_instance(db, env),
            },
            Type::TypedDict(_) => match erasure {
                Erasure::Top => KnownClass::Dict
                    .to_instance(db, env)
                    .top_materialization(db, env),
                Erasure::Bare => KnownClass::Dict.to_instance_unknown(db, env),
            },
            Type::NewTypeInstance(newtype) => {
                newtype.concrete_base_type(db).domain(db, env, erasure)
            }
            Type::Callable(_) => match erasure {
                Erasure::Top => {
                    Type::Callable(CallableType::unknown(db)).top_materialization(db, env)
                }
                Erasure::Bare => Type::Callable(CallableType::unknown(db)),
            },
            Type::TypeAlias(alias) => alias.value_type(db).domain(db, env, erasure),
            Type::Overlapping(overlapping) => {
                overlapping.value_type(db, env).domain(db, env, erasure)
            }
            Type::Restricted(restricted) => restricted.value_type(db).domain(db, env, erasure),
            Type::Dynamic(_) => Type::object(),
            _ => self,
        }
    }
}

/// how a domain drops a class's type arguments
#[derive(Clone, Copy)]
enum Erasure {
    /// every specialization of the class, which an operand of any of them is assignable to
    Top,
    /// the class written bare, which is what a runtime test of it names
    Bare,
}

/// what a one-argument dunder call that failed does with the operand it failed on
pub(crate) enum OperandRejection<'db> {
    /// the operand is outside the domain whatever it is at runtime, so python moves on
    Whole,
    /// the operand may be outside the domain, and python moves on, or may be the `inside` part
    /// of it, which reaches the method and runs with it
    ///
    /// ```python
    /// class Wallet:
    ///     def __add__(self, other: Money) -> int: ...
    ///
    /// def f(w: Wallet, c: Coupon):
    ///     w + c  # a `class MoneyCoupon(Coupon, Money)` reaches `Wallet.__add__`
    /// ```
    Partly {
        inside: Type<'db>,
        /// the part that may be outside the domain, which python moves on with
        outside: Type<'db>,
    },
    /// the operand reaches the method whatever it is, so the call goes ahead with an operand it
    /// doesn't accept: a type error, with nothing to fall back to
    Reaches,
}

impl<'db> Bindings<'db> {
    /// what this failed one-argument dunder call does with `operand`
    ///
    /// each callee that failed is asked on its own: a receiver typed `int | float` calls
    /// `int.__add__` or `float.__add__`, never both, so `1.0` is rejected by the `int` that
    /// fails on it whatever the `float` would have done
    ///
    /// a type of the operand is outside a domain only when nothing can be both: a final class,
    /// a literal, or a class whose instances are laid out differently. two other classes may
    /// share a subclass, which is inside, where the callee reads it
    /// ([`reads_shared_subclasses`]). a union operand is split: of an `int | str` against an `int`
    /// domain, the `int` reaches the method and the `str` is handed back. of an `int | Box[str]`
    /// against a `Box[int]` parameter, the `Box[str]` reaches the method, which runs with it
    pub(crate) fn operand_rejection(
        &self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        operand: Type<'db>,
    ) -> OperandRejection<'db> {
        let operands = match operand {
            Type::Union(union) => union.elements(db).to_vec(),
            _ => vec![operand],
        };
        // the parts of the operand that reach the method, and the parts that may not
        let mut inside = Vec::new();
        let mut outside = Vec::new();
        for (callee, parameters) in self.failed_operand_parameters() {
            let Some(domain) = rejected_domain(db, env, callee, &parameters) else {
                return OperandRejection::Reaches;
            };
            let reads_shared_subclasses = reads_shared_subclasses(db, callee);
            for operand in &operands {
                if operand.is_assignable_to(db, env, domain) {
                    inside.push(*operand);
                    continue;
                }
                outside.push(*operand);
                if reads_shared_subclasses && !operand.is_disjoint_from(db, env, domain) {
                    inside.push(IntersectionType::from_two_elements(
                        db, env, *operand, domain,
                    ));
                }
            }
        }
        if outside.is_empty() {
            OperandRejection::Reaches
        } else if inside.is_empty() {
            OperandRejection::Whole
        } else {
            OperandRejection::Partly {
                inside: UnionType::from_elements(db, env, inside),
                outside: UnionType::from_elements(db, env, outside),
            }
        }
    }
}

/// whether an operand that may share a subclass with `callee`'s domain is read as possibly
/// inside it, which is what it is at runtime ([`OperandRejection::Partly`])
///
/// a `.by` dunder is, and so is one in a file whose operator dunders are checked
/// ([`checked-operator-operands`](crate::AnalysisSettings::checked_operator_operands)). elsewhere
/// an operand that isn't assignable to the domain is read as outside it, which is how ty reads
/// it
fn reads_shared_subclasses<'db>(db: &'db dyn Db, callee: Type<'db>) -> bool {
    let function = match callee {
        Type::BoundMethod(bound) => bound.function(db),
        Type::FunctionLiteral(function) => Some(function),
        _ => None,
    };
    function.is_some_and(|function| {
        let file = function.literal(db).last_definition.file(db);
        let source_type = file.source_type(db);
        (source_type.is_basedpython() && !source_type.is_stub())
            || db.analysis_settings(file).checked_operator_operands
    })
}

/// the domain a failed call to `callee` hands an operand back outside of: that of the
/// parameters the operand matched, as the function declares them. `None` for a callee that hands
/// nothing back, and for one whose overloads never matched the operand to a parameter, which
/// python calls and fails on whatever the operand is
///
/// the parameters are read as declared, not as bound to the receiver: in `Box[int].__add__` a
/// parameter annotated `T` is an `int`, but `T` is nothing a runtime test can see
fn rejected_domain<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    callee: Type<'db>,
    parameters: &[(Option<usize>, usize)],
) -> Option<Type<'db>> {
    let function = match callee {
        Type::BoundMethod(bound) => bound.function(db)?,
        Type::FunctionLiteral(function) => function,
        _ => return None,
    };
    let implementation = function.literal(db).last_definition;
    if parameters.is_empty() || !tests_its_operand(db, implementation) {
        return None;
    }
    let declared: Vec<_> = function.iter_overloads_and_implementation(db).collect();
    Some(UnionType::from_elements(
        db,
        env,
        parameters.iter().map(|(overload, index)| {
            overload
                .and_then(|overload| declared.get(overload))
                .copied()
                .unwrap_or(implementation)
                .parameter_signature(db, ReturnCallableTypeVarScope::Public)
                .parameters()
                .get(*index)
                .map_or(Type::object(), Parameter::annotated_type)
                .runtime_domain(db, env)
        }),
    ))
}

/// whether python's call to the operator dunder `implementation` hands an operand outside its
/// domain back with `NotImplemented`
///
/// - a coroutine or generator function never does: it hands back the coroutine or generator it
///   makes, whatever the operand
/// - a stub declares what the method does, and is taken at its word, as is a `.py` method when
///   the project doesn't check `.py` dunders
/// - otherwise the method has the test only where it is a plain `def` of an operator dunder, with
///   an operand parameter, in a class body: that is what a `.by` dunder is lowered with a test
///   for, and what a checked `.py` dunder is checked for. a function bound to a dunder under
///   another name, a `@staticmethod` or `@classmethod`, an operand gathered into `*args`, and a
///   method of an extension, which is called directly rather than dispatched to, have none
///
/// reading the operand parameter's signature can lead back here through a protocol whose own
/// dunders mention the class, so a cycle starts from no test and iterates to a fixpoint
#[salsa::tracked(cycle_initial = |_, _, _| false, heap_size = ruff_memory_usage::heap_size)]
pub(crate) fn tests_its_operand<'db>(
    db: &'db dyn Db,
    implementation: OverloadLiteral<'db>,
) -> bool {
    let program_file = implementation.program_file(db);
    let index = semantic_index(db, program_file);
    let body_scope = implementation.body_scope(db).file_scope_id(db);
    if implementation.header(db).is_async || body_scope.is_generator_function(index) {
        return false;
    }
    let file = implementation.file(db);
    let source_type = file.source_type(db);
    if source_type.is_stub()
        || (!source_type.is_basedpython() && !db.analysis_settings(file).checked_operator_operands)
    {
        return true;
    }
    if !hands_back_operands(implementation.name(db))
        || implementation.has_known_decorator(db, FunctionDecorators::STATICMETHOD)
        || implementation.has_known_decorator(db, FunctionDecorators::CLASSMETHOD)
        || implementation
            .parameter_signature(db, ReturnCallableTypeVarScope::Public)
            .parameters()
            .get_positional(1)
            .is_none()
    {
        return false;
    }
    let definition = implementation.definition(db);
    let Some(class) = index.scope(definition.file_scope(db)).node().as_class() else {
        return false;
    };
    let module = parsed_module(db, program_file.python_file(db)).load(db);
    !class.node(&module).is_extension()
}

impl<'db> Bindings<'db> {
    /// whether the dunder this call resolved to may still hand back `operand`, which it takes
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
    /// operation also has to be answered by whatever comes next. a method says this
    ///
    /// - with a return annotation that includes `NotImplementedType`
    /// - with a return type inferred from the body that is nothing else: whatever it takes, it
    ///   hands back
    ///
    /// a return type inferred from an unchecked body may include `NotImplementedType` only for the
    /// operands of the wrong class, which dispatch already moves past, so it says nothing more
    ///
    /// a checked `.py` dunder says it with a `return NotImplemented` reached for an operand inside
    /// the domain, which is then handed back only where it could be `operand` ([`body_rejection`]).
    /// its return type has to declare that too, but only the body says which operands, so the
    /// body is read, and the return type only where the body can't say: where a `return` reached
    /// with an operand inside the domain returns `Any`, say, and where the method only declares
    /// itself
    pub(crate) fn may_reject_by_value(
        &self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        operand: Type<'db>,
    ) -> bool {
        self.iter_flat().any(|binding| {
            let function = match binding.callable_type {
                Type::BoundMethod(bound) => bound.function(db),
                Type::FunctionLiteral(function) => Some(function),
                _ => None,
            };
            let Some(function) = function else {
                return false;
            };
            let implementation = function.literal(db).last_definition;
            let writes_return = implementation.header(db).writes_return;
            let declares = || {
                binding.matching_overloads().any(|(_, overload)| {
                    overload.return_ty.includes_not_implemented(db)
                        && (writes_return
                            || overload.return_ty.without_not_implemented(db).is_never())
                })
            };
            match body_rejection(db, implementation) {
                Some(body) => {
                    (!body.rejected.is_never() && !operand.is_disjoint_from(db, env, body.rejected))
                        || (body.opaque && declares())
                }
                None => declares(),
            }
        })
    }
}

impl<'db> Type<'db> {
    /// whether this is, or is a union that includes, `NotImplementedType`
    pub(crate) fn includes_not_implemented(self, db: &'db dyn Db) -> bool {
        match self {
            Type::Union(union) => union
                .elements(db)
                .iter()
                .any(|element| element.includes_not_implemented(db)),
            // `type Result = int | NotImplementedType`
            Type::TypeAlias(alias) => alias.value_type(db).includes_not_implemented(db),
            _ => self.is_notimplemented(db),
        }
    }

    /// the type an operation has when a dunder it dispatched to returned this: a
    /// `NotImplemented` the dunder hands back is the operation moving on, never its result
    pub(crate) fn without_not_implemented(self, db: &'db dyn Db) -> Type<'db> {
        match self {
            Type::TypeAlias(alias) if self.includes_not_implemented(db) => {
                alias.value_type(db).without_not_implemented(db)
            }
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
    /// the step a call of a dunder made with `operand`, from `call`, which calls the dunder with
    /// an operand and gives its answer and whether the method may still hand the operand back
    ///
    /// an operand that may be partly inside the domain reaches the method as that part, which
    /// the method answers, and may be outside it, which the method hands back
    pub(crate) fn of(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        operand: Type<'db>,
        call: impl Fn(Type<'db>) -> Result<(T, bool), CallDunderError<'db>>,
    ) -> Self {
        match call(operand) {
            Ok((answer, may_reject)) => DunderStep::Accepts { answer, may_reject },
            Err(error) => match error.operand_rejection(db, env, operand) {
                OperandRejection::Whole => DunderStep::Rejects,
                OperandRejection::Partly { inside, .. } => match call(inside) {
                    Ok((answer, _)) => DunderStep::Accepts {
                        answer,
                        may_reject: true,
                    },
                    Err(_) => DunderStep::NotAccepted(error),
                },
                OperandRejection::Reaches => DunderStep::NotAccepted(error),
            },
        }
    }
}

impl<'db> CallDunderError<'db> {
    /// what this failed operator dunder call does with `operand`: python moves past it, or the
    /// call goes ahead with an operand it shouldn't have been given
    ///
    /// a missing method rejects every operand. a method that is there rejects only an operand
    /// outside its parameter's runtime domain: one inside it reaches the method, which can't tell
    /// the operand's type arguments apart and runs with it, so the call is a type error and
    /// nothing falls back. see [`Type::runtime_domain`]
    fn operand_rejection(
        &self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        operand: Type<'db>,
    ) -> OperandRejection<'db> {
        match self {
            CallDunderError::MethodNotAvailable | CallDunderError::PossiblyUnbound { .. } => {
                OperandRejection::Whole
            }
            CallDunderError::CallError(_, bindings, _) => {
                bindings.operand_rejection(db, env, operand)
            }
        }
    }
}

impl<'db> CallableType<'db> {
    /// the signature `==` or `!=` sees in this `__eq__` or `__ne__`, bound to its receiver: what
    /// an override of one has to be compatible with
    ///
    /// ```python
    /// class Point:
    ///     def __eq__(self, other: Point) -> bool: ...
    /// ```
    ///
    /// python hands an `__eq__` every operand, and one outside its domain (or one it hands back by
    /// value, when its return type includes `NotImplementedType`) moves on: to the other operand's
    /// `__eq__`, and then to an identity test, which is a `bool`. so `Point.__eq__` compares with
    /// every operand, and is a `bool` whichever way it goes. this is why narrowing the operand
    /// is a valid override of `object.__eq__(self, other: object) -> bool`, which itself hands
    /// back every operand but its receiver. no other operator falls back to something with a type
    /// of its own: `<` with nothing that takes the operand is a `TypeError`
    ///
    /// `None` when the method takes every operand and never hands one back, so that `==` sees it
    /// as it is written
    pub(crate) fn as_compared_by_equality(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Option<CallableType<'db>> {
        let signatures = self.signatures(db);
        let operands = signatures
            .iter()
            .map(|signature| {
                signature
                    .parameters()
                    .get_positional(0)
                    .map(Parameter::annotated_type)
            })
            .collect::<Option<Vec<_>>>()?;
        let returns = UnionType::from_elements(
            db,
            env,
            signatures.iter().map(|signature| signature.return_ty),
        );
        let domain = UnionType::from_elements(
            db,
            env,
            operands
                .into_iter()
                .map(|operand| operand.runtime_domain(db, env)),
        );
        if Type::object().is_assignable_to(db, env, domain) && !returns.includes_not_implemented(db)
        {
            return None;
        }
        // an operand one overload hands back can reach any other answer, so each overload is
        // read as answering with what any of them might
        let return_ty = UnionType::from_elements(
            db,
            env,
            [
                returns.without_not_implemented(db),
                KnownClass::Bool.to_instance(db, env),
            ],
        );
        Some(self.with_signatures(
            db,
            CallableSignature::from_overloads(signatures.iter().map(|signature| {
                signature
                    .clone()
                    .with_parameter_type(0, Type::object())
                    .with_return_type(return_ty)
            })),
        ))
    }
}

/// basedpython: the test a `.by` operator dunder makes of its operand before its body runs,
/// handing back as `NotImplemented` an operand outside the domain of the parameter it is bound
/// to — which is where dispatch moves past it ([`Bindings::operand_rejection`])
///
/// ```by
/// class A:
///     def __add__(self, other: int) -> int:
///         ...
/// ```
///
/// is lowered with `if not isinstance(other, int): return NotImplemented` at the top of the body.
/// the body then sees what the annotation says it sees, and `A() + B()` reaches `B.__radd__` as
/// the checker says it does
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OperandGuard {
    /// the test, whose `False` is the operand handed back
    Test(ParametricIsPlan),
    /// no test the runtime can make tells an operand outside the domain apart
    Untestable(UntestableOperand),
}

/// why no guard can test an operand against a parameter's domain
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UntestableOperand {
    /// the runtime can't test a value against the domain at all
    Erased(ErasedTargetReason),
    /// the runtime test reads what a value records of its type, and one that records nothing
    /// answers `False` although it is inside the domain
    Inexact,
}

/// the guard `function` begins with, where it is a method of a `.by` class that dispatch may
/// move past, and its operand parameter has a domain some operand is outside of. `None` for a
/// method that takes every operand, and for one python never hands an operand back from
///
/// the domain is that of every overload's operand parameter together, since dispatch asks each
/// overload ([`Bindings::operand_rejection`]). an `@overload` itself never runs, and its
/// implementation makes the test
pub(crate) fn operand_guard<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    function: &ast::StmtFunctionDef,
    inferred: InferredInCheckOrder<'db>,
) -> Option<OperandGuard> {
    let file = inferred.file();
    let source_type = file.file(db).source_type(db);
    if !source_type.is_basedpython()
        || source_type.is_stub()
        || !hands_back_operands(function.name.as_str())
    {
        return None;
    }
    let index = semantic_index(db, file);
    let definition = index.try_definition(function)?;
    // a decorated dunder is called through whatever the decorator made, which a test written at
    // the top of this body doesn't guard
    let Type::FunctionLiteral(function_type) = binding_type(db, definition) else {
        return None;
    };
    let implementation = function_type.literal(db).last_definition;
    // an `@overload` never runs, and dispatch takes nothing else to test its operand. a method
    // a protocol or an `@abstractmethod` declares without implementing is never what python
    // calls. one with a body is what a class that inherits it runs
    if implementation.is_overload(db)
        || !*tests_its_operand(db, implementation)
        || ((is_protocol_member(db, implementation)
            || implementation.has_known_decorator(db, FunctionDecorators::ABSTRACT_METHOD))
            && is_empty_body(function))
    {
        return None;
    }
    // the receiver is positional parameter zero, and the operand the one after it
    let operand = UnionType::from_elements(
        db,
        env,
        function_type
            .signature(db)
            .overloads
            .iter()
            .map(|signature| {
                signature
                    .parameters()
                    .get_positional(1)
                    .map_or(Type::object(), Parameter::annotated_type)
            }),
    );
    operand_test(
        db,
        env,
        operand,
        RuntimeSite::body_of(db, function, inferred),
    )
}

/// the test an operand annotated `operand` is given where `site`, the top of the dunder's body,
/// runs. `None` where every operand is inside the domain
fn operand_test<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    operand: Type<'db>,
    site: RuntimeSite<'db>,
) -> Option<OperandGuard> {
    let file = site.inferred().file();
    let target = operand.domain(db, env, Erasure::Bare);
    // without `@runtime_checkable`, `isinstance` refuses a protocol. the test a protocol gets
    // then reads the annotations a value's class records, or, for one written bare, is the
    // `isinstance` python refuses
    if names_unchecked_protocol(db, target) {
        return Some(OperandGuard::Untestable(UntestableOperand::Inexact));
    }
    let plan = type_test_plan(db, env, file.file(db), Type::object(), target, None);
    match plan {
        ParametricIsPlan::Fold(true) => return None,
        ParametricIsPlan::ErasedTarget(reason) => {
            return Some(OperandGuard::Untestable(UntestableOperand::Erased(reason)));
        }
        _ if !plan.narrows_negatively() => {
            return Some(OperandGuard::Untestable(UntestableOperand::Inexact));
        }
        _ => {}
    }
    // the test is code of the body, so it reads each class as the body does, once it is called:
    // a class the module binds further down, and binds to nothing else, is bound by then
    for spelling in plan.spellings() {
        if let Some(name) = spelling.hidden_read(db, site) {
            return Some(OperandGuard::Untestable(UntestableOperand::Erased(
                ErasedTargetReason::NotAtRuntime(name, RuntimeBinding::Hidden),
            )));
        }
        for name in spelling.global_reads() {
            let binding = runtime_binding(
                db,
                env,
                RuntimeRead::Global {
                    name,
                    range: site.range(),
                },
                site.inferred(),
            );
            let bound = match binding {
                RuntimeBinding::Bound => true,
                RuntimeBinding::Unbound | RuntimeBinding::NotTheCheckers => {
                    binds_one_class_wherever_bound(db, file, name)
                }
                RuntimeBinding::Hidden => false,
            };
            if !bound {
                return Some(OperandGuard::Untestable(UntestableOperand::Erased(
                    ErasedTargetReason::NotAtRuntime(name.clone(), binding),
                )));
            }
        }
    }
    Some(OperandGuard::Test(plan))
}

/// whether `ty` is, or has an arm that is, a protocol without `@runtime_checkable`
fn names_unchecked_protocol<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    match ty {
        Type::Union(union) => union
            .elements(db)
            .iter()
            .any(|element| names_unchecked_protocol(db, *element)),
        Type::ProtocolInstance(protocol) => protocol
            .class_origin(db)
            .and_then(|origin| origin.into_protocol_class(db))
            .is_none_or(|class| !class.is_runtime_checkable(db)),
        _ => false,
    }
}
