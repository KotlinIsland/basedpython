//! basedpython: an operator dunder of a `.py` class, checked against what python hands it.
//!
//! Python calls `__add__`, `__eq__` and the other operator dunders with whatever the other operand
//! is. Dispatch reads an operand outside the domain of the parameter's annotation as handed back
//! with `NotImplemented` ([`Bindings::rejects_operand`]), which only holds if the body hands it
//! back. A `.by` dunder is lowered with a test that does; a `.py` one has to be written with it:
//!
//! ```python
//! class Money:
//!     def __add__(self, other: Money) -> Money:
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
//! - a body that returns a value for an operand it hasn't tested into the domain is reported as
//!   `unguarded-operand`: dispatch has already sent that operand somewhere else
//! - a `return NotImplemented` reached with an operand inside the domain hands that one back too,
//!   so the operation moves on with it ([`Bindings::may_reject_by_value`])
//!
//! [`Bindings::rejects_operand`]: crate::types::call::bind::Bindings::rejects_operand
//! [`Bindings::may_reject_by_value`]: crate::types::call::bind::Bindings::may_reject_by_value
//! [`AnalysisSettings::checked_operator_operands`]: crate::AnalysisSettings::checked_operator_operands

use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ruff_python_ast::helpers::ReturnStatementVisitor;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::Visitor;
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::definition::Definition;
use ty_python_core::operator_dunders::hands_back_operands;
use ty_python_core::semantic_index;

use crate::types::context::InferContext;
use crate::types::diagnostic::UNGUARDED_OPERAND;
use crate::types::function::{FunctionDecorators, OverloadLiteral};
use crate::types::inferred_narrowing::{
    NormalExit, holds_the_argument, is_declaration_body, narrowed_at_exit, normal_exits,
};
use crate::types::signatures::{Parameter, ReturnCallableTypeVarScope};
use crate::types::{
    IntersectionBuilder, Type, TypeContext, UnionType, binding_type, infer_scope_types,
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
    /// overload together, since dispatch asks each overload
    domain: Type<'db>,
}

impl<'db> PassedOperand<'db> {
    /// what the body sees the operand as before it tests it: the annotation, which an operand
    /// inside the domain matched at the call that dispatched to the method, or anything outside
    /// the domain, which python hands the method too
    pub(crate) fn as_passed(&self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Type<'db> {
        UnionType::from_two_elements(db, env, self.annotated, self.domain.negate(db, env))
    }
}

/// the operand `implementation` is handed, where it is an operator dunder of a `.py` class that
/// some operand is outside the domain of, and the project checks such a dunder against what
/// python hands it
///
/// `None` for a method that takes every operand, for a `@staticmethod` or `@classmethod`, and for
/// one whose operand has no annotation, which claims nothing about it
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
        || !hands_back_operands(implementation.name(db))
        || implementation.is_overload(db)
        || implementation.has_known_decorator(db, FunctionDecorators::STATICMETHOD)
        || implementation.has_known_decorator(db, FunctionDecorators::CLASSMETHOD)
    {
        return None;
    }
    let program_file = implementation.program_file(db);
    let definition = implementation.definition(db);
    semantic_index(db, program_file)
        .scope(definition.file_scope(db))
        .node()
        .as_class()?;

    // the receiver is positional parameter zero, and the operand the one after it. a method with
    // none there gathers the operand into `*args`, which takes anything
    let operand = |overload: OverloadLiteral<'db>| {
        overload
            .parameter_signature(db, ReturnCallableTypeVarScope::Public)
            .parameters()
            .get_positional(1)
            .cloned()
    };
    let parameter = operand(implementation)?;
    if parameter.inferred_annotation {
        return None;
    }
    let name = parameter.name()?.clone();
    let annotated = parameter.annotated_type();

    let env = &ProgramEnvironment::from_definition(definition);
    let overloads = match binding_type(db, definition) {
        Type::FunctionLiteral(function) => function.overloads_and_implementation(db).0,
        _ => &[],
    };
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
    if Type::object().is_assignable_to(db, env, domain) {
        return None;
    }
    Some(PassedOperand {
        name,
        annotated,
        domain,
    })
}

/// what the body of an operator dunder of a `.py` class does with the operand python hands it
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(crate) struct OperandExits {
    /// where the body returns a value with an operand it hasn't tested into the domain: a
    /// `return` statement, or `None` for falling off the end of the body
    pub(crate) unguarded: Box<[Option<TextRange>]>,
    /// the body puts another value in the operand parameter, so what it tests is not what python
    /// handed it
    pub(crate) rebinds: bool,
    /// a `return NotImplemented` is reached with an operand inside the domain, so the method
    /// hands back some of those too
    pub(crate) rejects_by_value: bool,
}

/// what the body of `implementation` does with its [`PassedOperand`]: nothing for a method that
/// has none
///
/// a cycle starts at "it hands back nothing inside the domain, and returns nothing unguarded",
/// which is where a body that has not been read yet stands
#[salsa::tracked(
    returns(ref),
    cycle_initial = |_, _, _| OperandExits::default(),
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn operand_exits<'db>(
    db: &'db dyn Db,
    implementation: OverloadLiteral<'db>,
) -> OperandExits {
    let Some(operand) = passed_operand(db, implementation) else {
        return OperandExits::default();
    };
    let program_file = implementation.program_file(db);
    let module = parsed_module(db, program_file.python_file(db)).load(db);
    let node = implementation.node(db, implementation.file(db), &module);

    // a method that declares one without implementing it, with a `...` body or as an
    // `@abstractmethod`, makes its claim for whatever implements it. a generator or coroutine
    // function returns before its body runs
    let mut returns = ReturnStatementVisitor::default();
    returns.visit_body(&node.body);
    if is_declaration_body(node)
        || implementation.has_known_decorator(db, FunctionDecorators::ABSTRACT_METHOD)
        || returns.is_generator
        || node.is_async
    {
        return OperandExits::default();
    }

    let env = &ProgramEnvironment::from_file(program_file);
    let index = semantic_index(db, program_file);
    let body_scope = implementation.body_scope(db);
    let file_scope_id = body_scope.file_scope_id(db);
    let use_def = index.use_def_map(file_scope_id);
    let inference = infer_scope_types(db, body_scope, TypeContext::default());

    // what a `return` hands back: `None` for a bare one, and for falling off the end
    let returned = |exit: NormalExit| match exit {
        NormalExit::Return { range, .. } => returns
            .returns
            .iter()
            .find(|statement| statement.range() == range)
            .and_then(|statement| statement.value.as_deref())
            .map_or_else(
                || Type::none(db, env),
                |value| inference.expression_type(value),
            ),
        NormalExit::EndOfBody => Type::none(db, env),
    };

    // a body that puts another value in the parameter tests that, not what python handed it. it
    // is reported once, and only a `NotImplemented` it might return is still read as handing back
    // an operand inside the domain
    if !holds_the_argument(db, index, file_scope_id, &operand.name, &[]) {
        let rejects_by_value = normal_exits(db, use_def)
            .into_iter()
            .any(|exit| returned(exit).includes_not_implemented(db));
        return OperandExits {
            unguarded: Box::default(),
            rebinds: true,
            rejects_by_value,
        };
    }

    let Some(place) = index
        .place_table(file_scope_id)
        .symbol_id(&operand.name)
        .map(Into::into)
    else {
        return OperandExits::default();
    };
    let as_passed = operand.as_passed(db, env);
    let start = |definition: Option<Definition<'db>>| {
        definition.map_or(as_passed, |definition| binding_type(db, definition))
    };
    let outside = operand.domain.negate(db, env);
    let overlaps = |a: Type<'db>, b: Type<'db>| {
        !IntersectionBuilder::new(db, env)
            .add_positive(a)
            .add_positive(b)
            .build()
            .is_never()
    };

    let mut unguarded = Vec::new();
    let mut rejects_by_value = false;
    for exit in normal_exits(db, use_def) {
        let returned = returned(exit);
        let established = narrowed_at_exit(db, env, use_def, exit, place, &start);
        // `Any` is inside every domain as far as assignability goes, and says nothing about the
        // operand: what is left of it outside the domain has to be nothing
        if !returned.without_not_implemented(db).is_never() && overlaps(established, outside) {
            unguarded.push(match exit {
                NormalExit::Return { range, .. } => Some(range),
                NormalExit::EndOfBody => None,
            });
        }
        if returned.includes_not_implemented(db) && overlaps(established, operand.domain) {
            rejects_by_value = true;
        }
    }
    OperandExits {
        unguarded: unguarded.into_boxed_slice(),
        rebinds: false,
        rejects_by_value,
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
    let Some(operand) = passed_operand(db, implementation) else {
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
            Some(range) => (*range, "where this returns"),
            None => (node.name.range(), "where the body ends"),
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
            "return `NotImplemented` for an operand outside `{domain}` before returning anything \
             else"
        ));
    }
}
