//! basedpython-ui: the composition model's checks
//!
//! A `@composable` function describes a piece of ui as a function of the
//! observables it reads. Its body, the `once` content blocks written in it
//! (`Column:`, `Row:`, …) and the `local` blocks written in it (a keyed
//! `each`) all run *while composing*; a handler block, a lambda, a nested
//! `def` or an effect block written in it runs *later*, in response to an
//! event. That distinction — what runs during composition and what does not —
//! is what every check here is about:
//!
//! - a value that is not deeply immutable cannot be held in state
//!   (`mutable-state-value`)
//! - an in-place mutation written anywhere in a composable is invisible to it
//!   (`silent-mutation`)
//! - a state write while composing is a write to the frame being built
//!   (`state-write-in-composition`)
//! - a slot created under a condition lives as long as the condition
//!   (`conditional-slot`)
//! - a `return` in a content block nested in another block goes nowhere
//!   (`content-block-control-flow`)
//! - a composable with an unstable parameter is never skipped
//!   (`unstable-parameter`)
//! - a composable or builder called outside a composition has nothing to
//!   compose into (`composable-outside-composition`)
//! - a composition may only depend on what it can observe: a parameter, a
//!   global or a captured name it reads must be deeply immutable or an
//!   observable (`unobservable-dependency`)
//!
//! Every scope is checked once, from its own inference: a block is a scope of
//! its own, so what it is part of is found by walking *out* — through `once`
//! blocks that run inline, noting each callback boundary crossed — to the
//! composable whose composition it belongs to ([`composition_of_scope`]).

use ruff_db::diagnostic::{Annotation, Span};
use ruff_db::files::File;
use ruff_db::parsed::ParsedModuleRef;
use ruff_db::source::{SourceText, source_text};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_module_resolver::{KnownModule, resolve_module_confident};
use ty_python_core::SemanticIndex;
use ty_python_core::scope::{FileScopeId, NodeWithScopeKind};

use crate::Db;
use crate::types::context::InferContext;
use crate::types::dedicated::basedpython_ui::{
    ObservableKind, is_composable, is_composition_root, is_set_root, is_slot_function,
    is_widget_builder, observable_kind, state_list_element_type, state_value_type, underlying,
};
use crate::types::diagnostic::{
    COMPOSABLE_OUTSIDE_COMPOSITION, CONDITIONAL_SLOT, CONTENT_BLOCK_CONTROL_FLOW,
    MUTABLE_STATE_VALUE, SILENT_MUTATION, STATE_WRITE_IN_COMPOSITION, UNOBSERVABLE_DEPENDENCY,
    UNSTABLE_PARAMETER,
};
use crate::types::function::{FunctionType, KnownFunction};
use crate::types::immutability::{
    is_builtin_mutable_container, is_deeply_immutable, is_stable_parameter_type, is_write_projected,
};
use crate::types::trailing_lambda::{
    block_callee, callee_callback_is_borrowed, callee_callback_is_once,
};
use crate::types::{
    KnownClass, ProgramEnvironment, Type, TypeContext, TypeQualifiers, infer_definition_types,
    infer_expression_types, infer_scope_types,
};

/// the methods of the builtin mutable containers that change them in place
const CONTAINER_MUTATORS: &[&str] = &[
    "append",
    "extend",
    "insert",
    "pop",
    "remove",
    "clear",
    "sort",
    "reverse",
    "update",
    "setdefault",
    "popitem",
    "add",
    "discard",
    "appendleft",
    "popleft",
    "rotate",
    "__iadd__",
    "__imul__",
    "__ior__",
    "__iand__",
    "__isub__",
    "__ixor__",
];

/// basedpython-ui entry point: check the scope `context` is inferring.
///
/// `expression_type` supplies the scope's own inferred types. It is a callback
/// so the check can read the in-progress inference rather than re-enter it as a
/// query.
pub(crate) fn check_scope<'db, 'ast>(
    context: &InferContext<'db, 'ast>,
    index: &'ast SemanticIndex<'db>,
    expression_type: impl Fn(&Expr) -> Option<Type<'db>>,
) {
    let db = context.db();
    let env = context.program_environment();
    let module = context.module();
    let scope = context.scope().file_scope_id(db);
    let node = context.scope().node(db);

    // a `once` block nested in another block: its `return` goes nowhere. this
    // is a property of the language, not of the framework, so it is checked
    // whether or not the framework is in the program
    if let NodeWithScopeKind::Function(function) = node
        && let function = function.node(module)
        && function.is_trailing_lambda
    {
        check_nested_content_block_control_flow(context, index, module, scope, function);
    }

    // every other check is about the framework's observables and scopes:
    // nothing to say unless the framework resolves in this program
    if resolve_module_confident(
        db,
        env.resolver_environment(db),
        &KnownModule::BasedpythonUiRuntime.name(),
    )
    .is_none()
    {
        return;
    }

    let composition = composition_of_scope(db, context.file(), index, module, scope);

    if let Some(composition) = &composition
        && composition.owner_scope == scope
        && let CompositionOwner::Composable(composable) = composition.owner
        && let NodeWithScopeKind::Function(function) = node
    {
        check_parameter_stability(context, composable, function.node(module));
    }

    let mut checker = CompositionChecker {
        context,
        index,
        module,
        source: source_text(db, context.file()),
        scope,
        composition,
        expression_type,
        conditional_depth: 0,
    };
    match node {
        NodeWithScopeKind::Module => checker.visit_body(&module.syntax().body),
        NodeWithScopeKind::Function(function) => checker.visit_body(&function.node(module).body),
        NodeWithScopeKind::Class(class) => checker.visit_body(&class.node(module).body),
        NodeWithScopeKind::Lambda(lambda) => checker.visit_expr(&lambda.node(module).body),
        // the first iterable of a comprehension is evaluated in the enclosing
        // scope; everything else is this scope's
        NodeWithScopeKind::ListComprehension(comprehension) => {
            let comprehension = comprehension.node(module);
            checker.visit_expr(&comprehension.elt);
            checker.visit_own_generators(&comprehension.generators);
        }
        NodeWithScopeKind::SetComprehension(comprehension) => {
            let comprehension = comprehension.node(module);
            checker.visit_expr(&comprehension.elt);
            checker.visit_own_generators(&comprehension.generators);
        }
        NodeWithScopeKind::DictComprehension(comprehension) => {
            let comprehension = comprehension.node(module);
            if let Some(key) = comprehension.key.as_deref() {
                checker.visit_expr(key);
            }
            checker.visit_expr(&comprehension.value);
            checker.visit_own_generators(&comprehension.generators);
        }
        NodeWithScopeKind::GeneratorExpression(generator) => {
            let generator = generator.node(module);
            checker.visit_expr(&generator.elt);
            checker.visit_own_generators(&generator.generators);
        }
        NodeWithScopeKind::ClassTypeParameters(_)
        | NodeWithScopeKind::FunctionTypeParameters(_)
        | NodeWithScopeKind::TypeAliasTypeParameters(_)
        | NodeWithScopeKind::TypeAlias(_) => {}
    }
}

// ---------------------------------------------------------------------------
// what a scope is part of
// ---------------------------------------------------------------------------

/// the framework entry points whose `root` argument is where a composition
/// starts
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootEntry {
    /// `run_app(...)`: the root block of a windowed app
    RunApp,
    /// `compose_test(...)`: the root block of a headless test
    ComposeTest,
    /// `Runtime.set_root(...)`: the runtime's own entry point, which the two
    /// above wrap; a test or benchmark hands it a lambda or a function
    SetRoot,
}

impl RootEntry {
    /// the entry point of a known function, if it is one
    fn from_known(known: KnownFunction) -> Option<Self> {
        match known {
            KnownFunction::BasedpythonUiRunApp => Some(Self::RunApp),
            KnownFunction::BasedpythonUiComposeTest => Some(Self::ComposeTest),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::RunApp => "run_app",
            Self::ComposeTest => "compose_test",
            Self::SetRoot => "set_root",
        }
    }
}

/// what a composition belongs to
#[derive(Clone, Copy)]
pub(crate) enum CompositionOwner<'db> {
    /// a `@composable` function: its body is the composition
    Composable(FunctionType<'db>),
    /// the `root` of an entry point, where a composition starts without a
    /// composable of its own
    Root(RootEntry),
}

impl CompositionOwner<'_> {
    /// how a message names the owner: "`Counter`", "the `run_app` root"
    fn describe(self, db: &dyn Db) -> String {
        match self {
            Self::Composable(function) => format!("`{}`", function.name(db)),
            Self::Root(entry) => format!("the `{}` root", entry.name()),
        }
    }
}

/// when a scope runs, relative to the composition it takes part in. Ordered
/// from the most to the least known: what a scope inherits from the scopes
/// between it and its owner is the worst timing crossed on the way
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Timing {
    /// while composing, exactly as often as the owner: the owner's body, or a
    /// chain of `once` blocks written in it
    #[default]
    Inline,
    /// while composing, but any number of times: a `local` block (a keyed
    /// `each`) was crossed
    Local,
    /// unknown: a block whose callee could not be resolved was crossed
    Unknown,
    /// after composition: a deferred callback — a handler block, a lambda, a
    /// nested `def` — was crossed
    Deferred,
}

/// the composition a scope takes part in, and how it got there
pub(crate) struct Composition<'db> {
    pub(crate) owner: CompositionOwner<'db>,
    /// the scope of the owner's body — the composable's, or the root block's
    pub(crate) owner_scope: FileScopeId,
    /// where the owner is declared, for a secondary annotation
    owner_span: Span,
    /// when the scope runs, relative to the owner's composition
    timing: Timing,
    /// when what the scope *reads* is read, relative to the owner's
    /// composition: as `timing`, except that the lambda given to `derived` /
    /// `remember` reads on behalf of the composition that made it — what it
    /// reads is what the computation depends on — while for every other
    /// purpose it still runs later
    read_timing: Timing,
    /// the scope is reached through a content block written under a
    /// condition, or through a comprehension: what it does happens only
    /// sometimes
    conditional: bool,
}

impl Composition<'_> {
    /// where the owner is declared: the composable's signature, or the
    /// argument an entry point was handed as its root
    pub(crate) fn owner_range(&self) -> Option<TextRange> {
        self.owner_span.range()
    }

    /// whether the scope runs while its owner is composing — nothing that runs
    /// later, and nothing whose timing is unknown
    pub(crate) fn runs_while_composing(&self) -> bool {
        self.timing <= Timing::Local
    }

    /// whether what the scope reads is read while its owner is composing: a
    /// dependency of the composition, which must be something it can observe
    fn reads_while_composing(&self) -> bool {
        self.read_timing <= Timing::Local
    }

    /// whether the scope runs exactly once per composition of its owner: it
    /// runs while composing, unconditionally, and through `once` blocks alone
    fn runs_once_per_composition(&self) -> bool {
        self.timing == Timing::Inline && !self.conditional
    }

    /// whether the scope runs after its owner has composed, when there is no
    /// composition to build into
    pub(crate) fn runs_after_composing(&self) -> bool {
        self.timing == Timing::Deferred
    }
}

/// what a trailing-lambda block's callee makes of the block
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockKind {
    /// a `once` callback: the block runs inline, exactly once
    Once,
    /// a `local` callback: the block runs before the call returns, any number
    /// of times (a keyed `each`)
    Local,
    /// an unborrowed callback: the callee may keep the block and run it later
    Deferred,
    /// the callee cannot be resolved
    Opaque,
    /// the `root` of `run_app` / `compose_test`: a composition of its own
    Root(RootEntry),
}

/// what `block`'s callee makes of it
pub(crate) fn block_kind<'db>(
    db: &'db dyn Db,
    index: &SemanticIndex<'db>,
    block: &ast::StmtFunctionDef,
) -> BlockKind {
    let Some(callee) = block_callee(db, index, block) else {
        return BlockKind::Opaque;
    };
    if let Type::FunctionLiteral(function) = callee.ty
        && let Some(known) = function.known(db)
        && is_composition_root(known)
        && let Some(entry) = RootEntry::from_known(known)
    {
        return BlockKind::Root(entry);
    }
    if callee_callback_is_once(db, callee.ty) {
        return BlockKind::Once;
    }
    match callee_callback_is_borrowed(db, callee.ty) {
        Some(true) => BlockKind::Local,
        Some(false) => BlockKind::Deferred,
        None => BlockKind::Opaque,
    }
}

/// the composition `scope` takes part in, found by walking out to the
/// composable (or root block) whose composition it is, noting what is crossed
/// on the way. `None` when nothing encloses it but the module or a class.
///
/// `file` is the file `index` and `module` belong to. Everything here is read
/// from salsa queries — a definition's inferred type, a standalone expression's
/// — never from the inference of the scope being checked, so the walk can be
/// asked from inside that inference (the checks) and from outside it (the
/// editor's invalidation hint) alike
pub(crate) fn composition_of_scope<'db, 'ast>(
    db: &'db dyn Db,
    file: File,
    index: &'ast SemanticIndex<'db>,
    module: &'ast ParsedModuleRef,
    scope: FileScopeId,
) -> Option<Composition<'db>> {
    let mut conditional = false;
    let mut timing = Timings::default();
    // the range of the scope we came from, to see whether it sits under a
    // condition in the scope now being looked at
    let mut child: Option<TextRange> = None;
    let composition = |owner, owner_scope, owner_span, timing: Timings, conditional| Composition {
        owner,
        owner_scope,
        owner_span,
        timing: timing.run,
        read_timing: timing.read,
        conditional,
    };

    for (id, ancestor) in index.ancestor_scopes(scope) {
        match ancestor.node() {
            NodeWithScopeKind::Function(function) => {
                let function = function.node(module);
                if let Some(child) = child {
                    conditional |= statement_is_conditional(&function.body, child);
                }
                if function.is_trailing_lambda {
                    match block_kind(db, index, function) {
                        BlockKind::Once => {}
                        BlockKind::Local => timing.cross(Timing::Local),
                        BlockKind::Deferred => timing.cross(Timing::Deferred),
                        BlockKind::Opaque => timing.cross(Timing::Unknown),
                        BlockKind::Root(entry) => {
                            let callee = function.trailing_lambda_callee()?;
                            return Some(composition(
                                CompositionOwner::Root(entry),
                                id,
                                Span::from(file).with_range(callee.range()),
                                timing,
                                conditional,
                            ));
                        }
                    }
                } else {
                    let definition = index.expect_single_definition(function);
                    if let Some(composable) =
                        infer_definition_types(db, definition).function_type(definition)
                        && is_composable(db, composable)
                    {
                        return Some(composition(
                            CompositionOwner::Composable(composable),
                            id,
                            composable.spans(db).signature,
                            timing,
                            conditional,
                        ));
                    }
                    // a plain `def` handed to the runtime as its root is a
                    // composition of its own; any other runs when called
                    if let Some(span) = set_root_argument(
                        db,
                        file,
                        index,
                        module,
                        id,
                        ScopeArgument::Named(function.name.as_str()),
                    ) {
                        return Some(composition(
                            CompositionOwner::Root(RootEntry::SetRoot),
                            id,
                            span,
                            timing,
                            conditional,
                        ));
                    }
                    timing.cross(Timing::Deferred);
                }
                child = Some(function.range());
            }
            NodeWithScopeKind::Lambda(lambda) => {
                let lambda = lambda.node(module);
                if let Some(span) = set_root_argument(
                    db,
                    file,
                    index,
                    module,
                    id,
                    ScopeArgument::Lambda(lambda.range()),
                ) {
                    return Some(composition(
                        CompositionOwner::Root(RootEntry::SetRoot),
                        id,
                        span,
                        timing,
                        conditional,
                    ));
                }
                // the lambda given to `derived` / `remember` runs later too,
                // but what it reads is what the computation depends on: a
                // read of the composition that made it
                if computation_kind(db, index, module, id, lambda.range()).is_some() {
                    timing.run = timing.run.max(Timing::Deferred);
                } else {
                    timing.cross(Timing::Deferred);
                }
                child = Some(lambda.range());
            }
            NodeWithScopeKind::ListComprehension(comprehension) => {
                conditional = true;
                child = Some(comprehension.node(module).range());
            }
            NodeWithScopeKind::SetComprehension(comprehension) => {
                conditional = true;
                child = Some(comprehension.node(module).range());
            }
            NodeWithScopeKind::DictComprehension(comprehension) => {
                conditional = true;
                child = Some(comprehension.node(module).range());
            }
            NodeWithScopeKind::GeneratorExpression(generator) => {
                conditional = true;
                child = Some(generator.node(module).range());
            }
            NodeWithScopeKind::Module | NodeWithScopeKind::Class(_) => return None,
            NodeWithScopeKind::ClassTypeParameters(_)
            | NodeWithScopeKind::FunctionTypeParameters(_)
            | NodeWithScopeKind::TypeAliasTypeParameters(_)
            | NodeWithScopeKind::TypeAlias(_) => {}
        }
    }
    None
}

/// the timings accumulated while walking out of a scope: when the scope runs,
/// and when what it reads is read (see [`Composition::read_timing`])
#[derive(Clone, Copy, Default)]
struct Timings {
    run: Timing,
    read: Timing,
}

impl Timings {
    /// a callback boundary crossed on the way out, for running and reading
    /// alike
    fn cross(&mut self, timing: Timing) {
        self.run = self.run.max(timing);
        self.read = self.read.max(timing);
    }
}

/// how a scope can be the argument of a call in its enclosing scope
#[derive(Clone, Copy)]
enum ScopeArgument<'a> {
    /// a lambda written as the argument, identified by its range
    Lambda(TextRange),
    /// a function passed by name
    Named(&'a str),
}

impl ScopeArgument<'_> {
    /// whether `value`, an argument as written, is this scope
    fn matches(self, value: &Expr) -> bool {
        match self {
            Self::Lambda(range) => value.range() == range,
            Self::Named(name) => matches!(value, Expr::Name(value) if value.id.as_str() == name),
        }
    }
}

/// the calls whose argument a scope can be. Each is found syntactically in
/// the scope's enclosing scope, and its callee is read from the standalone
/// inference the semantic index registers for exactly these calls — so the
/// argument's scope learns whom it was handed to without re-entering the
/// enclosing scope's inference, which may itself be waiting on this scope's,
/// for a lambda's return type
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArgumentOf {
    /// `<receiver>.set_root(<root>)`
    SetRoot,
    /// `derived(<compute>)` / `remember(<compute>)`
    Computation,
}

impl ArgumentOf {
    /// whether `func` is spelled like this call's callee — the same shapes
    /// the semantic index registers
    fn is_callee(self, func: &Expr) -> bool {
        match self {
            Self::SetRoot => {
                matches!(func, Expr::Attribute(attribute) if attribute.attr.as_str() == "set_root")
            }
            Self::Computation => {
                let name = match func {
                    Expr::Name(name) => name.id.as_str(),
                    Expr::Attribute(attribute) => attribute.attr.as_str(),
                    _ => return false,
                };
                matches!(name, "derived" | "remember")
            }
        }
    }

    /// the parameter the argument fills: its name and its position
    const fn parameter(self) -> (&'static str, usize) {
        match self {
            Self::SetRoot => ("root", 0),
            Self::Computation => ("compute", 0),
        }
    }
}

/// The call of kind `of` in the scope enclosing `scope` whose argument is
/// `argument` — this scope, as a lambda or a name: the callee's type and the
/// argument as written.
fn enclosing_call_argument<'db, 'ast>(
    db: &'db dyn Db,
    index: &'ast SemanticIndex<'db>,
    module: &'ast ParsedModuleRef,
    scope: FileScopeId,
    of: ArgumentOf,
    argument: ScopeArgument<'_>,
) -> Option<(Type<'db>, &'ast Expr)> {
    let parent = index.parent_scope(scope)?;
    let mut finder = CallArgumentFinder {
        of,
        argument,
        found: None,
    };
    match parent.node() {
        NodeWithScopeKind::Function(function) => finder.visit_body(&function.node(module).body),
        NodeWithScopeKind::Module => finder.visit_body(&module.syntax().body),
        NodeWithScopeKind::Class(class) => finder.visit_body(&class.node(module).body),
        NodeWithScopeKind::Lambda(lambda) => finder.visit_expr(&lambda.node(module).body),
        _ => return None,
    }
    let (call, value) = finder.found?;
    let expression = index.try_expression(call.func.as_ref())?;
    let callee = infer_expression_types(db, expression, TypeContext::default())
        .try_expression_type(call.func.as_ref())?;
    Some((callee, value))
}

/// Whether the scope `scope` is handed to `Runtime.set_root` as its `root` —
/// `rt.set_root(lambda: App())`, `rt.set_root(root)` — in which case the
/// argument's span is returned.
fn set_root_argument<'db, 'ast>(
    db: &'db dyn Db,
    file: File,
    index: &'ast SemanticIndex<'db>,
    module: &'ast ParsedModuleRef,
    scope: FileScopeId,
    argument: ScopeArgument<'_>,
) -> Option<Span> {
    let (callee, root) =
        enclosing_call_argument(db, index, module, scope, ArgumentOf::SetRoot, argument)?;
    let Type::BoundMethod(method) = callee else {
        return None;
    };
    is_set_root(db, method.function(db)).then(|| Span::from(file).with_range(root.range()))
}

/// The computation the lambda scope `scope`, written at `lambda`, is the
/// `compute` argument of: `BasedpythonUiDerived` for `derived(lambda: ...)`,
/// `BasedpythonUiRemember` for `remember(lambda: ...)`. Both are computations
/// whose reads are dependencies of the composition that made them. `None` for
/// a lambda written anywhere else.
pub(crate) fn computation_kind<'db, 'ast>(
    db: &'db dyn Db,
    index: &'ast SemanticIndex<'db>,
    module: &'ast ParsedModuleRef,
    scope: FileScopeId,
    lambda: TextRange,
) -> Option<KnownFunction> {
    let (callee, _) = enclosing_call_argument(
        db,
        index,
        module,
        scope,
        ArgumentOf::Computation,
        ScopeArgument::Lambda(lambda),
    )?;
    let Type::FunctionLiteral(function) = callee else {
        return None;
    };
    match function.known(db) {
        known
        @ Some(KnownFunction::BasedpythonUiDerived | KnownFunction::BasedpythonUiRemember) => known,
        _ => None,
    }
}

/// Finds the call of one kind in a scope's own statements whose argument is a
/// given lambda or name.
struct CallArgumentFinder<'a, 'ast> {
    of: ArgumentOf,
    argument: ScopeArgument<'a>,
    found: Option<(&'ast ast::ExprCall, &'ast Expr)>,
}

impl<'ast> Visitor<'ast> for CallArgumentFinder<'_, 'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if self.found.is_some() {
            return;
        }
        match stmt {
            // a nested scope's statements are not this scope's; the call a
            // trailing-lambda block makes, and any decorator, are
            Stmt::FunctionDef(function) => {
                for decorator in &function.decorator_list {
                    self.visit_expr(&decorator.expression);
                }
            }
            Stmt::ClassDef(_) => {}
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if self.found.is_some() {
            return;
        }
        match expr {
            Expr::Lambda(_) => return,
            Expr::Call(call) if self.of.is_callee(&call.func) => {
                let (name, position) = self.of.parameter();
                if let Some(value) = call.arguments.find_argument_value(name, position)
                    && self.argument.matches(value)
                {
                    self.found = Some((call, value));
                    return;
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

/// whether the statement at `target` sits under a conditional statement —
/// `if`, `for`, `while`, `try`, `match` — somewhere in `body`
fn statement_is_conditional(body: &[Stmt], target: TextRange) -> bool {
    let Some(stmt) = body.iter().find(|stmt| stmt.range().contains_range(target)) else {
        return false;
    };
    if stmt.range() == target {
        return false;
    }
    match stmt {
        // a `finally` body runs however the `try` exited, so what it holds is
        // reached as unconditionally as the `try` statement itself
        Stmt::Try(try_stmt)
            if try_stmt
                .finalbody
                .iter()
                .any(|stmt| stmt.range().contains_range(target)) =>
        {
            statement_is_conditional(&try_stmt.finalbody, target)
        }
        Stmt::If(_) | Stmt::For(_) | Stmt::While(_) | Stmt::Try(_) | Stmt::Match(_) => true,
        Stmt::With(with) => statement_is_conditional(&with.body, target),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// `content-block-control-flow`
// ---------------------------------------------------------------------------

/// A `once` block's `return` leaves the scope the block is written in — but
/// only that one. When that scope is itself a block, the `return` leaves the
/// inner block and stops: the enclosing function keeps running and the value
/// is discarded. Report each such `return`.
fn check_nested_content_block_control_flow<'db, 'ast>(
    context: &InferContext<'db, 'ast>,
    index: &'ast SemanticIndex<'db>,
    module: &'ast ParsedModuleRef,
    scope: FileScopeId,
    block: &'ast ast::StmtFunctionDef,
) {
    if block_kind(context.db(), index, block) != BlockKind::Once {
        return;
    }
    let nested_in_block = index.parent_scope(scope).is_some_and(|parent| {
        matches!(
            parent.node(),
            NodeWithScopeKind::Function(function) if function.node(module).is_trailing_lambda
        )
    });
    if !nested_in_block {
        return;
    }
    // the scope the `return` was written to leave: the nearest enclosing
    // function that is not a block
    let target = index
        .ancestor_scopes(scope)
        .skip(1)
        .find_map(|(_, ancestor)| match ancestor.node() {
            NodeWithScopeKind::Function(function) => {
                let function = function.node(module);
                (!function.is_trailing_lambda).then(|| format!("`{}`", function.name))
            }
            NodeWithScopeKind::Lambda(_) => Some("the enclosing lambda".to_owned()),
            NodeWithScopeKind::Module => Some("the module".to_owned()),
            _ => None,
        })
        .unwrap_or_else(|| "the enclosing scope".to_owned());
    NestedReturnChecker { context, target }.visit_body(&block.body);
}

struct NestedReturnChecker<'a, 'db, 'ast> {
    context: &'a InferContext<'db, 'ast>,
    /// how the message names the scope the `return` cannot leave
    target: String,
}

impl<'ast> Visitor<'ast> for NestedReturnChecker<'_, '_, 'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            // a nested function / class is its own `return` target; a nested
            // block is checked from its own scope
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            Stmt::Return(ret) => {
                if let Some(builder) = self.context.report_lint(&CONTENT_BLOCK_CONTROL_FLOW, ret) {
                    builder.into_diagnostic(format_args!(
                        "`return` inside a nested content block leaves only the block; \
                         it cannot leave {}",
                        self.target
                    ));
                }
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, _expr: &'ast Expr) {}
}

// ---------------------------------------------------------------------------
// `unstable-parameter`
// ---------------------------------------------------------------------------

/// Report each parameter of `composable` whose declared type is not stable:
/// the runtime cannot compare such an argument, so the composable is never
/// skipped.
fn check_parameter_stability<'db, 'ast>(
    context: &InferContext<'db, 'ast>,
    composable: FunctionType<'db>,
    function: &'ast ast::StmtFunctionDef,
) {
    let db = context.db();
    let env = context.program_environment();
    let signature = composable.signature(db);
    let Some(signature) = signature.overloads.last() else {
        return;
    };
    for parameter in function.parameters.iter_non_variadic_params() {
        let node = &parameter.parameter;
        if node.annotation.is_none() {
            continue;
        }
        let Some(declared) = signature
            .parameters()
            .iter()
            .find(|declared| declared.name() == Some(&node.name.id))
        else {
            continue;
        };
        let ty = declared.annotated_type();
        if is_stable_parameter_type(db, env, ty) {
            continue;
        }
        let Some(builder) = context.report_lint(&UNSTABLE_PARAMETER, node) else {
            continue;
        };
        builder.into_diagnostic(format_args!(
            "`{name}: {ty}` is unstable, so `{composable}` is never skipped; prefer {alternatives}",
            name = node.name.id,
            ty = ty.display(db, env),
            composable = composable.name(db),
            alternatives = stable_alternatives(db, env, ty, frozen_record(db, context.file())),
        ));
    }
}

/// how a message spells "a frozen record" for the file it is reported in:
/// `.py` has no `frozen data class`, and telling a python author to write one
/// sends them looking for syntax their file cannot use
fn frozen_record(db: &dyn Db, file: ruff_db::files::File) -> &'static str {
    if file.source_type(db).is_basedpython() {
        "a `frozen data class`"
    } else {
        "a `@dataclass(frozen=True)`"
    }
}

/// the stable spellings a message suggests for an unstable parameter type. A
/// read-only view (`list[out int]`) is stable, but is not suggested: it is
/// still an `unobservable-dependency` when read while composing
fn stable_alternatives<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    frozen: &str,
) -> String {
    match container_shape(db, env, ty) {
        Some(ContainerShape::List(element)) => {
            format!(
                "`tuple[{}, ...]`, `state_list`, or {frozen}",
                element.display(db, env)
            )
        }
        Some(ContainerShape::Set(element)) => {
            format!("`frozenset[{}]` or `state_list`", element.display(db, env))
        }
        Some(ContainerShape::Dict) => format!("`state_dict` or {frozen}"),
        None => format!("{frozen} or an observable"),
    }
}

/// the builtin container a type is, when the spellings a message suggests
/// depend on it
#[derive(Clone, Copy)]
enum ContainerShape<'db> {
    /// `list[T]`, with its element type
    List(Type<'db>),
    /// `set[T]`, with its element type
    Set(Type<'db>),
    /// `dict[K, V]`
    Dict,
}

fn container_shape<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<ContainerShape<'db>> {
    // the shape a message names is the shape inside any use-site restriction:
    // `final list[str]` should still be told to prefer `tuple[str, ...]`
    let ty = underlying(db, ty);
    if let Some(specialization) = ty.known_specialization(db, env, KnownClass::List)
        && let [element] = specialization.types(db)
    {
        return Some(ContainerShape::List(*element));
    }
    if let Some(specialization) = ty.known_specialization(db, env, KnownClass::Set)
        && let [element] = specialization.types(db)
    {
        return Some(ContainerShape::Set(*element));
    }
    if let Some(specialization) = ty.known_specialization(db, env, KnownClass::Dict)
        && let [_, _] = specialization.types(db)
    {
        return Some(ContainerShape::Dict);
    }
    None
}

// ---------------------------------------------------------------------------
// `unobservable-dependency`
// ---------------------------------------------------------------------------

/// what a name a composition reads, but did not bind itself, is to it
#[derive(Clone, Copy, PartialEq, Eq)]
enum DependencyKind {
    /// a parameter of the composable, which its caller fills
    Parameter,
    /// a module-level name
    Global,
    /// a local of a function enclosing the composition
    Captured,
}

/// how a message suggests making a dependency of type `ty` observable: the
/// state constructor that holds a value of its shape, the immutable spelling
/// of it, or — for a value with neither — the frozen record or observable
/// that should replace it
fn observable_alternatives<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    kind: DependencyKind,
    frozen: &str,
) -> String {
    let (holder, immutable) = match container_shape(db, env, ty) {
        Some(ContainerShape::List(element)) => (
            "`state_list`",
            format!("`tuple[{}, ...]`, {frozen}", element.display(db, env)),
        ),
        Some(ContainerShape::Set(element)) => (
            "`state_list`",
            format!("`frozenset[{}]`", element.display(db, env)),
        ),
        Some(ContainerShape::Dict) => ("`state_dict`", frozen.to_owned()),
        None => {
            return match kind {
                DependencyKind::Parameter => {
                    format!("pass {frozen} or an observable, or read it only in a handler")
                }
                DependencyKind::Global | DependencyKind::Captured => {
                    format!("make it {frozen} or an observable, or read it only in a handler")
                }
            };
        }
    };
    match kind {
        DependencyKind::Parameter => format!(
            "hold it in state ({holder}), pass an immutable value ({immutable}), \
             or read it only in a handler"
        ),
        DependencyKind::Global | DependencyKind::Captured => {
            format!("hold it in state ({holder}), make it immutable, or read it only in a handler")
        }
    }
}

// ---------------------------------------------------------------------------
// the walk over a scope's body
// ---------------------------------------------------------------------------

struct CompositionChecker<'a, 'db, 'ast, F> {
    context: &'a InferContext<'db, 'ast>,
    index: &'ast SemanticIndex<'db>,
    module: &'ast ParsedModuleRef,
    source: SourceText,
    /// the scope being checked
    scope: FileScopeId,
    /// the composition the scope takes part in, if any
    composition: Option<Composition<'db>>,
    expression_type: F,
    /// how many conditional constructs enclose what is being visited
    conditional_depth: usize,
}

impl<'db, 'ast, F> Visitor<'ast> for CompositionChecker<'_, 'db, 'ast, F>
where
    F: Fn(&Expr) -> Option<Type<'db>>,
{
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            // the block's call is made here; its body is a scope of its own
            Stmt::FunctionDef(function) if function.is_trailing_lambda => {
                self.visit_block_call(function);
            }
            // a nested function does not run where it is defined: its
            // decorators and defaults do
            Stmt::FunctionDef(function) => {
                for decorator in &function.decorator_list {
                    self.visit_expr(&decorator.expression);
                }
                for default in function
                    .parameters
                    .iter_non_variadic_params()
                    .filter_map(|parameter| parameter.default.as_deref())
                {
                    self.visit_expr(default);
                }
            }
            Stmt::ClassDef(class) => {
                for decorator in &class.decorator_list {
                    self.visit_expr(&decorator.expression);
                }
                for base in class.bases() {
                    self.visit_expr(base);
                }
                for keyword in class.keywords() {
                    self.visit_expr(&keyword.value);
                }
            }
            Stmt::Assign(assign) => {
                // the assigned value is known only for a single target
                let value = match assign.targets.as_slice() {
                    [_] => Some(&*assign.value),
                    _ => None,
                };
                for target in &assign.targets {
                    self.check_store(target, value);
                }
                walk_stmt(self, stmt);
            }
            Stmt::AnnAssign(assign) => {
                if let Some(value) = assign.value.as_deref() {
                    self.check_store(&assign.target, Some(value));
                }
                walk_stmt(self, stmt);
            }
            Stmt::AugAssign(assign) => {
                self.check_augmented_store(assign);
                walk_stmt(self, stmt);
            }
            Stmt::Delete(delete) => {
                for target in &delete.targets {
                    self.check_delete(target);
                }
                walk_stmt(self, stmt);
            }
            Stmt::If(if_stmt) => {
                self.visit_expr(&if_stmt.test);
                self.conditional(|this| {
                    this.visit_body(&if_stmt.body);
                    for clause in &if_stmt.elif_else_clauses {
                        if let Some(test) = &clause.test {
                            this.visit_expr(test);
                        }
                        this.visit_body(&clause.body);
                    }
                });
            }
            Stmt::For(for_stmt) => {
                self.visit_expr(&for_stmt.iter);
                self.conditional(|this| {
                    this.visit_expr(&for_stmt.target);
                    this.visit_body(&for_stmt.body);
                    this.visit_body(&for_stmt.orelse);
                });
            }
            // everything but `finally` depends on how the body exited; the
            // `finally` body runs whatever happened above it, so a slot in one
            // runs exactly as often as the statement does
            Stmt::Try(try_stmt) => {
                self.conditional(|this| {
                    this.visit_body(&try_stmt.body);
                    for handler in &try_stmt.handlers {
                        let ast::ExceptHandler::ExceptHandler(handler) = handler;
                        if let Some(kind) = handler.type_.as_deref() {
                            this.visit_expr(kind);
                        }
                        this.visit_body(&handler.body);
                    }
                    this.visit_body(&try_stmt.orelse);
                });
                self.visit_body(&try_stmt.finalbody);
            }
            Stmt::While(_) | Stmt::Match(_) => {
                self.conditional(|this| walk_stmt(this, stmt));
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            // a lambda body does not run where it is written; its defaults do
            Expr::Lambda(lambda) => {
                for default in lambda
                    .parameters
                    .iter()
                    .flat_map(|parameters| parameters.iter_non_variadic_params())
                    .filter_map(|parameter| parameter.default.as_deref())
                {
                    self.visit_expr(default);
                }
            }
            // only a comprehension's first iterable is evaluated here; the
            // rest is a scope of its own
            Expr::ListComp(comprehension) => self.visit_first_iterable(&comprehension.generators),
            Expr::SetComp(comprehension) => self.visit_first_iterable(&comprehension.generators),
            Expr::DictComp(comprehension) => self.visit_first_iterable(&comprehension.generators),
            Expr::Generator(generator) => self.visit_first_iterable(&generator.generators),
            Expr::Call(call) => {
                self.check_call(expr, call);
                walk_expr(self, expr);
            }
            Expr::Name(name) if name.ctx.is_load() => self.check_dependency(expr, name),
            Expr::If(if_expr) => {
                self.visit_expr(&if_expr.test);
                self.conditional(|this| {
                    this.visit_expr(&if_expr.body);
                    this.visit_expr(&if_expr.orelse);
                });
            }
            Expr::BoolOp(bool_op) => {
                if let Some((first, rest)) = bool_op.values.split_first() {
                    self.visit_expr(first);
                    self.conditional(|this| {
                        for value in rest {
                            this.visit_expr(value);
                        }
                    });
                }
            }
            _ => walk_expr(self, expr),
        }
    }
}

impl<'db, 'ast, F> CompositionChecker<'_, 'db, 'ast, F>
where
    F: Fn(&Expr) -> Option<Type<'db>>,
{
    fn db(&self) -> &'db dyn Db {
        self.context.db()
    }

    fn env(&self) -> &'ast ProgramEnvironment<'db> {
        self.context.program_environment()
    }

    fn type_of(&self, expr: &Expr) -> Option<Type<'db>> {
        (self.expression_type)(expr)
    }

    /// the source text of `ranged`, for naming an expression in a message
    fn text(&self, range: TextRange) -> &str {
        &self.source[range]
    }

    /// visit with one more conditional construct enclosing what is visited
    fn conditional(&mut self, visit: impl FnOnce(&mut Self)) {
        self.conditional_depth += 1;
        visit(self);
        self.conditional_depth -= 1;
    }

    fn visit_first_iterable(&mut self, generators: &'ast [ast::Comprehension]) {
        if let Some(first) = generators.first() {
            self.visit_expr(&first.iter);
        }
    }

    /// the parts of a comprehension's generators that belong to the
    /// comprehension's own scope: every iterable but the first, and the
    /// conditions
    fn visit_own_generators(&mut self, generators: &'ast [ast::Comprehension]) {
        for (position, generator) in generators.iter().enumerate() {
            if position > 0 {
                self.visit_expr(&generator.iter);
            }
            for condition in &generator.ifs {
                self.visit_expr(condition);
            }
        }
    }

    /// the call a trailing-lambda block makes, which belongs to this scope.
    /// `f(2):` is the call `f(2)`; a bare `f:` calls `f` with the block alone
    fn visit_block_call(&mut self, block: &'ast ast::StmtFunctionDef) {
        let Some(decorator) = block.decorator_list.first() else {
            return;
        };
        let expression = match &decorator.expression {
            Expr::Await(await_expr) => await_expr.value.as_ref(),
            expression => expression,
        };
        if !expression.is_call_expr()
            && let Some(ty) = self.type_of(expression)
        {
            self.check_callee(expression, ty, None);
        }
        self.visit_expr(expression);
    }

    fn check_call(&mut self, expr: &'ast Expr, call: &'ast ast::ExprCall) {
        let Some(callee) = self.type_of(&call.func) else {
            return;
        };
        self.check_callee(&call.func, callee, Some((expr, call)));
    }

    /// the checks on a call: `call` is the call expression and its node when
    /// the callee is called with written arguments, `None` for a bare block
    /// callee (`Row:`)
    fn check_callee(
        &mut self,
        callee_expr: &'ast Expr,
        callee: Type<'db>,
        call: Option<(&'ast Expr, &'ast ast::ExprCall)>,
    ) {
        let db = self.db();
        match callee {
            Type::FunctionLiteral(function) => {
                let known = function.known(db);
                if let Some((expr, call)) = call {
                    match known {
                        Some(KnownFunction::BasedpythonUiState) => {
                            self.check_held_value(expr, call, "initial", HeldValue::State);
                        }
                        Some(KnownFunction::BasedpythonUiStateList) => {
                            self.check_held_value(expr, call, "initial", HeldValue::StateList);
                        }
                        Some(KnownFunction::BasedpythonUiDerived) => {
                            self.check_held_value(expr, call, "compute", HeldValue::State);
                        }
                        Some(KnownFunction::BasedpythonUiRemember) => {
                            self.check_held_value(expr, call, "compute", HeldValue::Result);
                        }
                        Some(KnownFunction::BasedpythonUiProvide) => {
                            if let Some(value) = call.arguments.find_argument_value("value", 1)
                                && let Some(value_ty) = self.type_of(value)
                            {
                                self.report_mutable_state_value(value, value_ty);
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(known) = known
                    && is_slot_function(known)
                {
                    let range = call.map_or(callee_expr.range(), |(expr, _)| expr.range());
                    self.check_slot_call(range, known);
                }
                if is_composable(db, function) {
                    self.check_composition_call(callee_expr, "a composable", function);
                } else if is_widget_builder(db, function) {
                    self.check_composition_call(callee_expr, "a builder", function);
                }
            }
            Type::ClassLiteral(class) if class.is_known(db, KnownClass::BasedpythonUiState) => {
                if let Some((expr, call)) = call {
                    self.check_held_value(expr, call, "initial", HeldValue::State);
                }
            }
            Type::ClassLiteral(class) if class.is_known(db, KnownClass::BasedpythonUiDerived) => {
                if let Some((expr, call)) = call {
                    self.check_held_value(expr, call, "compute", HeldValue::State);
                }
            }
            Type::ClassLiteral(class) if class.is_known(db, KnownClass::BasedpythonUiStateList) => {
                if let Some((expr, call)) = call {
                    self.check_held_value(expr, call, "initial", HeldValue::StateList);
                }
            }
            Type::GenericAlias(alias)
                if alias
                    .origin(db)
                    .is_known(db, KnownClass::BasedpythonUiState) =>
            {
                if let Some((expr, call)) = call {
                    self.check_held_value(expr, call, "initial", HeldValue::State);
                }
            }
            Type::GenericAlias(alias)
                if alias
                    .origin(db)
                    .is_known(db, KnownClass::BasedpythonUiDerived) =>
            {
                if let Some((expr, call)) = call {
                    self.check_held_value(expr, call, "compute", HeldValue::State);
                }
            }
            Type::GenericAlias(alias)
                if alias
                    .origin(db)
                    .is_known(db, KnownClass::BasedpythonUiStateList) =>
            {
                if let Some((expr, call)) = call {
                    self.check_held_value(expr, call, "initial", HeldValue::StateList);
                }
            }
            Type::BoundMethod(method) => {
                if let Some((_, call)) = call {
                    self.check_method_call(call, method.self_instance(db), method.function(db));
                }
            }
            _ => {}
        }
    }

    // -- `mutable-state-value` --------------------------------------------

    /// The value a construction call holds, read off the call's solved result:
    /// what `state([1])` holds is the `T` of the `State[T]` it returns.
    fn check_held_value(
        &mut self,
        expr: &'ast Expr,
        call: &'ast ast::ExprCall,
        argument: &str,
        held: HeldValue,
    ) {
        let db = self.db();
        let env = self.env();
        let Some(argument) = call.arguments.find_argument_value(argument, 0) else {
            return;
        };
        let Some(result) = self.type_of(expr) else {
            return;
        };
        let held = match held {
            HeldValue::State => state_value_type(db, env, result),
            HeldValue::StateList => state_list_element_type(db, env, result),
            HeldValue::Result => Some(result),
        };
        if let Some(held) = held {
            self.report_mutable_state_value(argument, held);
        }
    }

    fn report_mutable_state_value(&self, at: impl Ranged, held: Type<'db>) {
        let db = self.db();
        let env = self.env();
        if is_deeply_immutable(db, env, held) {
            return;
        }
        let Some(builder) = self.context.report_lint(&MUTABLE_STATE_VALUE, at) else {
            return;
        };
        builder.into_diagnostic(format_args!(
            "`{}` cannot be held in state: a change to it cannot be observed; \
             use `state_list`, a `tuple`, or {frozen}",
            held.display(db, env),
            frozen = frozen_record(db, self.context.file())
        ));
    }

    // -- calls on a receiver ----------------------------------------------

    /// A method call on an observable writes into it (`mutable-state-value`)
    /// or changes it (`state-write-in-composition`); one on a builtin
    /// container changes it where nothing can see (`silent-mutation`).
    fn check_method_call(
        &mut self,
        call: &'ast ast::ExprCall,
        receiver: Type<'db>,
        method: FunctionType<'db>,
    ) {
        let db = self.db();
        let env = self.env();
        let name = method.name(db).as_str();
        let receiver_expr = match call.func.as_ref() {
            Expr::Attribute(attribute) => Some(attribute.value.as_ref()),
            _ => None,
        };

        if let Some(kind) = observable_kind(db, env, receiver) {
            let written = match (kind, name) {
                (ObservableKind::StateList, "append") => {
                    call.arguments.find_argument_value("value", 0)
                }
                (ObservableKind::StateList, "insert") => {
                    call.arguments.find_argument_value("value", 1)
                }
                (ObservableKind::State, "set") => call.arguments.find_argument_value("new", 0),
                _ => None,
            };
            if let Some(written) = written
                && let Some(written_ty) = self.type_of(written)
            {
                self.report_mutable_state_value(written, written_ty);
            }
            if kind.is_mutator(name) {
                let written = receiver_expr.unwrap_or(&call.func);
                self.report_state_write(call.range(), self.text(written.range()));
            }
            return;
        }

        if CONTAINER_MUTATORS.contains(&name)
            && is_builtin_mutable_container(db, env, receiver)
            && !is_write_projected(db, env, receiver)
            && !receiver_expr.is_some_and(|receiver| self.is_fresh_local(receiver))
        {
            let what = format!("{}(...)", self.text(call.func.range()));
            self.report_silent_mutation(call, &what, receiver);
        }
    }

    // -- stores -------------------------------------------------------------

    /// A store to `target`: `value` is the assigned expression when the
    /// statement assigns this one target and nothing else.
    fn check_store(&mut self, target: &'ast Expr, value: Option<&'ast Expr>) {
        let db = self.db();
        let env = self.env();
        match target {
            Expr::Tuple(tuple) => {
                for element in &tuple.elts {
                    self.check_store(element, None);
                }
            }
            Expr::List(list) => {
                for element in &list.elts {
                    self.check_store(element, None);
                }
            }
            Expr::Starred(starred) => self.check_store(&starred.value, None),
            Expr::Attribute(attribute) => {
                let Some(object) = self.type_of(&attribute.value) else {
                    return;
                };
                if let Some(kind) = observable_kind(db, env, object) {
                    if kind == ObservableKind::State && attribute.attr.as_str() == "value" {
                        if let Some(value) = value
                            && let Some(value_ty) = self.type_of(value)
                        {
                            self.report_mutable_state_value(value, value_ty);
                        }
                        self.report_state_write(target.range(), self.text(attribute.value.range()));
                    }
                    return;
                }
                let what = format!("{} = ...", self.text(attribute.range()));
                self.check_attribute_store(attribute, object, &what);
            }
            Expr::Subscript(subscript) => {
                let Some(object) = self.type_of(&subscript.value) else {
                    return;
                };
                if let Some(kind) = observable_kind(db, env, object) {
                    if matches!(kind, ObservableKind::StateList | ObservableKind::StateDict) {
                        if let Some(value) = value
                            && let Some(value_ty) = self.type_of(value)
                        {
                            self.report_mutable_state_value(value, value_ty);
                        }
                        self.report_state_write(target.range(), self.text(subscript.value.range()));
                    }
                    return;
                }
                let what = format!("{}[...] = ...", self.text(subscript.value.range()));
                self.check_subscript_store(subscript, object, &what);
            }
            _ => {}
        }
    }

    fn check_augmented_store(&mut self, assign: &'ast ast::StmtAugAssign) {
        let db = self.db();
        let env = self.env();
        let op = assign.op.as_str();
        match assign.target.as_ref() {
            Expr::Attribute(attribute) => {
                let Some(object) = self.type_of(&attribute.value) else {
                    return;
                };
                if let Some(kind) = observable_kind(db, env, object) {
                    if kind == ObservableKind::State && attribute.attr.as_str() == "value" {
                        self.report_state_write(
                            assign.target.range(),
                            self.text(attribute.value.range()),
                        );
                    }
                    return;
                }
                let what = format!("{} {op}= ...", self.text(attribute.range()));
                self.check_attribute_store(attribute, object, &what);
            }
            Expr::Subscript(subscript) => {
                let Some(object) = self.type_of(&subscript.value) else {
                    return;
                };
                if let Some(kind) = observable_kind(db, env, object) {
                    if matches!(kind, ObservableKind::StateList | ObservableKind::StateDict) {
                        self.report_state_write(
                            assign.target.range(),
                            self.text(subscript.value.range()),
                        );
                    }
                    return;
                }
                let what = format!("{}[...] {op}= ...", self.text(subscript.value.range()));
                self.check_subscript_store(subscript, object, &what);
            }
            // `items += [1]` rebinds `items` to the same list, changed in place
            target @ Expr::Name(_) => {
                let Some(ty) = self.type_of(target) else {
                    return;
                };
                if !matches!(
                    assign.op,
                    ast::Operator::Add
                        | ast::Operator::Mult
                        | ast::Operator::BitOr
                        | ast::Operator::BitAnd
                        | ast::Operator::Sub
                        | ast::Operator::BitXor
                ) {
                    return;
                }
                if is_builtin_mutable_container(db, env, ty)
                    && !is_write_projected(db, env, ty)
                    && !self.is_fresh_local(target)
                {
                    let what = format!("{} {op}= ...", self.text(target.range()));
                    self.report_silent_mutation(assign, &what, ty);
                }
            }
            _ => {}
        }
    }

    fn check_delete(&mut self, target: &'ast Expr) {
        let db = self.db();
        let env = self.env();
        match target {
            Expr::Tuple(tuple) => {
                for element in &tuple.elts {
                    self.check_delete(element);
                }
            }
            Expr::List(list) => {
                for element in &list.elts {
                    self.check_delete(element);
                }
            }
            Expr::Subscript(subscript) => {
                let Some(object) = self.type_of(&subscript.value) else {
                    return;
                };
                if observable_kind(db, env, object).is_some() {
                    return;
                }
                let what = format!("del {}[...]", self.text(subscript.value.range()));
                self.check_subscript_store(subscript, object, &what);
            }
            Expr::Attribute(attribute) => {
                let Some(object) = self.type_of(&attribute.value) else {
                    return;
                };
                if observable_kind(db, env, object).is_some() {
                    return;
                }
                let what = format!("del {}", self.text(attribute.range()));
                self.check_attribute_store(attribute, object, &what);
            }
            _ => {}
        }
    }

    /// A subscript store or delete on a builtin mutable container is a
    /// `silent-mutation`, unless the container is a fresh local or a read-only
    /// view (through which the write is already rejected).
    fn check_subscript_store(
        &mut self,
        subscript: &'ast ast::ExprSubscript,
        object: Type<'db>,
        what: &str,
    ) {
        let db = self.db();
        let env = self.env();
        if is_builtin_mutable_container(db, env, object)
            && !is_write_projected(db, env, object)
            && !self.is_fresh_local(&subscript.value)
        {
            self.report_silent_mutation(subscript, what, object);
        }
    }

    /// An attribute store on an instance is a `silent-mutation` unless the
    /// class is frozen (the store is already rejected), the attribute is
    /// `Final` / read-only (likewise), or the instance is a fresh local.
    fn check_attribute_store(
        &mut self,
        attribute: &'ast ast::ExprAttribute,
        object: Type<'db>,
        what: &str,
    ) {
        let db = self.db();
        let env = self.env();
        let Some((class, _)) = object
            .nominal_class(db, env)
            .and_then(|class| class.static_class_literal(db))
        else {
            return;
        };
        if class.is_frozen_dataclass(db) == Some(true) || class.is_enum_variant(db) {
            return;
        }
        let member = object.member(db, env, attribute.attr.as_str());
        if member
            .qualifiers
            .intersects(TypeQualifiers::FINAL | TypeQualifiers::READ_ONLY)
        {
            return;
        }
        if self.is_fresh_local(&attribute.value) {
            return;
        }
        self.report_silent_mutation(attribute, what, object);
    }

    // -- reports ------------------------------------------------------------

    /// `silent-mutation`: `what` names the mutation (`items.append(...)`),
    /// `mutated` is the type changed in place
    fn report_silent_mutation(&self, at: impl Ranged, what: &str, mutated: Type<'db>) {
        let db = self.db();
        let env = self.env();
        let Some(composition) = &self.composition else {
            return;
        };
        let Some(builder) = self.context.report_lint(&SILENT_MUTATION, at) else {
            return;
        };
        let owner = composition.owner.describe(db);
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "`{what}` mutates `{}` in place, which {owner}'s composition cannot observe; \
             mutate a `StateList` or rebuild an immutable value",
            mutated.display(db, env),
        ));
        diagnostic.annotate(
            Annotation::secondary(composition.owner_span.clone())
                .message(format_args!("{owner} composes here")),
        );
    }

    /// `state-write-in-composition`: `written` names the observable written
    fn report_state_write(&self, at: TextRange, written: &str) {
        let db = self.db();
        let Some(composition) = &self.composition else {
            return;
        };
        if !composition.runs_while_composing() {
            return;
        }
        let Some(builder) = self.context.report_lint(&STATE_WRITE_IN_COMPOSITION, at) else {
            return;
        };
        builder.into_diagnostic(format_args!(
            "`{written}` is written while {} is composing; \
             move the write into an event handler or an effect",
            composition.owner.describe(db),
        ));
    }

    /// `conditional-slot`: a slot call that does not run exactly when its
    /// composition scope does
    fn check_slot_call(&self, at: TextRange, slot: KnownFunction) {
        let Some(composition) = &self.composition else {
            return;
        };
        if self.conditional_depth == 0 && composition.runs_once_per_composition() {
            return;
        }
        let Some(builder) = self.context.report_lint(&CONDITIONAL_SLOT, at) else {
            return;
        };
        builder.into_diagnostic(format_args!(
            "`{}()` under a condition: it will be created and disposed as the condition changes",
            slot.name()
        ));
    }

    /// `composable-outside-composition`: a composable or builder called where
    /// nothing is composing — outside every composition, or in a callback that
    /// runs after it
    fn check_composition_call(&self, callee: &'ast Expr, kind: &str, function: FunctionType<'db>) {
        let outside = self
            .composition
            .as_ref()
            .is_none_or(Composition::runs_after_composing);
        if !outside {
            return;
        }
        let Some(builder) = self
            .context
            .report_lint(&COMPOSABLE_OUTSIDE_COMPOSITION, callee)
        else {
            return;
        };
        builder.into_diagnostic(format_args!(
            "`{}` is {kind} and can only be called while composing",
            function.name(self.db())
        ));
    }

    // -- `unobservable-dependency` ------------------------------------------

    /// A load of a name the composition did not bind itself — a parameter of
    /// the composable, a module global, a local captured from an enclosing
    /// function — is a dependency of the composition, and must be something
    /// it can observe: a deeply immutable value cannot change, an observable
    /// notifies when it does, anything else changes without telling anyone.
    fn check_dependency(&self, expr: &'ast Expr, name: &'ast ast::ExprName) {
        let db = self.db();
        let env = self.env();
        let Some(composition) = &self.composition else {
            return;
        };
        if !composition.reads_while_composing() {
            return;
        }
        let Some(kind) = self.dependency_kind(composition, name.id.as_str()) else {
            return;
        };
        let Some(ty) = self.type_of(expr) else {
            return;
        };
        // a module is a namespace read through, not a value that changes
        if matches!(ty, Type::ModuleLiteral(_)) {
            return;
        }
        if is_deeply_immutable(db, env, ty) || observable_kind(db, env, ty).is_some() {
            return;
        }
        let Some(builder) = self.context.report_lint(&UNOBSERVABLE_DEPENDENCY, name) else {
            return;
        };
        let owner = composition.owner.describe(db);
        let what = match kind {
            DependencyKind::Parameter => format!("`{}: {}`", name.id, ty.display(db, env)),
            DependencyKind::Global | DependencyKind::Captured => {
                format!("`{}` (`{}`)", name.id, ty.display(db, env))
            }
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "{what} is read while {owner} composes, but nothing observes a change to it; {}",
            observable_alternatives(db, env, ty, kind, frozen_record(db, self.context.file())),
        ));
        diagnostic.annotate(
            Annotation::secondary(composition.owner_span.clone())
                .message(format_args!("{owner} composes here")),
        );
    }

    /// What `name`, loaded in this scope, is to `composition`: a parameter of
    /// its composable, a module global, or a local captured from a function
    /// enclosing it. `None` when the composition binds the name itself — a
    /// local of the body, of a block written in it, of a comprehension, a
    /// block's own parameter: this run's own value, whatever its origin — or
    /// when nothing in the file binds it: a builtin, or a member the block's
    /// receiver supplies.
    fn dependency_kind(
        &self,
        composition: &Composition<'db>,
        name: &str,
    ) -> Option<DependencyKind> {
        for (id, _) in self.index.visible_ancestor_scopes(self.scope) {
            let table = self.index.place_table(id);
            let Some(symbol_id) = table.symbol_id(name) else {
                continue;
            };
            let symbol = table.symbol(symbol_id);
            if symbol.is_global() {
                return Some(DependencyKind::Global);
            }
            if symbol.is_nonlocal() || !symbol.is_bound() {
                continue;
            }
            if id.is_global() {
                return Some(DependencyKind::Global);
            }
            if !self.is_composition_scope(composition, id) {
                return Some(DependencyKind::Captured);
            }
            // bound inside the composition: the composable's own parameter
            // is what its caller passed; anything else is this run's value
            let is_parameter = id == composition.owner_scope
                && matches!(composition.owner, CompositionOwner::Composable(_))
                && matches!(
                    self.index.scope(id).node(),
                    NodeWithScopeKind::Function(function)
                        if function.node(self.module).parameters.includes(name)
                );
            return is_parameter.then_some(DependencyKind::Parameter);
        }
        None
    }

    /// whether `scope` runs as part of `composition`: it is this scope, or
    /// one of those between it and the composition's owner
    fn is_composition_scope(&self, composition: &Composition<'db>, scope: FileScopeId) -> bool {
        for (id, _) in self.index.ancestor_scopes(self.scope) {
            if id == scope {
                return true;
            }
            if id == composition.owner_scope {
                return false;
            }
        }
        false
    }

    // -- fresh locals -------------------------------------------------------

    /// Whether `receiver` is rooted in a name that the composition itself
    /// binds to a fresh value — a display, a comprehension, a constructor
    /// call — in this scope or one of the scopes between it and the
    /// composition's owner. Mutating such a value is mutating something no
    /// one else holds.
    fn is_fresh_local(&self, receiver: &Expr) -> bool {
        let Some(name) = root_name(receiver) else {
            return false;
        };
        let Some(composition) = &self.composition else {
            return false;
        };
        for (id, scope) in self.index.ancestor_scopes(self.scope) {
            let body: &[Stmt] = match scope.node() {
                NodeWithScopeKind::Function(function) => {
                    let function = function.node(self.module);
                    if function.parameters.includes(name) {
                        return false;
                    }
                    &function.body
                }
                NodeWithScopeKind::Lambda(lambda) => {
                    let lambda = lambda.node(self.module);
                    if lambda
                        .parameters
                        .as_deref()
                        .is_some_and(|parameters| parameters.includes(name))
                    {
                        return false;
                    }
                    &[]
                }
                NodeWithScopeKind::Module | NodeWithScopeKind::Class(_) => return false,
                _ => &[],
            };
            let mut scan = BindingScan {
                name,
                found: false,
                fresh: true,
                type_of: |expr: &Expr| self.type_in_scope(id, expr),
            };
            scan.visit_body(body);
            if scan.found {
                return scan.fresh;
            }
            if id == composition.owner_scope {
                return false;
            }
        }
        false
    }

    /// the type of `expr` in `scope`: this scope's in-progress inference, or
    /// an enclosing scope's own.
    ///
    /// Asking an *enclosing* scope for its types from inside this one is only
    /// safe while the enclosing scope's inference does not, in turn, wait on
    /// this scope's — which would close a cycle through `infer_scope_types`.
    /// Two things keep it open. A trailing-lambda block's callback is required
    /// to return `None` (`trailing-lambda-return-type`), so the enclosing scope
    /// never needs a block body's result; and a block reads its own callee from
    /// the standalone inference the semantic index registers for it, never from
    /// the enclosing scope. A lambda's return type *is* needed by the scope
    /// that writes it, but a lambda body binds no names of its own that this
    /// walk would ask about — it stops at the first parameter or binding it
    /// finds. If a block ever gains a real return type, this must move to the
    /// standalone-expression route that [`enclosing_call_argument`] uses
    fn type_in_scope(&self, scope: FileScopeId, expr: &Expr) -> Option<Type<'db>> {
        if scope == self.scope {
            return self.type_of(expr);
        }
        let db = self.db();
        let scope = scope.to_scope_id(db, self.context.program_file());
        infer_scope_types(db, scope, TypeContext::default()).try_expression_type(expr)
    }
}

/// what a construction call holds, read off its result type
#[derive(Clone, Copy)]
enum HeldValue {
    /// the `T` of the `State[T]` / `Derived[T]` returned
    State,
    /// the `T` of the `StateList[T]` returned
    StateList,
    /// the result itself (`remember`)
    Result,
}

/// the name an attribute / subscript chain is rooted in: `items` for
/// `items[0].children`
fn root_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Attribute(attribute) => root_name(&attribute.value),
        Expr::Subscript(subscript) => root_name(&subscript.value),
        _ => None,
    }
}

/// Scans a scope's own statements for the bindings of one name, deciding
/// whether every one of them binds a fresh value. Nested scopes bind their own
/// names and are not entered.
struct BindingScan<'a, F> {
    name: &'a str,
    /// whether the scope binds the name at all
    found: bool,
    /// whether every binding found so far is fresh
    fresh: bool,
    type_of: F,
}

impl<'db, 'ast, F> BindingScan<'_, F>
where
    F: Fn(&Expr) -> Option<Type<'db>>,
{
    fn bind(&mut self, target: &'ast Expr, value: Option<&'ast Expr>) {
        match target {
            Expr::Name(name) if name.id.as_str() == self.name => {
                self.found = true;
                self.fresh &= value.is_some_and(|value| self.is_fresh_value(value));
            }
            Expr::Tuple(tuple) => {
                for element in &tuple.elts {
                    self.bind(element, None);
                }
            }
            Expr::List(list) => {
                for element in &list.elts {
                    self.bind(element, None);
                }
            }
            Expr::Starred(starred) => self.bind(&starred.value, None),
            _ => {}
        }
    }

    fn bind_name(&mut self, name: &str) {
        if name == self.name {
            self.found = true;
            self.fresh = false;
        }
    }

    /// a value nothing else can hold: a display, a comprehension, or the
    /// instance a constructor call just made
    fn is_fresh_value(&self, value: &Expr) -> bool {
        match value {
            Expr::List(_)
            | Expr::Dict(_)
            | Expr::Set(_)
            | Expr::ListComp(_)
            | Expr::DictComp(_)
            | Expr::SetComp(_) => true,
            Expr::Call(call) => matches!(
                (self.type_of)(&call.func),
                Some(Type::ClassLiteral(_) | Type::GenericAlias(_))
            ),
            Expr::Named(named) => self.is_fresh_value(&named.value),
            _ => false,
        }
    }
}

impl<'db, 'ast, F> Visitor<'ast> for BindingScan<'_, F>
where
    F: Fn(&Expr) -> Option<Type<'db>>,
{
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::FunctionDef(function) => {
                if !function.is_trailing_lambda {
                    self.bind_name(function.name.as_str());
                }
            }
            Stmt::ClassDef(class) => self.bind_name(class.name.as_str()),
            Stmt::Assign(assign) => {
                let value = match assign.targets.as_slice() {
                    [_] => Some(&*assign.value),
                    _ => None,
                };
                for target in &assign.targets {
                    self.bind(target, value);
                }
            }
            Stmt::AnnAssign(assign) => {
                if let Some(value) = assign.value.as_deref() {
                    self.bind(&assign.target, Some(value));
                }
            }
            Stmt::For(for_stmt) => {
                self.bind(&for_stmt.target, None);
                walk_stmt(self, stmt);
            }
            Stmt::With(with) => {
                for item in &with.items {
                    if let Some(target) = item.optional_vars.as_deref() {
                        self.bind(target, None);
                    }
                }
                walk_stmt(self, stmt);
            }
            Stmt::Try(try_stmt) => {
                for handler in &try_stmt.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    if let Some(name) = &handler.name {
                        self.bind_name(name.as_str());
                    }
                }
                walk_stmt(self, stmt);
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    let bound = alias.asname.as_ref().map_or_else(
                        || alias.name.split('.').next().unwrap_or(""),
                        |name| name.as_str(),
                    );
                    self.bind_name(bound);
                }
            }
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    let bound = alias.asname.as_ref().unwrap_or(&alias.name);
                    self.bind_name(bound.as_str());
                }
            }
            Stmt::Global(global) => {
                for name in &global.names {
                    self.bind_name(name.as_str());
                }
            }
            Stmt::Nonlocal(nonlocal) => {
                for name in &nonlocal.names {
                    self.bind_name(name.as_str());
                }
            }
            _ => walk_stmt(self, stmt),
        }
    }

    // a walrus inside an expression is not looked for: a name it binds is
    // taken as not bound here, which errs towards reporting
    fn visit_expr(&mut self, _expr: &'ast Expr) {}
}
