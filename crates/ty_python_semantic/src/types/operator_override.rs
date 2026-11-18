//! basedpython: an operator dunder a subclass has, which code typed against its ancestor doesn't
//! expect python to ask
//!
//! # a reflected dunder asked first
//!
//! when the right operand's class is a subclass of the left operand's, python asks the right
//! operand first: for a binary operator where the subclass answers the reflected dunder
//! differently, and for a comparison whatever it answers it with
//!
//! ```python
//! class A:
//!     def __add__(self, other: object) -> int: ...
//!
//! class B(A):
//!     def __radd__(self, other: object) -> str: ...
//!
//! def f(a1: A, a2: A):
//!     a1 + a2  # `int` by `A.__add__`, but `B.__radd__` answers it first when `a2` is a `B`
//! ```
//!
//! code typed against `A` can't see that coming, so a class may answer a reflected dunder
//! differently from an ancestor only with what that ancestor's own dunder answers: `B.__radd__`
//! has to return an `int`, or hand the operand back with `NotImplemented`, after which python
//! asks `A.__add__`
//!
//! # a dunder that takes what the ancestor's hands back
//!
//! an operand an ancestor's dunder hands back is answered by whatever python asks next, which is
//! how dispatch through the ancestor types it. a subclass's dunder that takes that operand answers
//! it instead
//!
//! ```python
//! class A:
//!     def __add__(self, other: int) -> int: ...
//!
//! class Wide(A):
//!     def __add__(self, other: object) -> int: ...  # a valid override, but `A() + s` changes
//! ```
//!
//! so an operator dunder may take only operands the ancestor's takes
//!
//! # an in-place dunder the ancestor doesn't have
//!
//! `x += y` asks `x`'s `__iadd__`, and only without one `x.__add__`
//!
//! ```python
//! class Foo:
//!     def __add__(self, x: str) -> str: ...
//!
//! class Bar(Foo):
//!     def __iadd__(self, x: object) -> int: ...
//!
//! def f(foo: Foo):
//!     foo += "a"  # `str` by `Foo.__add__`, but `Bar.__iadd__` answers it when `foo` is a `Bar`
//! ```
//!
//! so an in-place dunder a class adds to an ancestor that has none takes only operands the
//! ancestor's binary dunder takes, and answers them as it does. one it hands back reaches the
//! class's own binary dunder, which is held to the ancestor's as any override is
//!
//! the stdlib's own classes are taken at their word: `MutableSet.__ior__` answers what `Set.__or__`
//! would differently, and a class that inherits it isn't reported for it

use ruff_python_ast as ast;
use ruff_python_ast::helpers::TypeModifier;
use ruff_text_size::{Ranged, TextRange};

use super::call::CallArguments;
use super::callable::CallableTypes;
use super::class::{ClassType, StaticClassLiteral};
use super::class_base::ClassBase;
use super::context::InferContext;
use super::diagnostic::INVALID_OPERATOR_OVERRIDE;
use super::function::FunctionType;
use super::operator_domain::{DunderStep, tests_its_operand};
use super::relation_error::ErrorContextTree;
use super::restricted::RestrictedType;
use super::signatures::{CallableSignature, Parameter, Signature};
use super::typevar::{BindingContext, BoundTypeVarInstance, SelfTypeVarOrigin};
use super::{IntersectionType, Type, TypeContext, UnionType};
use crate::place::Place;
use crate::{Db, ProgramEnvironment};

/// the operators whose right operand python may ask first, each as its dunder, the reflected
/// dunder python asks of the right operand, and how the operator is written. `__eq__` and `__ne__`
/// are their own reflections, and an override of them is checked as an override
const REFLECTED_OPERATORS: [(&str, &str, &str); 17] = [
    ("__add__", "__radd__", "+"),
    ("__sub__", "__rsub__", "-"),
    ("__mul__", "__rmul__", "*"),
    ("__matmul__", "__rmatmul__", "@"),
    ("__truediv__", "__rtruediv__", "/"),
    ("__floordiv__", "__rfloordiv__", "//"),
    ("__mod__", "__rmod__", "%"),
    ("__pow__", "__rpow__", "**"),
    ("__lshift__", "__rlshift__", "<<"),
    ("__rshift__", "__rrshift__", ">>"),
    ("__and__", "__rand__", "&"),
    ("__xor__", "__rxor__", "^"),
    ("__or__", "__ror__", "|"),
    ("__lt__", "__gt__", "<"),
    ("__le__", "__ge__", "<="),
    ("__gt__", "__lt__", ">"),
    ("__ge__", "__le__", ">="),
];

/// the in-place operators, each as its dunder, the binary dunder python asks without it, and how
/// the operator is written
const IN_PLACE_OPERATORS: [(&str, &str, &str); 13] = [
    ("__iadd__", "__add__", "+="),
    ("__isub__", "__sub__", "-="),
    ("__imul__", "__mul__", "*="),
    ("__imatmul__", "__matmul__", "@="),
    ("__itruediv__", "__truediv__", "/="),
    ("__ifloordiv__", "__floordiv__", "//="),
    ("__imod__", "__mod__", "%="),
    ("__ipow__", "__pow__", "**="),
    ("__ilshift__", "__lshift__", "<<="),
    ("__irshift__", "__rshift__", ">>="),
    ("__iand__", "__and__", "&="),
    ("__ixor__", "__xor__", "^="),
    ("__ior__", "__or__", "|="),
];

/// report each operator dunder of `class` that python asks where code typed against an ancestor
/// expects another dunder, or expects an operand handed back, and that answers differently
///
/// the class may have the dunder from a base rather than its own body: in `class C(B, A)`,
/// `B.__radd__` answers `A() + C()` before `A.__add__`, though `B` alone has no `A` to conform
/// to. where the class that defines the dunder already has the ancestor, that class is where it
/// is reported, so its subclasses aren't
pub(crate) fn check_operator_overrides<'db>(
    context: &InferContext<'db, '_>,
    class: StaticClassLiteral<'db>,
    class_node: &ast::StmtClassDef,
) {
    let db = context.db();
    // the check belongs with the other operator checks: a `.by` class always has it, and a `.py`
    // class where its file's operator dunders are checked. elsewhere a `.py` class is read as ty
    // reads it, which asks nothing of an operator dunder beyond an ordinary override
    let file = context.file();
    if !context.is_lint_enabled(&INVALID_OPERATOR_OVERRIDE)
        || !(file.source_type(db).is_basedpython()
            || db.analysis_settings(file).checked_operator_operands)
    {
        return;
    }
    let ancestry = Ancestry::new(context, class, class_node);
    for (dunder, reflected_dunder, symbol) in REFLECTED_OPERATORS {
        ancestry.check_reflected(dunder, reflected_dunder, symbol);
    }
    // every operator dunder whose override python may ask in the ancestor's place, `__eq__` and
    // `__ne__` aside: their own override rule lets them take fewer operands, and what they hand
    // back ends at an identity test. a comparison's reflection is another comparison
    let reflected_only = REFLECTED_OPERATORS
        .iter()
        .map(|(_, reflected_dunder, _)| *reflected_dunder)
        .filter(|reflected_dunder| {
            !REFLECTED_OPERATORS
                .iter()
                .any(|(dunder, _, _)| dunder == reflected_dunder)
        });
    for name in REFLECTED_OPERATORS
        .iter()
        .map(|(dunder, _, _)| *dunder)
        .chain(reflected_only)
    {
        ancestry.check_slot(name, None);
    }
    for (in_place, dunder, symbol) in IN_PLACE_OPERATORS {
        ancestry.check_slot(in_place, Some((dunder, symbol)));
    }
}

/// the class being checked, as its own body sees it, with the classes it inherits from
struct Ancestry<'db, 'ctx, 'ast> {
    context: &'ctx InferContext<'db, 'ast>,
    class: StaticClassLiteral<'db>,
    class_node: &'ctx ast::StmtClassDef,
    /// the class with its type parameters left as themselves: `B[T]`, whose ancestors are `A[T]`,
    /// rather than `B[Unknown]`, whose `Unknown` answers anything
    class_type: ClassType<'db>,
    instance: Type<'db>,
    /// `Self` in the class's methods: whatever class a value of this class is, which for a final
    /// class is the class
    self_type: Type<'db>,
    /// the class's method resolution order, the class first
    mro: Vec<ClassType<'db>>,
}

impl<'db, 'ctx, 'ast> Ancestry<'db, 'ctx, 'ast> {
    fn new(
        context: &'ctx InferContext<'db, 'ast>,
        class: StaticClassLiteral<'db>,
        class_node: &'ctx ast::StmtClassDef,
    ) -> Self {
        let db = context.db();
        let class_type = class.identity_specialization(db);
        let env = context.program_environment();
        let instance = Type::instance(db, env, class_type);
        // a final class has no subclass for `Self` to be
        let self_type = if class_type.is_final(db) {
            instance
        } else {
            Type::TypeVar(BoundTypeVarInstance::synthetic_self(
                db,
                SelfTypeVarOrigin::Class(class.into()),
                instance,
                BindingContext::Synthetic(env.program(db)),
            ))
        };
        let mro = classes_of(db, class_type);
        Self {
            context,
            class,
            class_node,
            class_type,
            instance,
            self_type,
            mro,
        }
    }

    fn member(&self, class: ClassType<'db>, name: &str) -> Place<'db> {
        Type::from(class)
            .member(self.context.db(), self.context.program_environment(), name)
            .place
    }

    /// the furthest class `class` has `name` from, without a class in between answering it
    /// differently
    fn definer(&self, class: ClassType<'db>, name: &str) -> ClassType<'db> {
        let place = self.member(class, name);
        classes_of(self.context.db(), class)
            .into_iter()
            .take_while(|ancestor| {
                self.member(*ancestor, name)
                    .is_equal_ignoring_provenance(place)
            })
            .last()
            .unwrap_or(class)
    }

    /// where this class has its `name`, `place`, from a base that has it too and already has
    /// `ancestor`, that base is reported for it rather than this class
    fn answered_by_base(&self, name: &str, place: Place<'db>, ancestor: ClassType<'db>) -> bool {
        let db = self.context.db();
        self.mro.iter().skip(1).any(|base| {
            self.member(*base, name).is_equal_ignoring_provenance(place)
                && classes_of(db, *base)
                    .iter()
                    .any(|class| class.class_literal(db) == ancestor.class_literal(db))
        })
    }

    /// where to report `name`: its definition in the class body, or the class header when the
    /// class has it from a base
    fn range(&self, definer: ClassType<'db>, name: &str) -> TextRange {
        let db = self.context.db();
        if definer == self.class_type {
            own_definition(self.class_node, name).unwrap_or_else(|| self.class.header_range(db))
        } else {
            self.class.header_range(db)
        }
    }

    /// report `reflected_dunder` where python asks it before an ancestor's `dunder`, and it
    /// answers differently
    fn check_reflected(&self, dunder: &str, reflected_dunder: &str, symbol: &str) {
        let db = self.context.db();
        let env = self.context.program_environment();
        let reflected = self.member(self.class_type, reflected_dunder);
        if reflected.is_undefined() {
            return;
        }
        // python asks a comparison's reflected dunder first for any subclass, even one that
        // inherits it. a binary operator's, only where the subclass answers it differently
        let is_comparison = matches!(dunder, "__lt__" | "__le__" | "__gt__" | "__ge__");
        let definer = self.definer(self.class_type, reflected_dunder);
        // the ancestors first, so that a class whose reflected comparison disagrees with an
        // ancestor's is told so by name, and the class's own pair of comparisons last
        let order = self
            .mro
            .iter()
            .enumerate()
            .skip(1)
            .chain(self.mro.iter().enumerate().take(1));
        for (index, ancestor) in order {
            if index == 0 {
                // a class whose own two comparisons disagree is wrong as soon as anything
                // subclasses it. every comparison is another's reflection, so the reflected
                // dunder the class defines covers a disagreement either way round
                if !is_comparison || self.class_type.is_final(db) || definer != self.class_type {
                    continue;
                }
            } else if (!is_comparison
                && self
                    .member(*ancestor, reflected_dunder)
                    .is_equal_ignoring_provenance(reflected))
                || self.answered_by_base(reflected_dunder, reflected, *ancestor)
            {
                continue;
            }
            let Some(wrong) =
                reflected_conformance(db, env, *ancestor, self.instance, dunder, reflected_dunder)
            else {
                continue;
            };
            let Some(builder) = self.context.report_lint(
                &INVALID_OPERATOR_OVERRIDE,
                self.range(definer, reflected_dunder),
            ) else {
                return;
            };
            let class_name = self.class.name(db);
            let ancestor_name = ancestor.name(db);
            let dunder_definer = self.definer(*ancestor, dunder).name(db);
            // `A() + B()`, or for a class asked about its own subclasses, `A() < x`
            let operation = if index == 0 {
                format!("`{ancestor_name}() {symbol} x`, for `x` a subclass,")
            } else {
                format!("`{ancestor_name}() {symbol} {class_name}()`")
            };
            let mut diagnostic = match wrong {
                Wrong::Answers { found, expected } => builder.into_diagnostic(format_args!(
                    "`{class_name}.{reflected_dunder}` answers {operation} with `{}`, where \
                     `{dunder_definer}.{dunder}` answers `{}`",
                    found.display(db, env),
                    expected.display(db, env),
                )),
                Wrong::NotAccepted { operand } => builder.into_diagnostic(format_args!(
                    "`{class_name}.{reflected_dunder}` takes `{}` without accepting it, for \
                     {operation}",
                    operand.display(db, env),
                )),
            };
            if index == 0 {
                diagnostic.info(format_args!(
                    "python asks a subclass's `{reflected_dunder}` before `{dunder}` even when \
                     it inherits it, so it has to answer as `{class_name}.{dunder}` does, unless \
                     `{class_name}` is `@final`",
                ));
            } else {
                diagnostic.info(format_args!(
                    "python asks a subclass's `{reflected_dunder}` before \
                     `{dunder_definer}.{dunder}`, so for a left operand of exactly \
                     `{ancestor_name}` it has to answer as `{dunder_definer}.{dunder}` does, or \
                     hand the operand back",
                ));
            }
            // one ancestor is enough to show the reflected dunder is wrong
            return;
        }
    }

    /// report `name` where an ancestor answers its slot with another dunder, and `name` takes an
    /// operand that dunder hands back, or answers one differently
    ///
    /// the ancestor's slot is its own `name`, or for an in-place dunder it doesn't have, the
    /// binary dunder `in_place` names, written with `in_place`'s symbol. code typed against any
    /// ancestor may be handed a value of this class, so each one is asked, except where an
    /// ancestor whose own `name` this class overrides has it: that ancestor is held to its own
    /// ancestors where it is checked, and taken at its word where it isn't, as a stdlib class is
    fn check_slot(&self, name: &str, in_place: Option<(&str, &str)>) {
        let db = self.context.db();
        let env = self.context.program_environment();
        let own = self.member(self.class_type, name);
        if own.is_undefined() {
            return;
        }
        let definer = self.definer(self.class_type, name);
        let taken = operand_domain(db, env, self.instance, name);
        // the method resolution orders of the ancestors whose own `name` this class overrides
        let mut overridden: Vec<Vec<ClassType<'db>>> = Vec::new();
        for ancestor in self.mro.iter().skip(1) {
            if overridden
                .iter()
                .flatten()
                .any(|class| class.class_literal(db) == ancestor.class_literal(db))
            {
                continue;
            }
            let ancestors_own = self.member(*ancestor, name);
            let slot = if !ancestors_own.is_undefined() {
                if ancestors_own.is_equal_ignoring_provenance(own) {
                    continue;
                }
                overridden.push(classes_of(db, *ancestor));
                name
            } else if let Some((dunder, _)) = in_place {
                dunder
            } else {
                // an ancestor without the dunder at all is answered by the other operand, which
                // no definition here can see
                continue;
            };
            if self.answered_by_base(name, own, *ancestor) {
                continue;
            }
            // an ancestor without the binary dunder either, a mixin say, answers nothing here
            if self.member(*ancestor, slot).is_undefined() {
                continue;
            }
            let receiver = Type::instance(db, env, *ancestor);
            let expected = operand_domain(db, env, receiver, slot);
            // an override of the same dunder is held to its answers by the override check
            let wrong = if !taken.is_assignable_to(db, env, expected) {
                SlotWrong::Wider
            } else if slot != name
                && let Some(unfit) = self.in_place_unfit(receiver, slot, name, taken)
            {
                unfit
            } else {
                continue;
            };
            let Some(builder) = self
                .context
                .report_lint(&INVALID_OPERATOR_OVERRIDE, self.range(definer, name))
            else {
                return;
            };
            let class_name = self.class.name(db);
            let ancestor_name = ancestor.name(db);
            let slot_definer = self.definer(*ancestor, slot).name(db);
            let mut diagnostic = match &wrong {
                SlotWrong::Wider => builder.into_diagnostic(format_args!(
                    "`{class_name}.{name}` takes {}, more than `{slot_definer}.{slot}` takes \
                     (`{}`)",
                    if takes_every_operand(db, env, taken) {
                        "every operand".to_string()
                    } else {
                        format!("`{}`", display_domain(db, env, taken))
                    },
                    display_domain(db, env, expected),
                )),
                SlotWrong::Unfit { own, replaced, .. } => builder.into_diagnostic(format_args!(
                    "`{class_name}.{name}` is `{}`, which doesn't answer the operands it takes as \
                     `{slot_definer}.{slot}` (`{}`) does",
                    own.display(db, env),
                    replaced.display(db, env),
                )),
            };
            match in_place {
                Some((_, symbol)) if slot != name => diagnostic.info(format_args!(
                    "`x {symbol} y` with `x: {ancestor_name}` is `{slot_definer}.{slot}`, but for \
                     `x` of class `{class_name}` it is `{class_name}.{name}`, so that may take only \
                     what `{slot_definer}.{slot}` takes, and answer as it does",
                )),
                _ => diagnostic.info(format_args!(
                    "an operand `{slot_definer}.{slot}` hands back is answered by what python asks \
                     next, but for a receiver of class `{class_name}` by `{class_name}.{name}`",
                )),
            }
            if let SlotWrong::Unfit { error_context, .. } = wrong {
                error_context.attach_to(db, env, &mut diagnostic);
            }
            // one ancestor is enough to show the dunder is wrong
            return;
        }
    }

    /// how this class's in-place dunder `name` fails `receiver`'s binary `slot`, which code typed
    /// against the receiver's class has `x += y` ask in its place
    ///
    /// python asks `name` with every operand in `taken`, so of those it has to accept each one
    /// `slot` accepts, and answer it as `slot` does. an operand it hands back reaches the class's
    /// own binary dunder, which answers as `slot` does where the override check holds it to that,
    /// so `name` may take fewer operands, and may narrow `Self`
    ///
    /// the two are compared as callables, which is how the override check compares methods:
    /// overloads, type parameters and parameters without annotations are read as it reads them.
    /// python passes the operand positionally, so its name and kind don't matter
    fn in_place_unfit(
        &self,
        receiver: Type<'db>,
        slot: &str,
        name: &str,
        taken: Type<'db>,
    ) -> Option<SlotWrong<'db>> {
        let db = self.context.db();
        let env = self.context.program_environment();
        // `owner`'s `name`, with `Self` as `receiver`
        let bound = |owner: Type<'db>, name: &str, self_type: Type<'db>| match owner
            .member(db, env, name)
            .place
            .ignore_possibly_undefined()?
        {
            Type::BoundMethod(method) => {
                method.callables_with_receiver(db, env, self_type, self_type)
            }
            member => member.try_upcast_to_callable(db, env),
        };
        let own = bound(self.instance, name, self.self_type)?;
        // an overload python can't call with one operand stays as it is, which nothing fits
        let own_answers = as_answered(db, &own, |signature| {
            Some(
                signature
                    .as_called_with_operand()
                    .unwrap_or_else(|| signature.clone()),
            )
        })?
        .to_type(db, env);
        // code typed against the ancestor has its `Self` as the ancestor, and code typed with a
        // type variable bound to it, or inside one of its methods, as whatever class the value is
        for receiver_self in [receiver, self.self_type] {
            let replaced = bound(receiver, slot, receiver_self)?;
            // an overload python can't call with one operand never answers `x += y`
            let Some(called) = as_answered(db, &replaced, Signature::as_called_with_operand) else {
                continue;
            };
            let Some(restricted) = restrict_operand(db, env, &called, taken) else {
                continue;
            };
            let restricted = restricted.to_type(db, env);
            if !own_answers.is_assignable_to(db, env, restricted) {
                return Some(SlotWrong::Unfit {
                    own: own.to_type(db, env),
                    replaced: replaced.to_type(db, env),
                    error_context: own_answers.assignability_error_context(db, env, restricted),
                });
            }
        }
        None
    }
}

/// the classes in `class`'s method resolution order, `class` first
fn classes_of<'db>(db: &'db dyn Db, class: ClassType<'db>) -> Vec<ClassType<'db>> {
    class
        .iter_mro(db)
        .filter_map(|base| match base {
            ClassBase::Class(ancestor) => Some(ancestor),
            _ => None,
        })
        .collect()
}

/// how a reflected dunder fails an ancestor's dunder
enum Wrong<'db> {
    /// it answers with `found`, which the ancestor's dunder doesn't
    Answers {
        found: Type<'db>,
        expected: Type<'db>,
    },
    /// it takes `operand` but doesn't accept it, and runs with it anyway
    NotAccepted { operand: Type<'db> },
}

/// how `instance`'s `reflected_dunder` fails `ancestor`'s `dunder`, asked first with an operand
/// whose class is exactly `ancestor`, if it does
fn reflected_conformance<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ancestor: ClassType<'db>,
    instance: Type<'db>,
    dunder: &str,
    reflected_dunder: &str,
) -> Option<Wrong<'db>> {
    // the left operand is exactly the ancestor: python asks the right operand first only when
    // its class is a subclass of the left operand's
    let left = RestrictedType::from_type_expression(
        db,
        env,
        TypeModifier::Final,
        Type::instance(db, env, ancestor),
    );
    let expected = left
        .try_call_dunder(
            db,
            env,
            dunder,
            CallArguments::positional([instance]),
            TypeContext::default(),
        )
        .ok()?
        .return_type(db, env)
        .without_not_implemented(db);
    match call_step(db, env, instance, reflected_dunder, left) {
        DunderStep::Rejects => None,
        DunderStep::Accepts { answer, .. } => {
            let found = answer.without_not_implemented(db);
            (!found.is_assignable_to(db, env, expected))
                .then_some(Wrong::Answers { found, expected })
        }
        // only a plain method is held to handing back an operand it doesn't take: a comparison
        // a dataclass writes, say, hands back an operand of another class, whatever its type says
        DunderStep::NotAccepted(_) => tested_dunder(db, env, instance, reflected_dunder)
            .is_some()
            .then_some(Wrong::NotAccepted { operand: left }),
    }
}

/// how a dunder fails the ancestor's dunder it answers in place of
enum SlotWrong<'db> {
    /// it takes operands the ancestor's dunder hands back
    Wider,
    /// as a callable, restricted to the operands python asks it with, it doesn't fit `replaced`
    Unfit {
        own: Type<'db>,
        replaced: Type<'db>,
        error_context: ErrorContextTree<'db>,
    },
}

/// `callables` read as `x += y` calls them, each overload as `call` reads it, and answering
/// only what it doesn't hand back with `NotImplemented`, which is answered by what python asks
/// next. `None` where `call` leaves no overload
fn as_answered<'db>(
    db: &'db dyn Db,
    callables: &CallableTypes<'db>,
    call: impl Fn(&Signature<'db>) -> Option<Signature<'db>>,
) -> Option<CallableTypes<'db>> {
    let mut answered = Vec::new();
    for callable in callables {
        let overloads: Vec<_> = callable
            .signatures(db)
            .iter()
            .filter_map(&call)
            .map(|signature| {
                let return_ty = signature.return_ty.without_not_implemented(db);
                signature.with_return_type(return_ty)
            })
            .collect();
        if !overloads.is_empty() {
            answered
                .push(callable.with_signatures(db, CallableSignature::from_overloads(overloads)));
        }
    }
    (!answered.is_empty()).then(|| CallableTypes::from_elements(answered))
}

/// `callables` with their operand restricted to `taken`: each overload asked only about the
/// operands in `taken`, and one that accepts none of them left out. `None` where none is left,
/// since what the callables accept of `taken` is nothing
fn restrict_operand<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    callables: &CallableTypes<'db>,
    taken: Type<'db>,
) -> Option<CallableTypes<'db>> {
    if takes_every_operand(db, env, taken) {
        return Some(callables.clone());
    }
    let mut restricted = Vec::new();
    for callable in callables {
        let overloads: Vec<_> = callable
            .signatures(db)
            .iter()
            .filter_map(|signature| {
                let Some(operand) = signature.parameters().get_positional(0) else {
                    return Some(signature.clone());
                };
                let operand =
                    IntersectionType::from_two_elements(db, env, operand.annotated_type(), taken);
                (!operand.is_never()).then(|| signature.clone().with_parameter_type(0, operand))
            })
            .collect();
        if !overloads.is_empty() {
            restricted
                .push(callable.with_signatures(db, CallableSignature::from_overloads(overloads)));
        }
    }
    (!restricted.is_empty()).then(|| CallableTypes::from_elements(restricted))
}

/// whether a dunder that takes `taken` takes every operand
fn takes_every_operand<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    taken: Type<'db>,
) -> bool {
    Type::object().is_assignable_to(db, env, taken)
}

/// a runtime domain as the test of an operand names it: each class bare, as `isinstance` is
/// given it, rather than as every specialization of it
fn display_domain<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    domain: Type<'db>,
) -> String {
    let element = |element: Type<'db>| match element {
        Type::NominalInstance(instance) => instance
            .class(db, env)
            .class_literal(db)
            .name(db)
            .to_string(),
        _ => element.display(db, env).to_string(),
    };
    match domain {
        Type::Union(union) => union
            .elements(db)
            .iter()
            .map(|member| element(*member))
            .collect::<Vec<_>>()
            .join(" | "),
        _ => element(domain),
    }
}

/// what calling `receiver`'s `name` with `operand` does, as dispatch reads it
fn call_step<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    receiver: Type<'db>,
    name: &str,
    operand: Type<'db>,
) -> DunderStep<'db, Type<'db>> {
    DunderStep::of(db, env, operand, |operand| {
        receiver
            .try_call_dunder(
                db,
                env,
                name,
                CallArguments::positional([operand]),
                TypeContext::default(),
            )
            .map(|bindings| (bindings.return_type(db, env), false))
    })
}

/// `receiver`'s operator dunder `name`, where it is a plain method that hands back an operand
/// outside its domain
fn tested_dunder<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    receiver: Type<'db>,
    name: &str,
) -> Option<FunctionType<'db>> {
    let Some(Type::BoundMethod(bound)) = receiver
        .member(db, env, name)
        .place
        .ignore_possibly_undefined()
    else {
        return None;
    };
    let function = bound.function(db)?;
    tests_its_operand(db, function.literal(db).last_definition).then_some(function)
}

/// the operands `receiver`'s operator dunder `name` takes rather than hands back: the runtime
/// domain of each overload's operand together, or every operand where it isn't a plain method
/// that tests its operand
fn operand_domain<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    receiver: Type<'db>,
    name: &str,
) -> Type<'db> {
    let Some(function) = tested_dunder(db, env, receiver, name) else {
        return Type::object();
    };
    UnionType::from_elements(
        db,
        env,
        function.signature(db).overloads.iter().map(|signature| {
            signature
                .parameters()
                .get_positional(1)
                .map_or(Type::object(), Parameter::annotated_type)
                .runtime_domain(db, env)
        }),
    )
}

/// where the class body itself defines `name`, with a `def` or an assignment
fn own_definition(class_node: &ast::StmtClassDef, name: &str) -> Option<TextRange> {
    let named = |target: &ast::Expr| {
        target
            .as_name_expr()
            .filter(|target| target.id.as_str() == name)
            .map(Ranged::range)
    };
    class_node
        .body
        .iter()
        .find_map(|statement| match statement {
            ast::Stmt::FunctionDef(function) if function.name.as_str() == name => {
                Some(function.name.range())
            }
            ast::Stmt::Assign(assign) => assign.targets.iter().find_map(named),
            ast::Stmt::AnnAssign(assign) => named(&assign.target),
            _ => None,
        })
}
