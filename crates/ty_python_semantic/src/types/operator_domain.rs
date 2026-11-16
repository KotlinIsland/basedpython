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

use ruff_python_ast as ast;
use ty_python_core::semantic_index;

use super::check_order::InferredInCheckOrder;
use super::reified_infer::{ErasedTargetReason, ParametricIsPlan, type_test_plan};
use super::runtime_spelling::RuntimeSite;
use super::signatures::Parameter;
use super::subclass_of::SubclassOfInner;
use super::typevar::{TypeVarBoundOrConstraints, TypeVarKind};
use super::{
    ClassLiteral, IntersectionType, KnownClass, SubclassOfType, Type, UnionType, binding_type,
    call::CallDunderError, call::bind::Bindings, callable::CallableType, infer_definition_types,
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
    fn runtime_domain(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Self {
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
            // the bound of a `.by` parameter written without a type is what the body happens to
            // ask of it, which no test is written from: the method runs with every operand
            Type::TypeVar(typevar)
                if typevar.typevar(db).kind(db) == TypeVarKind::InferredParameter
                    && typevar
                        .typevar(db)
                        .definition(db)
                        .is_some_and(|definition| {
                            definition.file(db).source_type(db).is_basedpython()
                        }) =>
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
    fn rejects(&self, db: &'db dyn Db, env: &ProgramEnvironment<'db>, operand: Type<'db>) -> bool {
        match self {
            CallDunderError::MethodNotAvailable | CallDunderError::PossiblyUnbound { .. } => true,
            CallDunderError::CallError(_, bindings, _) => {
                bindings.rejects_operand(db, env, operand)
            }
        }
    }
}

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
pub(crate) fn hands_back_operands(name: &str) -> bool {
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

/// basedpython: the test a `.by` operator dunder makes of its operand before its body runs,
/// handing back as `NotImplemented` an operand outside the domain of the parameter it is bound
/// to — which is where dispatch moves past it ([`Bindings::rejects_operand`])
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
/// overload ([`Bindings::rejects_operand`]). an `@overload` itself never runs, and its
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
    index.scope(definition.file_scope(db)).node().as_class()?;
    let function_type = match binding_type(db, definition) {
        Type::FunctionLiteral(function_type) => function_type,
        _ => infer_definition_types(db, definition)
            .undecorated_type()?
            .as_function_literal()?,
    };
    if function_type.literal(db).last_definition.is_overload(db)
        || function_type.is_staticmethod(db)
        || function_type.is_classmethod(db)
    {
        return None;
    }
    // the receiver is positional parameter zero, and the operand the one after it. a method with
    // none there gathers the operand into `*args`, which takes anything
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
