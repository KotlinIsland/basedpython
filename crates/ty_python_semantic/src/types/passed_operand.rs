//! basedpython: an operator dunder of a `.py` class, checked against what python hands it.
//!
//! Python calls `__add__`, `__eq__` and the other operator dunders with whatever the other operand
//! is. Dispatch reads an operand outside the domain of the parameter's annotation as handed back
//! with `NotImplemented` ([`Bindings::operand_rejection`]), which only holds if the body hands it
//! back. A `.by` dunder is lowered with a test that does; a `.py` one has to be written with it:
//!
//! ```python
//! class Money:
//!     def __add__(self, other: "Money") -> "Money":
//!         if not isinstance(other, Money):
//!             return NotImplemented
//!         return Money()
//! ```
//!
//! So under [`AnalysisSettings::checked_operator_operands`], three things follow from what python
//! really passes:
//!
//! - the body sees the operand as the annotation, or as anything outside its domain
//!   ([`PassedOperand::as_passed`]). an operand inside the domain matched the annotation at the
//!   call that dispatched to it, so only what is outside is new
//! - a body that returns a value, or raises, for an operand it hasn't tested into the domain is
//!   reported as `unguarded-operand`: dispatch has already sent that operand somewhere else
//! - a `return NotImplemented` reached with an operand inside the domain hands that one back too,
//!   so the operation moves on with it ([`Bindings::may_reject_by_value`]), and the return type
//!   has to declare it ([`report_undeclared_hand_backs`]). the test's own `return NotImplemented`
//!   needn't be declared
//!
//! [`Bindings::operand_rejection`]: crate::types::call::bind::Bindings::operand_rejection
//! [`Bindings::may_reject_by_value`]: crate::types::call::bind::Bindings::may_reject_by_value
//! [`AnalysisSettings::checked_operator_operands`]: crate::AnalysisSettings::checked_operator_operands

use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ruff_python_ast::helpers::ReturnStatementVisitor;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::Visitor;
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::definition::Definition;
use ty_python_core::semantic_index;

use crate::types::context::InferContext;
use crate::types::diagnostic::{UNGUARDED_OPERAND, report_invalid_return_type};
use crate::types::function::{FunctionDecorators, OverloadLiteral};
use crate::types::inferred_narrowing::{
    NormalExit, exits_through_finally, holds_the_argument, is_declaration_body, is_empty_body,
    narrowed_at_exit, narrowed_at_recorded_exit, raise_exits,
};
use crate::types::operator_domain::tests_its_operand;
use crate::types::signatures::{Parameter, ReturnCallableTypeVarScope};
use crate::types::{
    IntersectionBuilder, KnownClass, Type, TypeContext, UnionType, binding_type, infer_scope_types,
};
use crate::{Db, ProgramEnvironment};

/// the operand of an operator dunder of a `.py` class, which python hands whatever the other
/// operand is
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct PassedOperand<'db> {
    /// the operand parameter's name
    pub(crate) name: Name,
    /// what the parameter is annotated with
    annotated: Type<'db>,
    /// what dispatch reads the method as taking: the domain of the operand parameter of every
    /// overload together, since dispatch asks each overload. `object` for a method that takes
    /// every operand
    domain: Type<'db>,
    /// whether some operand is outside the domain, so that the body has something to test
    pub(crate) has_outside: bool,
}

impl<'db> PassedOperand<'db> {
    /// what the body sees the operand as before it tests it: the annotation, which an operand
    /// inside the domain matched at the call that dispatched to the method, or anything outside
    /// the domain, which python hands the method too
    pub(crate) fn as_passed(&self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Type<'db> {
        if !self.has_outside {
            return Type::object();
        }
        UnionType::from_two_elements(db, env, self.annotated, self.domain.negate(db, env))
    }
}

/// the operand `implementation` is handed, where it is an operator dunder of a `.py` class that
/// some operand is outside the domain of, and the project checks such a dunder against what
/// python hands it
///
/// `None` for a method dispatch doesn't take to test its operand ([`tests_its_operand`]): a
/// coroutine or generator function, a `@staticmethod` or `@classmethod`, an operand gathered into
/// `*args`, or a function bound under another name. also `None` for a decorated method, which
/// python calls through whatever the decorator made. an operand with no annotation of its own is
/// read as the type it inherits, from the method it overrides or the overloads it implements, and
/// otherwise takes every operand: such a method has nothing to test, but a `NotImplemented` it
/// returns still hands back what reaches it
#[salsa::tracked(
    returns(ref),
    cycle_initial = |_, _, _| None,
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn passed_operand<'db>(
    db: &'db dyn Db,
    implementation: OverloadLiteral<'db>,
) -> Option<PassedOperand<'db>> {
    let file = implementation.file(db);
    let source_type = file.source_type(db);
    if source_type.is_basedpython()
        || source_type.is_stub()
        || !db.analysis_settings(file).checked_operator_operands
        || implementation.is_overload(db)
        || !tests_its_operand(db, implementation)
    {
        return None;
    }
    let definition = implementation.definition(db);
    let Type::FunctionLiteral(function) = binding_type(db, definition) else {
        return None;
    };

    // the receiver is positional parameter zero, and the operand the one after it
    let operand = |overload: OverloadLiteral<'db>| {
        overload
            .parameter_signature(db, ReturnCallableTypeVarScope::Public)
            .parameters()
            .get_positional(1)
            .cloned()
    };
    let parameter = operand(implementation)?;
    let name = parameter.name()?.clone();
    let annotated = parameter.annotated_type();

    let env = &ProgramEnvironment::from_definition(definition);
    let overloads = function.overloads_and_implementation(db).0;
    let domain = if overloads.is_empty() {
        annotated.runtime_domain(db, env)
    } else {
        UnionType::from_elements(
            db,
            env,
            overloads.iter().map(|overload| {
                operand(*overload)
                    .as_ref()
                    .map_or(Type::object(), Parameter::annotated_type)
                    .runtime_domain(db, env)
            }),
        )
    };
    let has_outside = !Type::object().is_assignable_to(db, env, domain);
    Some(PassedOperand {
        name,
        annotated,
        domain: if has_outside { domain } else { Type::object() },
        has_outside,
    })
}

/// where the body of an operator dunder of a `.py` class gives an answer for an operand it
/// hasn't tested into the domain
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(crate) enum UnguardedExit {
    /// a `return` statement that hands back a value
    Return(TextRange),
    /// falling off the end of the body, which returns `None`
    EndOfBody,
    /// a `raise`, or an `assert` whose test fails: python stops there rather than moving on
    Raise(TextRange),
}

/// what the body of an operator dunder of a `.py` class does with the operand python hands it
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct OperandExits<'db> {
    /// where the body answers for an operand it hasn't tested into the domain
    unguarded: Box<[UnguardedExit]>,
    /// the body puts another value in the operand parameter, so what it tests is not what python
    /// handed it
    rebinds: bool,
    /// the operands inside the domain a `return NotImplemented` can be reached with, which the
    /// method hands back too. `Never` when it hands back none
    rejected_by_value: Type<'db>,
    /// the value of each `return` that hands back `NotImplemented` with an operand inside the
    /// domain, rather than as the test that sends an operand outside it on
    returned_by_value: Box<[TextRange]>,
    /// a `return` reached with an operand inside the domain returns something that may be
    /// `NotImplemented` without saying so, such as `Any` or `object`, so the body can't say
    /// whether it hands the operand back
    opaque_by_value: bool,
    /// the body was read. a method that only declares itself, and the start of a cycle, aren't
    read: bool,
}

impl OperandExits<'_> {
    fn none() -> Self {
        OperandExits {
            unguarded: Box::default(),
            rebinds: false,
            rejected_by_value: Type::Never,
            returned_by_value: Box::default(),
            opaque_by_value: false,
            read: false,
        }
    }
}

/// what the body of `implementation` does with its [`PassedOperand`]: nothing for a method that
/// has none
///
/// a cycle starts at "it hands back nothing inside the domain, and answers nothing unguarded",
/// which is where a body that has not been read yet stands
#[salsa::tracked(
    returns(ref),
    cycle_initial = |_, _, _| OperandExits::none(),
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn operand_exits<'db>(
    db: &'db dyn Db,
    implementation: OverloadLiteral<'db>,
) -> OperandExits<'db> {
    let Some(operand) = passed_operand(db, implementation) else {
        return OperandExits::none();
    };
    let program_file = implementation.program_file(db);
    let module = parsed_module(db, program_file.python_file(db)).load(db);
    let node = implementation.node(db, implementation.file(db), &module);
    let env = &ProgramEnvironment::from_file(program_file);
    let index = semantic_index(db, program_file);

    // a method that declares one without implementing it, with a `...` body, as an
    // `@abstractmethod`, or as a member of a protocol that does nothing, makes its claim for
    // whatever implements it
    if is_declaration_body(node)
        || implementation.has_known_decorator(db, FunctionDecorators::ABSTRACT_METHOD)
        || (is_protocol_member(db, implementation) && is_empty_body(node))
    {
        return OperandExits::none();
    }

    let mut returns = ReturnStatementVisitor::default();
    returns.visit_body(&node.body);
    let body_scope = implementation.body_scope(db);
    let file_scope_id = body_scope.file_scope_id(db);
    let use_def = index.use_def_map(file_scope_id);
    let inference = infer_scope_types(db, body_scope, TypeContext::default());

    // the value a `return` is written with, where it has one
    let value = |exit: NormalExit| match exit {
        NormalExit::Return { range, .. } => returns
            .returns
            .iter()
            .find(|statement| statement.range() == range)
            .and_then(|statement| statement.value.as_deref()),
        NormalExit::EndOfBody => None,
    };
    // what a `return` hands back: `None` for a bare one, and for falling off the end
    let returned = |exit: NormalExit| {
        value(exit).map_or_else(
            || Type::none(db, env),
            |value| inference.expression_type(value),
        )
    };
    let not_implemented = KnownClass::NotImplementedType.to_instance(db, env);
    let opaque = |returned: Type<'db>| {
        !returned.includes_not_implemented(db)
            && not_implemented.is_assignable_to(db, env, returned)
    };
    // a `return` is read where it is written, even when a `finally` suite runs after it: that is
    // where what it hands back is decided
    let exits = exits_through_finally(db, use_def);

    // a body that puts another value in the parameter tests that, not what python handed it. it
    // is reported once, and every `NotImplemented` it might return is read as handing back an
    // operand inside the domain
    if !holds_the_argument(db, index, file_scope_id, &operand.name, &[]) {
        let returned_by_value: Box<[TextRange]> = exits
            .iter()
            .filter(|exit| returned(**exit).includes_not_implemented(db))
            .filter_map(|exit| value(*exit).map(Ranged::range))
            .collect();
        return OperandExits {
            unguarded: Box::default(),
            rebinds: true,
            rejected_by_value: if returned_by_value.is_empty() {
                Type::Never
            } else {
                operand.domain
            },
            returned_by_value,
            opaque_by_value: exits.iter().any(|exit| opaque(returned(*exit))),
            read: true,
        };
    }

    let Some(place) = index
        .place_table(file_scope_id)
        .symbol_id(&operand.name)
        .map(Into::into)
    else {
        return OperandExits::none();
    };
    // nothing but the parameter binds the operand, so each way out starts from what python
    // passed
    let as_passed = operand.as_passed(db, env);
    let start = |_: Option<Definition<'db>>| as_passed;
    let outside = operand.domain.negate(db, env);
    let meet = |a: Type<'db>, b: Type<'db>| {
        IntersectionBuilder::new(db, env)
            .add_positive(a)
            .add_positive(b)
            .build()
    };

    let mut unguarded = Vec::new();
    let mut rejected_by_value = Vec::new();
    let mut returned_by_value = Vec::new();
    let mut opaque_by_value = false;
    for exit in exits {
        let returned = returned(exit);
        let established = narrowed_at_exit(db, env, use_def, exit, place, &start);
        // `Any` is inside every domain as far as assignability goes, and says nothing about the
        // operand: what is left of it outside the domain has to be nothing
        if !returned.without_not_implemented(db).is_never()
            && !meet(established, outside).is_never()
        {
            unguarded.push(match exit {
                NormalExit::Return { range, .. } => UnguardedExit::Return(range),
                NormalExit::EndOfBody => UnguardedExit::EndOfBody,
            });
        }
        let inside = meet(established, operand.domain);
        if opaque(returned) && !inside.is_never() {
            opaque_by_value = true;
        }
        if returned.includes_not_implemented(db) {
            if !inside.is_never()
                && let Some(value) = value(exit)
            {
                returned_by_value.push(value.range());
            }
            rejected_by_value.push(inside);
        }
    }
    // python stops at a raise, where dispatch would have moved on with an operand outside the
    // domain
    for (exit, range) in raise_exits(db, use_def) {
        let established = narrowed_at_recorded_exit(db, env, use_def, exit, place, &start);
        if !meet(established, outside).is_never() {
            unguarded.push(UnguardedExit::Raise(range));
        }
    }
    OperandExits {
        unguarded: unguarded.into_boxed_slice(),
        rebinds: false,
        rejected_by_value: UnionType::from_elements(db, env, rejected_by_value),
        returned_by_value: returned_by_value.into_boxed_slice(),
        opaque_by_value,
        read: true,
    }
}

/// what the body of a checked `.py` operator dunder hands back by value
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct BodyRejection<'db> {
    /// the operands inside the domain a `return NotImplemented` is reached with. `Never` for a
    /// body that hands back none
    pub(crate) rejected: Type<'db>,
    /// some `return` reached with an operand inside the domain may be `NotImplemented` without
    /// saying so, so only the return type can say whether the body hands it back
    pub(crate) opaque: bool,
}

/// what the body of the `.py` operator dunder `implementation` hands back by value: `None` for a
/// method whose body isn't read, because it isn't checked or only declares itself
///
/// kept apart from [`operand_exits`] so that an edit to the body that moves its `return`
/// statements, without changing what they hand back, doesn't change what dispatch reads
#[salsa::tracked(
    cycle_initial = |_, _, _| None,
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn body_rejection<'db>(
    db: &'db dyn Db,
    implementation: OverloadLiteral<'db>,
) -> Option<BodyRejection<'db>> {
    let exits = operand_exits(db, implementation);
    exits.read.then_some(BodyRejection {
        rejected: exits.rejected_by_value,
        opaque: exits.opaque_by_value,
    })
}

/// the `return` values of `implementation` a `NotImplemented` they return has to be declared at,
/// where it is an operator dunder whose test of its operand dispatch reads: those reached with an
/// operand inside the domain, which the method hands back by value. a `return NotImplemented`
/// reached only with an operand outside it is that test, which dispatch already moves past, so
/// it needn't be declared
///
/// ```python
/// class A:
///     def __add__(self, other: int) -> int:
///         if not isinstance(other, int):
///             return NotImplemented  # the test
///         return NotImplemented  # hands back an `int`, which `-> int` doesn't say
/// ```
///
/// `None` for any other function, and for a method whose body isn't read
fn returned_by_value<'db>(
    db: &'db dyn Db,
    implementation: OverloadLiteral<'db>,
) -> Option<&'db [TextRange]> {
    let exits = operand_exits(db, implementation);
    exits.read.then_some(&*exits.returned_by_value)
}

/// whether the return check of `implementation`, a function of a checked `.py` file, leaves
/// its `return NotImplemented` statements to [`check_operand_exits`]: an operator dunder whose
/// body is read, where only those reached with an operand inside the domain have to be declared.
/// it can't ask which those are itself, because they are read off the body it is checking
pub(crate) fn defers_not_implemented_returns<'db>(
    db: &'db dyn Db,
    implementation: OverloadLiteral<'db>,
) -> bool {
    passed_operand(db, implementation).is_some()
        && !implementation.has_known_decorator(db, FunctionDecorators::ABSTRACT_METHOD)
}

/// whether `implementation` is written in the body of a protocol class
pub(crate) fn is_protocol_member<'db>(
    db: &'db dyn Db,
    implementation: OverloadLiteral<'db>,
) -> bool {
    let definition = implementation.definition(db);
    let program_file = implementation.program_file(db);
    let index = semantic_index(db, program_file);
    let Some(class) = index.scope(definition.file_scope(db)).node().as_class() else {
        return false;
    };
    let module = parsed_module(db, program_file.python_file(db)).load(db);
    let class_definition = index.expect_single_definition(class.node(&module));
    matches!(
        binding_type(db, class_definition),
        Type::ClassLiteral(class) if class.is_protocol(db)
    )
}

/// report each `return NotImplemented` of the `.py` operator dunder `node` that hands back an
/// operand inside the domain, where its return type doesn't say it may
fn report_undeclared_hand_backs<'db>(
    context: &InferContext<'db, '_>,
    implementation: OverloadLiteral<'db>,
    node: &ast::StmtFunctionDef,
) {
    let db = context.db();
    let env = &context.program_environment();
    let Some(returns) = node.returns.as_deref() else {
        return;
    };
    if !defers_not_implemented_returns(db, implementation) {
        return;
    }
    let (Some(operand), Some(returned_by_value)) = (
        passed_operand(db, implementation),
        returned_by_value(db, implementation),
    ) else {
        return;
    };
    let declared = implementation.signature(db).return_ty;
    let not_implemented = KnownClass::NotImplementedType.to_instance(db, env);
    if not_implemented.is_assignable_to(db, env, declared) {
        return;
    }
    let method = node.name.as_str();
    let name = &operand.name;
    for range in returned_by_value {
        let Some(mut diagnostic) =
            report_invalid_return_type(context, *range, returns, declared, not_implemented)
        else {
            continue;
        };
        if operand.has_outside {
            diagnostic.info(format_args!(
                "this `return NotImplemented` is reached with `{name}` inside `{}`, so it hands \
                 that operand back: declare `NotImplementedType` in the return type",
                operand.domain.display(db, env),
            ));
        } else {
            diagnostic.info(format_args!(
                "`{name}` takes every operand, so this `return NotImplemented` hands back one \
                 `{method}` was given: annotate `{name}` with what `{method}` takes, which makes \
                 this its test, or declare `NotImplementedType` in the return type",
            ));
        }
    }
}

/// report where the `.py` operator dunder `node` returns a value with an operand it hasn't tested
/// into its domain, which dispatch has already sent somewhere else
pub(crate) fn check_operand_exits<'db>(
    context: &InferContext<'db, '_>,
    ty: Type<'db>,
    node: &ast::StmtFunctionDef,
) {
    let Type::FunctionLiteral(function) = ty else {
        return;
    };
    let db = context.db();
    let env = &context.program_environment();
    let implementation = function.literal(db).last_definition;
    report_undeclared_hand_backs(context, implementation, node);
    let Some(operand) = passed_operand(db, implementation)
        .as_ref()
        .filter(|operand| operand.has_outside)
    else {
        return;
    };
    let exits = operand_exits(db, implementation);
    let method = node.name.as_str();
    let domain = operand.domain.display(db, env);

    if exits.rebinds {
        let parameter = node
            .parameters
            .posonlyargs
            .iter()
            .chain(&node.parameters.args)
            .nth(1);
        let anchor = parameter.map_or_else(|| node.name.range(), Ranged::range);
        if let Some(builder) = context.report_lint(&UNGUARDED_OPERAND, anchor) {
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "`{method}` puts another value in `{name}`, so what it tests is not the operand \
                 python hands it",
                name = operand.name,
            ));
            diagnostic.info(format_args!(
                "python hands `{method}` any operand, and one outside `{domain}` has to be handed \
                 back with `NotImplemented`"
            ));
        }
        return;
    }

    for exit in &exits.unguarded {
        let (anchor, location) = match exit {
            UnguardedExit::Return(range) => (*range, "where this returns"),
            UnguardedExit::EndOfBody => (node.name.range(), "where the body ends"),
            UnguardedExit::Raise(range) => (*range, "where this raises"),
        };
        let Some(builder) = context.report_lint(&UNGUARDED_OPERAND, anchor) else {
            continue;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "`{name}` may be outside `{domain}` {location}",
            name = operand.name,
        ));
        diagnostic.info(format_args!(
            "python hands `{method}` any operand, and the operation is typed as moving on from \
             one outside `{domain}`"
        ));
        diagnostic.help(format_args!(
            "return `NotImplemented` for an operand outside `{domain}` before returning or \
             raising anything else"
        ));
    }
}
