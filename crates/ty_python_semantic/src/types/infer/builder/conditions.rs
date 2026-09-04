use std::collections::VecDeque;

use ruff_db::parsed::parsed_module;
use ruff_python_ast::{self as ast, helpers::any_over_expr};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::FxHashSet;
use ty_module_resolver::KnownModule;
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::predicate::PatternSubject;
use ty_python_core::scope::ScopeId;
use ty_python_core::{ProgramFile, place_table, semantic_index, use_def_map};
use ty_python_core::{Truthiness, place::PlaceExpr};

use crate::place::Place;
use crate::types::ProgramEnvironment;
use crate::types::TypeContext;
use crate::types::definition_expression_type;
use crate::types::definition_resolution::{
    ImportAliasResolution, ResolvedDefinition, scoped_definitions_for_name,
};
use crate::types::infer::infer_scope_types;
use crate::types::{
    ClassLiteral, IntersectionBuilder, KnownClass, Type,
    diagnostic::{OVERLAPPING_CONDITION, REDUNDANT_BOOLEAN_COMPARISON, REDUNDANT_CONDITION},
    infer::TypeInferenceBuilder,
};
use crate::{AnalysisSettings, Db};

/// How a condition's outcome is decided.
///
/// This is [`Truthiness`] plus the state a type cannot express. `TYPE_CHECKING` and
/// `sys.version_info` are constant for the same reason a `Literal[True]` parameter is: nothing
/// distinguishes them at the type level. But their constant-ness is manufactured by the checker's
/// model of the build environment rather than being a consequence of the program's own types, and
/// picking a branch with one is the entire point of writing it. That is [`Artificial`], and it is
/// the one constant outcome that is not worth reporting.
///
/// [`Artificial`]: ConditionTruthiness::Artificial
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionTruthiness {
    /// the tested value decides the branch at runtime
    Ambiguous,
    /// the tested value's own type decides it
    AlwaysTrue,
    AlwaysFalse,
    /// decided, but by the build environment rather than by the program
    Artificial,
}

impl ConditionTruthiness {
    /// Classify a condition. `artificial` is only consulted for an outcome that is already
    /// constant, and is a closure because answering it walks the condition.
    fn classify(
        truthiness: Truthiness,
        polarity: ConditionPolarity,
        artificial: impl FnOnce() -> bool,
    ) -> Self {
        let selected = match truthiness {
            Truthiness::Ambiguous => return Self::Ambiguous,
            _ if artificial() => return Self::Artificial,
            Truthiness::AlwaysTrue => ConditionPolarity::Truthy,
            Truthiness::AlwaysFalse => ConditionPolarity::Falsy,
        };
        if selected == polarity {
            Self::AlwaysTrue
        } else {
            Self::AlwaysFalse
        }
    }

    /// The outcome of a condition that has one, and the word for the truthiness behind it.
    const fn constant_outcome(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::AlwaysTrue => Some(("true", "truthy")),
            Self::AlwaysFalse => Some(("false", "falsy")),
            Self::Ambiguous | Self::Artificial => None,
        }
    }
}

/// Which half of the tested value a condition selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionPolarity {
    Truthy,
    Falsy,
}

impl ConditionPolarity {
    #[must_use]
    const fn negate(self) -> Self {
        match self {
            Self::Truthy => Self::Falsy,
            Self::Falsy => Self::Truthy,
        }
    }

    const fn noun(self) -> &'static str {
        match self {
            Self::Truthy => "truthiness",
            Self::Falsy => "falsiness",
        }
    }

    /// The half of an arm that this polarity rejects outright.
    const fn rejects<'db>(self) -> Type<'db> {
        match self {
            Self::Truthy => Type::AlwaysFalsy,
            Self::Falsy => Type::AlwaysTruthy,
        }
    }
}

/// The scope an expression was written in, which is where its type was worked out.
fn expression_scope<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    expr: &ast::Expr,
) -> ScopeId<'db> {
    semantic_index(db, file)
        .expression_scope_id(expr)
        .to_scope_id(db, file)
}

/// Where an expression's type is to be found.
///
/// A definition's own value is typed by that definition — which is also what reaches an expression
/// it borrows from an enclosing scope, such as a comprehension's first iterable. A statement
/// *around* a definition belongs to no definition of its own, so its scope answers for it.
#[derive(Debug, Clone, Copy)]
enum ExpressionTypes<'db> {
    Of(Definition<'db>),
    InScope(ProgramFile<'db>),
}

impl<'db> ExpressionTypes<'db> {
    fn file(self, db: &'db dyn Db) -> ProgramFile<'db> {
        match self {
            Self::Of(definition) => definition.program_file(db),
            Self::InScope(file) => file,
        }
    }

    fn of(self, db: &'db dyn Db, expr: &ast::Expr) -> Type<'db> {
        if let Self::Of(definition) = self {
            let ty = definition_expression_type(db, definition, expr);
            if !ty.is_unknown() {
                return ty;
            }
            // an unpacked assignment evaluates its right-hand side once, on its own, so the
            // targets sharing it do not carry the parts — the scope that ran it does
        }
        let file = self.file(db);
        infer_scope_types(db, expression_scope(db, file, expr), TypeContext::default())
            .expression_type(expr)
    }
}

/// The free form of [`TypeInferenceBuilder::is_environment_fact`], for an expression in another
/// scope — or another module — than the condition that led here.
fn expression_is_environment_fact<'db>(
    db: &'db dyn Db,
    types: ExpressionTypes<'db>,
    expr: &ast::Expr,
) -> bool {
    let is_version_info = |expr: &ast::Expr| {
        matches!(
            types.of(db, expr),
            Type::NominalInstance(instance) if instance.is_sys_version_info()
        )
    };
    match expr {
        ast::Expr::Name(name) => name.id == "TYPE_CHECKING" || is_version_info(expr),
        ast::Expr::Attribute(attribute) => {
            if is_version_info(expr) {
                return true;
            }
            let Type::ModuleLiteral(module) = types.of(db, &attribute.value) else {
                return false;
            };
            let module = module.module(db);
            match &*attribute.attr {
                "version_info" | "platform" => module.is_known(db, KnownModule::Sys),
                "name" => module.is_known(db, KnownModule::Os),
                "TYPE_CHECKING" => {
                    module.is_known(db, KnownModule::Typing)
                        || module.is_known(db, KnownModule::TypingExtensions)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// Whether the value `definition` binds is decided by the build environment rather than by the
/// program.
///
/// This is the alias half of [`is_environment_fact`]: `IS_PY314 = sys.version_info >= (3, 14)` is
/// as much a fact about the environment as the `sys.version_info` it reads, and so is every name
/// that goes on to stand for it, in this module or another. Tracked because following one alias
/// reaches whatever module declared it, and a name is asked about once per condition that tests
/// it.
///
/// [`is_environment_fact`]: TypeInferenceBuilder::is_environment_fact
#[salsa::tracked(returns(copy), cycle_initial=|_, _, _| false, heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn definition_is_environment_derived<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> bool {
    let mut visited = FxHashSet::default();
    definition_is_environment_derived_inner(db, definition, &mut visited)
}

fn definition_is_environment_derived_inner<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    visited: &mut FxHashSet<Definition<'db>>,
) -> bool {
    if !visited.insert(definition) {
        return false;
    }
    let module = parsed_module(db, definition.python_file(db)).load(db);
    // an import is followed by name resolution before it ever reaches here, so what is left is
    // the value a binding was written with
    let value: &ast::Expr = match definition.kind(db) {
        DefinitionKind::Assignment(assignment) => assignment.value(&module),
        DefinitionKind::AnnotatedAssignment(assignment) => {
            let Some(value) = assignment.value(&module) else {
                return false;
            };
            value
        }
        DefinitionKind::NamedExpression(named) => &named.node(&module).value,
        DefinitionKind::For(for_stmt) => for_stmt.iterable(&module),
        DefinitionKind::Comprehension(comprehension) => comprehension.iterable(&module),
        DefinitionKind::WithItem(with_item) => with_item.context_expr(&module),
        // a capture stands for whatever was matched, so it carries the subject's origin the way
        // an assignment carries its value's
        DefinitionKind::MatchPattern(pattern) => {
            let subject_is_derived = match pattern.predicate().subject(db) {
                PatternSubject::Expression(expression) => expression_is_environment_derived(
                    db,
                    ExpressionTypes::Of(definition),
                    expression.node_ref(db).node(&module),
                    visited,
                ),
                PatternSubject::Binder(binder) => {
                    definition_is_environment_derived_inner(db, binder, visited)
                }
            };
            return subject_is_derived || definition_is_environment_gated(db, definition, visited);
        }
        _ => return false,
    };
    expression_is_environment_derived(db, ExpressionTypes::Of(definition), value, visited)
        || definition_is_environment_gated(db, definition, visited)
}

/// Whether `definition` is only reached when the build environment says so.
///
/// `line_prefix` below is written twice, and neither value mentions the environment — but which of
/// them the program ever performs is settled before it runs, and so is the `Literal[""]` a reader
/// is then told the name has:
///
/// ```python
/// if sys.platform == "win32":
///     line_prefix = "\n"
/// else:
///     line_prefix = ""
/// ```
///
/// So the statements enclosing the binding are asked, the same question the value was asked. Only
/// this module is walked: a test written in another scope has no type here, which makes it no
/// fact, and a missed guard reports a condition rather than hiding one.
fn definition_is_environment_gated<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    visited: &mut FxHashSet<Definition<'db>>,
) -> bool {
    let file = definition.program_file(db);
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let target = definition.full_range(db, &module).range();
    statements_gate_range(db, file, &module.syntax().body, target, visited)
}

/// Whether any statement in `body` that encloses `target` decides it on the environment.
fn statements_gate_range<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    body: &[ast::Stmt],
    target: TextRange,
    visited: &mut FxHashSet<Definition<'db>>,
) -> bool {
    let Some(statement) = body
        .iter()
        .find(|statement| statement.range().contains_range(target))
    else {
        return false;
    };
    let gated = |test: &ast::Expr, visited: &mut FxHashSet<Definition<'db>>| {
        expression_is_environment_derived(db, ExpressionTypes::InScope(file), test, visited)
    };
    match statement {
        ast::Stmt::If(if_statement) => {
            if gated(&if_statement.test, visited) {
                return true;
            }
            for clause in &if_statement.elif_else_clauses {
                if let Some(test) = &clause.test
                    && gated(test, visited)
                {
                    return true;
                }
            }
            std::iter::once(&if_statement.body)
                .chain(
                    if_statement
                        .elif_else_clauses
                        .iter()
                        .map(|clause| &clause.body),
                )
                .any(|body| statements_gate_range(db, file, body, target, visited))
        }
        ast::Stmt::Match(match_statement) => {
            if gated(&match_statement.subject, visited) {
                return true;
            }
            match_statement
                .cases
                .iter()
                .any(|case| statements_gate_range(db, file, &case.body, target, visited))
        }
        ast::Stmt::While(while_statement) => {
            gated(&while_statement.test, visited)
                || std::iter::once(&while_statement.body)
                    .chain(std::iter::once(&while_statement.orelse))
                    .any(|body| statements_gate_range(db, file, body, target, visited))
        }
        // every other statement that holds a block: the guard, if there is one, is further in
        ast::Stmt::For(for_statement) => [&for_statement.body, &for_statement.orelse]
            .into_iter()
            .any(|body| statements_gate_range(db, file, body, target, visited)),
        ast::Stmt::With(with_statement) => {
            statements_gate_range(db, file, &with_statement.body, target, visited)
        }
        ast::Stmt::Try(try_statement) => [
            &try_statement.body,
            &try_statement.orelse,
            &try_statement.finalbody,
        ]
        .into_iter()
        .chain(try_statement.handlers.iter().map(|handler| {
            let ast::ExceptHandler::ExceptHandler(handler) = handler;
            &handler.body
        }))
        .any(|body| statements_gate_range(db, file, body, target, visited)),
        ast::Stmt::FunctionDef(function) => {
            statements_gate_range(db, file, &function.body, target, visited)
        }
        ast::Stmt::ClassDef(class) => statements_gate_range(db, file, &class.body, target, visited),
        _ => false,
    }
}

/// Whether `expr`, as it is written in `scope`, is decided by the build environment.
///
/// Every part of the expression is asked, so a guard stays recognisable through the shapes a
/// program builds around it — a comparison, a conditional expression, a tuple that an assignment
/// then unpacks.
fn expression_is_environment_derived<'db>(
    db: &'db dyn Db,
    types: ExpressionTypes<'db>,
    expr: &ast::Expr,
    visited: &mut FxHashSet<Definition<'db>>,
) -> bool {
    let mut found = false;
    any_over_expr(expr, &mut |part: &ast::Expr| {
        if found {
            return true;
        }
        if leaf_is_environment_derived(db, types, part, visited) {
            found = true;
        }
        found
    });
    found
}

/// Whether one leaf of an expression names an environment fact, directly or through an alias.
fn leaf_is_environment_derived<'db>(
    db: &'db dyn Db,
    types: ExpressionTypes<'db>,
    expr: &ast::Expr,
    visited: &mut FxHashSet<Definition<'db>>,
) -> bool {
    if expression_is_environment_fact(db, types, expr) {
        return true;
    }
    let scope = expression_scope(db, types.file(db), expr);
    match expr {
        ast::Expr::Name(name) => {
            scoped_definitions_for_name(db, scope, &name.id, ImportAliasResolution::ResolveAliases)
                .into_iter()
                .filter_map(|resolved| match resolved {
                    ResolvedDefinition::Definition(definition) => Some(definition),
                    ResolvedDefinition::Module(_) | ResolvedDefinition::FileWithRange(_) => None,
                })
                .any(|definition| definition_is_environment_derived_inner(db, definition, visited))
        }
        // a member read off a class or an instance of one, such as a `Final` flag a module
        // computed once from the platform it is being checked for
        ast::Expr::Attribute(attribute) => {
            let receiver = types.of(db, &attribute.value);
            member_definitions(db, scope, receiver, &attribute.attr)
                .into_iter()
                .any(|definition| definition_is_environment_derived_inner(db, definition, visited))
        }
        _ => false,
    }
}

/// The definitions of `name` on `receiver`, for the members a condition can be written against.
fn member_definitions<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    receiver: Type<'db>,
    name: &str,
) -> Vec<Definition<'db>> {
    let env = ProgramEnvironment::from_scope(scope);
    let mut definitions = Vec::new();
    for element in receiver.union_elements(db) {
        // an intersection narrows the receiver to one of its members, and it is that member's
        // declaration a read lands on
        let element = element
            .as_intersection()
            .and_then(|intersection| intersection.positive(db).iter().next().copied())
            .unwrap_or(element);
        let Some(class) = (match element {
            Type::NominalInstance(instance) => Some(instance.class_literal(db, &env)),
            Type::ClassLiteral(class) => Some(class),
            _ => None,
        })
        .and_then(ClassLiteral::as_static) else {
            continue;
        };
        for ancestor in class.iter_mro(db, None) {
            let Some(body_scope) = ancestor
                .into_class()
                .map(|class| class.class_literal(db))
                .and_then(ClassLiteral::as_static)
                .map(|class| class.body_scope(db))
            else {
                continue;
            };
            let Some(symbol) = place_table(db, body_scope).symbol_id(name) else {
                continue;
            };
            definitions.extend(
                use_def_map(db, body_scope)
                    .end_of_scope_symbol_bindings(symbol)
                    .filter_map(|binding| binding.binding.definition()),
            );
        }
    }
    definitions
}

/// Strip the `not`s off a condition, returning the expression whose truthiness is really tested
/// and the half of it the condition selects.
///
/// A walrus is stripped too: `while (chunk := read()):` tests what `read()` returned, and reading
/// the condition as the *target* would make a `while (x := False):` look like a constant place
/// rather than the literal it is.
fn condition_root(
    test: &ast::Expr,
    mut polarity: ConditionPolarity,
) -> (&ast::Expr, ConditionPolarity) {
    let mut root = test;
    loop {
        match root {
            ast::Expr::UnaryOp(unary) if unary.op == ast::UnaryOp::Not => {
                root = &unary.operand;
                polarity = polarity.negate();
            }
            ast::Expr::Named(named) => root = &named.value,
            _ => return (root, polarity),
        }
    }
}

impl<'db> TypeInferenceBuilder<'db, '_> {
    /// Check a condition — the test of an `if`/`elif`/`while`/`assert`, a conditional expression
    /// or a comprehension guard — for the two ways a truthiness test can go wrong: the branch it
    /// selects is shared by members the branch cannot tell apart, or it selects nothing because
    /// the outcome is already fixed.
    ///
    /// A boolean operator is not one condition but one per operand: each operand's truthiness is
    /// tested on its own at runtime, and the operator's *value* — the union of the operands — is
    /// not a value anything is tested for. `if count > 0 or leftovers:` asks two questions, and
    /// reading it as one would claim it conflates a `bool` with a `dict`.
    pub(super) fn check_condition(&mut self, test: &ast::Expr) {
        if !self.context.is_lint_enabled(&OVERLAPPING_CONDITION)
            && !self.context.is_lint_enabled(&REDUNDANT_CONDITION)
        {
            return;
        }
        // An explicit queue rather than recursion: a same-operator chain flattens into one
        // `BoolOp`, but `a and (b or (…))` nests, and unbounded recursion in this crate spends a
        // stack budget that windows measures in one megabyte.
        let mut queue = VecDeque::from([(test, ConditionPolarity::Truthy)]);
        while let Some((test, polarity)) = queue.pop_front() {
            let (root, polarity) = condition_root(test, polarity);
            if let ast::Expr::BoolOp(bool_op) = root {
                // front, so operands are checked in source order
                for operand in bool_op.values.iter().rev() {
                    queue.push_front((operand, polarity));
                }
                continue;
            }
            self.check_single_condition(test, root, polarity);
        }
    }

    fn check_single_condition(
        &mut self,
        test: &ast::Expr,
        root: &ast::Expr,
        polarity: ConditionPolarity,
    ) {
        let env = self.program_environment();
        // Only a value *read* can have its outcome fixed by its own type. A comparison or a call
        // computes a fresh value, and ty folding that one is the statically-known-branch
        // machinery doing its job — `elif isinstance(x, B):` closing an exhaustive chain is
        // deliberate, not a conditional that failed to be conditional.
        let is_place = PlaceExpr::try_from_expr(root).is_some();
        let truthiness = ConditionTruthiness::classify(
            self.expression_type(root).bool(self.db(), env),
            polarity,
            || is_place && self.is_artificial(root),
        );
        match truthiness {
            ConditionTruthiness::Ambiguous => {
                self.check_overlapping_condition(test, root, polarity);
            }
            ConditionTruthiness::AlwaysTrue | ConditionTruthiness::AlwaysFalse
                if is_place && self.outcome_is_declared(root, polarity) =>
            {
                self.report_redundant_condition(test, root, truthiness);
            }
            // a constant that is not a value read, or one the build environment manufactured
            ConditionTruthiness::AlwaysTrue
            | ConditionTruthiness::AlwaysFalse
            | ConditionTruthiness::Artificial => {}
        }
    }

    /// Whether `root`'s constant outcome is the program's doing rather than a
    /// narrowing's.
    ///
    /// A name is only rebound by the code between the narrowing and the read,
    /// and that code is in this scope, where ty can see it. An attribute reaches
    /// into an object, and any call in between may have written to it — ty holds
    /// the narrowed type across such a call, which is what makes attribute
    /// narrowing usable at all, but it means a constant read off one is not a
    /// fact about the program:
    ///
    /// ```py
    /// latch.on = False        # `on: bool`
    /// latch.flip()            # assigns `self.on = True`
    /// assert latch.on         # not "always false" — `on` is a `bool`
    /// ```
    ///
    /// So an attribute is asked what its class declares, which is a fact. A
    /// subscript reaches into an object the same way and has no declaration to
    /// fall back on — its element type comes out of a `__getitem__` call, which
    /// this would have to redo — so it is never reported.
    fn outcome_is_declared(&self, root: &ast::Expr, polarity: ConditionPolarity) -> bool {
        let db = self.db();
        let env = self.program_environment();
        match root {
            ast::Expr::Attribute(attribute) => self
                .expression_type(&attribute.value)
                .member(db, env, attribute.attr.as_str())
                .place
                .ignore_possibly_undefined()
                .is_some_and(|declared| {
                    ConditionTruthiness::classify(declared.bool(db, env), polarity, || false)
                        .constant_outcome()
                        .is_some()
                }),
            ast::Expr::Subscript(_) => false,
            _ => true,
        }
    }

    fn report_redundant_condition(
        &mut self,
        test: &ast::Expr,
        root: &ast::Expr,
        truthiness: ConditionTruthiness,
    ) {
        let env = self.program_environment();
        let Some((outcome, adjective)) = truthiness.constant_outcome() else {
            return;
        };
        let Some(builder) = self.context.report_lint(&REDUNDANT_CONDITION, test) else {
            return;
        };
        let mut diagnostic =
            builder.into_diagnostic(format_args!("This condition is always {outcome}"));
        diagnostic.info(format_args!(
            "`{}` is always {adjective}",
            self.expression_type(root).display(self.db(), env)
        ));
    }

    /// Whether any part of `test` is a build-environment fact, which makes the whole condition's
    /// constant outcome the checker's doing rather than the program's.
    ///
    /// A fact reaches a condition under whatever name the program gave it, so a name is followed
    /// to what it stands for — through this module and any other. The walk is only ever reached
    /// for a condition that is already constant, which is the rare case.
    fn is_artificial(&self, test: &ast::Expr) -> bool {
        let mut visited = FxHashSet::default();
        any_over_expr(test, &mut |expr: &ast::Expr| {
            self.is_environment_fact(expr) || self.reads_environment_alias(expr, &mut visited)
        })
    }

    /// Whether `expr` names something that stands for an environment fact.
    ///
    /// The types this needs are the ones being inferred right now, so they are read off this
    /// builder rather than by asking for the scope's inference — which is this. Only the
    /// definitions a name resolves to are followed outwards, and each of those is somewhere else.
    fn reads_environment_alias(
        &self,
        expr: &ast::Expr,
        visited: &mut FxHashSet<Definition<'db>>,
    ) -> bool {
        let db = self.db();
        match expr {
            ast::Expr::Name(name) => scoped_definitions_for_name(
                db,
                self.scope(),
                &name.id,
                ImportAliasResolution::ResolveAliases,
            )
            .into_iter()
            .filter_map(|resolved| match resolved {
                ResolvedDefinition::Definition(definition) => Some(definition),
                ResolvedDefinition::Module(_) | ResolvedDefinition::FileWithRange(_) => None,
            })
            .any(|definition| definition_is_environment_derived_inner(db, definition, visited)),
            ast::Expr::Attribute(attribute) => {
                let Some(receiver) = self.try_expression_type(&attribute.value) else {
                    return false;
                };
                member_definitions(db, self.scope(), receiver, &attribute.attr)
                    .into_iter()
                    .any(|definition| {
                        definition_is_environment_derived_inner(db, definition, visited)
                    })
            }
            _ => false,
        }
    }

    /// Whether `expr` reads a fact about the environment ty is checking *for*, as opposed to a
    /// value the program computes.
    ///
    /// `sys.version_info` has a type of its own, so it is recognised wherever it is bound.
    /// `sys.platform` and `os.name` are ordinary string literals by the time they have a type, so
    /// they are recognised by the module they are read off — a `from sys import platform` binding
    /// is indistinguishable from any other string and is missed. `TYPE_CHECKING` is recognised by
    /// its name, which is also all ty itself goes on when it gives the binding `Literal[True]`, so
    /// an import that renames it is missed as well.
    fn is_environment_fact(&self, expr: &ast::Expr) -> bool {
        let db = self.db();
        let is_version_info = |expr: &ast::Expr| {
            matches!(
                self.try_expression_type(expr),
                Some(Type::NominalInstance(instance)) if instance.is_sys_version_info()
            )
        };
        match expr {
            ast::Expr::Name(name) => name.id == "TYPE_CHECKING" || is_version_info(expr),
            ast::Expr::Attribute(attribute) => {
                if is_version_info(expr) {
                    return true;
                }
                let Some(Type::ModuleLiteral(module)) = self.try_expression_type(&attribute.value)
                else {
                    return false;
                };
                let module = module.module(db);
                match &*attribute.attr {
                    "version_info" | "platform" => module.is_known(db, KnownModule::Sys),
                    "name" => module.is_known(db, KnownModule::Os),
                    "TYPE_CHECKING" => {
                        module.is_known(db, KnownModule::Typing)
                            || module.is_known(db, KnownModule::TypingExtensions)
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    fn check_overlapping_condition(
        &mut self,
        test: &ast::Expr,
        root: &ast::Expr,
        polarity: ConditionPolarity,
    ) {
        let env = self.program_environment();
        if !self.context.is_lint_enabled(&OVERLAPPING_CONDITION) {
            return;
        }
        let db = self.db();
        let Some(tested) = self.try_expression_type(root) else {
            return;
        };
        let selected =
            selected_branch(db, env, tested, polarity, db.analysis_settings(self.file()));
        if !selected.conflates() {
            return;
        }

        let Some(builder) = self.context.report_lint(&OVERLAPPING_CONDITION, test) else {
            return;
        };
        let [leading @ .., last] = &*selected.kinds else {
            return;
        };
        let leading = leading
            .iter()
            .map(|kind| format!("`{}`", kind.part.display(db, env)))
            .collect::<Vec<_>>()
            .join(", ");
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "This condition does not distinguish between {leading} and `{}`",
            last.part.display(db, env)
        ));
        diagnostic.info(format_args!(
            "`{}` is tested for {}",
            tested.display(db, env),
            polarity.noun()
        ));
        diagnostic.help("Compare against the specific value instead of testing truthiness");
    }

    /// Check a comparison against a `True`/`False` literal whose other operand is already a
    /// `bool`, which says exactly what the operand says.
    pub(super) fn check_redundant_boolean_comparison(
        &self,
        left: &ast::Expr,
        right: &ast::Expr,
        left_ty: Type<'db>,
        right_ty: Type<'db>,
        op: ast::CmpOp,
        range: TextRange,
    ) {
        let env = self.program_environment();
        if !self.context.is_lint_enabled(&REDUNDANT_BOOLEAN_COMPARISON) {
            return;
        }
        let is_equality = match op {
            ast::CmpOp::Eq | ast::CmpOp::Is => true,
            ast::CmpOp::NotEq | ast::CmpOp::IsNot => false,
            _ => return,
        };
        let as_bool_literal = |expr: &ast::Expr| match expr {
            ast::Expr::BooleanLiteral(literal) => Some(literal.value),
            _ => None,
        };
        // `True == False` compares two constants, which is a deliberate constant, not a
        // redundant comparison
        let (operand_ty, literal) = match (as_bool_literal(left), as_bool_literal(right)) {
            (None, Some(literal)) => (left_ty, literal),
            (Some(literal), None) => (right_ty, literal),
            _ => return,
        };
        let db = self.db();
        if !operand_ty.is_subtype_of(db, env, KnownClass::Bool.to_instance(db, env)) {
            return;
        }

        let Some(builder) = self
            .context
            .report_lint(&REDUNDANT_BOOLEAN_COMPARISON, range)
        else {
            return;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "Comparison of a `bool` with `{}` is redundant",
            if literal { "True" } else { "False" }
        ));
        diagnostic.info(format_args!(
            "`{}` already is the value this comparison produces",
            operand_ty.display(db, env)
        ));
        if is_equality == literal {
            diagnostic.help("Test the operand directly");
        } else {
            diagnostic.help("Negate the operand with `not` instead");
        }
    }
}

/// One kind of value that reaches the selected branch.
struct SelectedKind<'db> {
    /// the class its values are instances of; two arms of the same kind are one entry
    class: ClassLiteral<'db>,
    /// the part of the arm that reaches the branch, as it should be spelled in the diagnostic
    part: Type<'db>,
}

/// What the branch a condition selects can contain.
struct SelectedBranch<'db> {
    kinds: Vec<SelectedKind<'db>>,
    /// some arm reaches the branch in its entirety
    has_whole: bool,
    /// some arm reaches it only in part — the rest of that arm goes the other way
    has_partial: bool,
}

impl SelectedBranch<'_> {
    /// Whether the branch conflates values a reader would expect it to tell apart.
    ///
    /// The bug is a value that is *unconditionally* in this branch sharing it with one that is
    /// only conditionally here: a sentinel meeting the falsy corner of a value that normally
    /// belongs to the other branch, as `None` meets `""` in `if not name:`. Two arms that are
    /// each only partly here — `if x:` over a `str | bytes` — conflate nothing that the union
    /// did not already conflate, and neither does a branch that holds whole arms only.
    fn conflates(&self) -> bool {
        self.kinds.len() >= 2 && self.has_whole && self.has_partial
    }
}

/// Work out what the branch `polarity` selects out of `tested` can contain.
///
/// Arms of the same class — or of two classes one of which derives the other — are one kind:
/// truthiness was never going to tell a `1` from a `2`, or a `list[A]` from a `list[B]`, and
/// nobody expected it to.
fn selected_branch<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    tested: Type<'db>,
    polarity: ConditionPolarity,
    settings: &AnalysisSettings,
) -> SelectedBranch<'db> {
    let mut selected = SelectedBranch {
        kinds: Vec::new(),
        has_whole: false,
        has_partial: false,
    };
    for_each_arm(db, env, tested, &mut |arm| {
        let Some(class) = arm_class(db, env, arm) else {
            return;
        };
        if is_exempt(db, arm, class, &settings.overlapping_condition_exempt_types) {
            return;
        }
        let truthiness = arm_truthiness(db, env, arm, settings);
        let whole = !truthiness.is_ambiguous();
        let part = match truthiness {
            Truthiness::AlwaysTrue if polarity == ConditionPolarity::Falsy => return,
            Truthiness::AlwaysFalse if polarity == ConditionPolarity::Truthy => return,
            Truthiness::Ambiguous => IntersectionBuilder::new(db, env)
                .add_positive(arm)
                .add_negative(polarity.rejects())
                .build(),
            _ => arm,
        };
        if part.is_never() {
            return;
        }
        if whole {
            selected.has_whole = true;
        } else {
            selected.has_partial = true;
        }
        match selected
            .kinds
            .iter_mut()
            .find(|kind| same_kind(db, env, kind.class, class))
        {
            // keep the most general class of the group, so which arm the message names does not
            // depend on the order the arms happen to be in
            Some(kind) if derives(db, env, kind.class, class) => {
                kind.class = class;
                kind.part = part;
            }
            Some(_) => {}
            None => selected.kinds.push(SelectedKind { class, part }),
        }
    });
    selected
}

/// Visit the union arms of `tested`, or `tested` itself when it has only the one.
///
/// An enum complement and an intersection with a finite alternative are unions in all but name.
fn for_each_arm<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    tested: Type<'db>,
    visit: &mut impl FnMut(Type<'db>),
) {
    if let Some(union) = tested.as_union_like(db) {
        for arm in union.elements(db) {
            visit(*arm);
        }
        return;
    }
    let alternatives = match tested {
        Type::EnumComplement(complement) => Some(complement.remaining_literal_union(db, env)),
        Type::Intersection(intersection) => intersection.finite_alternative_union(db, env),
        _ => None,
    };
    match alternatives {
        // the equality guard keeps a type that describes itself from recursing forever
        Some(alternatives) if alternatives != tested => for_each_arm(db, env, alternatives, visit),
        _ => visit(tested),
    }
}

fn arm_truthiness<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    arm: Type<'db>,
    settings: &AnalysisSettings,
) -> Truthiness {
    let truthiness = arm.bool(db, env);
    if truthiness.is_ambiguous()
        && settings.overlapping_condition_assume_truthy_instances
        && defines_no_truthiness(db, env, arm)
    {
        return Truthiness::AlwaysTrue;
    }
    truthiness
}

/// Whether `ty` is an instance that says nothing about its own truthiness.
///
/// Such an instance is truthy unless a subclass says otherwise, which is why ty calls it
/// ambiguous; `overlapping-condition-assume-truthy-instances` takes it at face value instead.
fn defines_no_truthiness<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> bool {
    matches!(ty, Type::NominalInstance(_) | Type::ProtocolInstance(_))
        && ["__bool__", "__len__"]
            .iter()
            .all(|dunder| matches!(ty.member(db, env, dunder).place, Place::Undefined))
}

/// The class whose instances a union arm holds, if it has one.
///
/// Two kinds of arm have none, and both are deliberately left out of the count. A gradual type
/// stands in for anything at all, so it is not a value the branch could have meant. And an
/// intersection is a remnant of some earlier narrowing whose truthiness ty models only
/// approximately: `str & ~AlwaysFalsy` cannot be falsy, but ty still calls it ambiguous, so
/// counting it would report a falsy branch that only ever holds `None`.
fn arm_class<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    arm: Type<'db>,
) -> Option<ClassLiteral<'db>> {
    if arm.is_intersection() {
        return None;
    }
    match arm.to_meta_type(db, env) {
        Type::ClassLiteral(class) => Some(class),
        Type::GenericAlias(alias) => Some(ClassLiteral::Static(alias.origin(db))),
        Type::SubclassOf(subclass_of) => subclass_of
            .subclass_of()
            .into_class(db, env)
            .map(|class| class.class_literal(db)),
        _ => None,
    }
}

/// Whether two arms hold the same kind of value, so that a condition was never going to tell them
/// apart in the first place.
///
/// Type arguments are deliberately out of scope: `list[A]` and `list[B]` are both lists, and a
/// truthiness test sees only that one of them is empty.
fn same_kind<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    left: ClassLiteral<'db>,
    right: ClassLiteral<'db>,
) -> bool {
    left == right || derives(db, env, left, right) || derives(db, env, right, left)
}

/// Whether `subclass` derives `base`, ignoring type arguments.
fn derives<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    subclass: ClassLiteral<'db>,
    base: ClassLiteral<'db>,
) -> bool {
    subclass != base
        && subclass.default_specialization(db).is_subclass_of(
            db,
            env,
            base.default_specialization(db),
        )
}

/// Whether the user has told us not to count this arm as distinct.
fn is_exempt<'db>(
    db: &'db dyn Db,
    arm: Type<'db>,
    class: ClassLiteral<'db>,
    exempt: &[Box<str>],
) -> bool {
    if exempt.is_empty() {
        return false;
    }
    let matches = |name: &str| exempt.iter().any(|entry| &**entry == name);
    if arm.is_none(db) && matches("None") {
        return true;
    }
    let qualified = class.qualified_name(db).to_string();
    matches(&qualified) || qualified.strip_prefix("builtins.").is_some_and(matches)
}
