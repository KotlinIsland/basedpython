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
    IntersectionType, KnownClass, SubclassOfType, Type, UnionType, call::bind::Bindings,
    callable::CallableType,
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
