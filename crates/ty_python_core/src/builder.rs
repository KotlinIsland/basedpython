use std::cell::{OnceCell, RefCell};
use std::sync::Arc;

use except_handlers::{ExceptionContextStackManager, ExceptionHandlers};
use itertools::Itertools;
use ruff_python_ast::helpers::{
    BindingKeyword, Truthiness, any_over_expr, binding_keyword, is_dotted_name,
    last_bound_parameter, parameter_modifiers, return_guards, statement_expression_values,
};
use rustc_hash::{FxHashMap, FxHashSet};

use ruff_db::parsed::ParsedModuleRef;

use ruff_db::source::{SourceText, source_text};
use ruff_index::IndexVec;
use ruff_python_ast::name::{Name, UnqualifiedName};
use ruff_python_ast::visitor::{
    Visitor, walk_body, walk_expr, walk_keyword, walk_pattern, walk_stmt,
};
use ruff_python_ast::{self as ast, AtomicNodeIndex, NodeIndex, PySourceType, PythonVersion};
use ruff_python_parser::semantic_errors::{
    LazyImportContext, SemanticSyntaxChecker, SemanticSyntaxContext, SemanticSyntaxError,
    SemanticSyntaxErrorKind, YieldOutsideFunctionKind,
};
use ruff_text_size::{Ranged, TextRange};
use smallvec::SmallVec;
use ty_module_resolver::{
    ImportingFile, ModuleName, ResolverEnvironment, resolve_module_for_import_from,
};

use crate::BlockScopedDeclaration;
use crate::HasTrackedScope;
use crate::ProgramFile;
use crate::ast_ids::node_key::ExpressionNodeKey;
use crate::ast_ids::{AstIdsBuilder, ScopedUseId};
use crate::ast_node_ref::AstNodeRef;
use crate::definition::{
    AnnotatedAssignmentDefinitionNodeRef, AssignmentDefinitionNodeRef, BindingsOwner,
    ComprehensionDefinitionNodeRef, Definition, DefinitionCategory, DefinitionKind,
    DefinitionNodeKey, DefinitionNodeRef, Definitions, DictKeyAssignmentNodeRef,
    ExceptHandlerDefinitionNodeRef, ForStmtDefinitionNodeRef, ImportDefinitionNodeRef,
    ImportFromDefinitionNodeRef, ImportFromSubmoduleDefinitionNodeRef,
    LambdaParameterDefinitionNodeRef, LoopHeaderDefinitionNodeRef, LoopStmtRef,
    MatchPatternDefinitionNodeRef, NestedBindingExecution, NestedBindingsDefinitionKind,
    ParameterDefinitionNodeRef, StarImportDefinitionNodeRef, TypeMatchCaptureDefinitionNodeRef,
    WithItemDefinitionNodeRef,
};
use crate::expression::{Expression, ExpressionKind};
use crate::fluid::{FluidUse, FluidUseRole};
use crate::frozen::{FrozenMap, FrozenSet};
use crate::member::MemberExprBuilder;
use crate::node_key::NodeKey;
use crate::place::{
    PlaceExpr, PlaceTableBuilder, PossiblyNarrowedPlacesBuilder, ScopedPlaceId,
    match_subject_place_expressions,
};
use crate::predicate::{
    CallableAndCallExpr, CaseNameCapturePredicate, CaseNamePredicateKind,
    ClassPatternKeywordPredicateKind, ClassPatternPredicateKind, MappingPatternEntryPredicateKind,
    MappingPatternPredicateKind, PatternPredicate, PatternPredicateKind, PatternSubject, Predicate,
    PredicateNode, PredicateOrLiteral, ScopedPredicateId, SequencePatternPredicateKind,
    StarImportPlaceholderPredicate, SubjectElementPatternPredicate,
};
use crate::re_exports::exported_names;
use crate::reachability_constraints::{
    ReachabilityConstraintsBuilder, ScopedReachabilityConstraintId,
};
use crate::scope::{
    FileScopeId, NodeWithScopeKey, NodeWithScopeKind, NodeWithScopeRef, Scope, ScopeId, ScopeKind,
    ScopeLaziness,
};
use crate::statement::StatementInner;
use crate::symbol::{ScopedSymbolId, Symbol};
use crate::unpack::{Unpack, UnpackKind, UnpackPosition, UnpackValue};
use crate::use_def::{
    EnclosingSnapshotKey, FlowSnapshot, FutureDefinitions, LiveBinding, LiveBindingStatus,
    PreviousDefinitions, ScopedDefinitionId, ScopedEnclosingSnapshotId, UseDefMapBuilder,
    UseDefMapInterner,
};
use crate::{Db, Statement, StatementNodeKey};
use crate::{
    DefinitionsByNode, Destructure, EvaluationMode, ExpressionsScopeMap, LoopHeader, LoopHeaderId,
    NarrowingAliasPredicate, PossiblyNarrowedPlaces, SemanticIndex, VisibleAncestorsIter,
};

use super::place::PlaceExprRef;

mod except_handlers;
mod loop_bindings_visitor;

#[derive(Clone, Debug, Default)]
struct Loop {
    /// Flow states at each `break` in the current loop.
    break_states: Vec<FlowSnapshot>,
    /// Flow states at each `continue` in the current loop.
    continue_states: Vec<FlowSnapshot>,
    /// basedpython: how many blocks of the current scope were open when the loop
    /// started. A `break` or a `continue` leaves every block opened since, so the
    /// names those blocks declared go out of scope on that edge.
    blocks_at_entry: usize,
}

/// A narrowing alias: a variable whose RHS is a narrowing expression
/// (e.g., `is_none = x is None`).
#[derive(Clone, Debug)]
struct NarrowingAlias<'ast> {
    /// The RHS expression (e.g., `x is None`).
    expression: &'ast ast::Expr,
    /// The scope whose place table should be used to resolve the aliased expression.
    expression_scope: FileScopeId,
    /// Places that, if reassigned, should invalidate this alias.
    narrowed_places: PossiblyNarrowedPlaces,
}

struct ScopeInfo<'ast> {
    file_scope_id: FileScopeId,
    /// Current loop state; None if we are not currently visiting a loop
    current_loop: Option<Loop>,
    /// Saved narrowing aliases from the enclosing scope, restored on `pop_scope`.
    narrowing_aliases: FxHashMap<Name, NarrowingAlias<'ast>>,
    /// basedpython: saved open blocks from the enclosing scope, restored on
    /// `pop_scope`. A scope's own body is not a block, so a declaration written at
    /// the top of a nested function belongs to no block at all — least of all to
    /// whichever block the `def` happened to sit in.
    open_blocks: Vec<OpenBlock>,
    /// `global` and `nonlocal` declarations from scopes nested under this one. This is used for:
    ///
    /// 1. Visibility of nested writes. A nested function that binds a variable might affect the
    ///    inferred type of that variable in an outer scope. After each nested scope is closed, we
    ///    install synthetic definitions to refer to visible nested writes, which we read from this
    ///    map. (Note that *which kind* of nested writes is visible isn't necessarily known at that
    ///    point; we stash both kinds and decide which one to use at inference time.)
    /// 2. Semantic syntax errors for invalid `nonlocal` declarations. A `nonlocal` declaration is
    ///    required to resolve to a local variable, and that variable must not be declared `global`
    ///    or defined in the global scope. (This is why we track all `nonlocal` declarations, even
    ///    if there's no binding in their scope.)
    ///
    /// When we're trying to figure out what scope a `nonlocal` declaration resolves to, we have to
    /// remember that the definition of a local variable can come after a nested function that
    /// mentions it. Similarly, when we're trying to figure out whether `global` bindings from a
    /// nested scopes are visible in the current scope, we have to remember that a `global`
    /// declaration can also come after nested function definitions. We build up these maps as we
    /// encounter each `global` and `nonlocal` keyword, but we generally need to wait until scopes
    /// are popped (or later, at inference time) to analyze them.
    nested_global_or_nonlocal_declarations: NestedGlobalOrNonlocalDeclarations,
    /// Text ranges for `global` and `nonlocal` declarations in this scope, which we use to
    /// populate `nested_global_or_nonlocal_declarations` when we reach end of scope.
    this_scope_global_or_nonlocal_declarations: FxHashMap<Name, TextRange>,
    /// Free symbol uses from nested scopes that may resolve to this scope.
    pending_captures: PendingCaptures,
}

type NestedGlobalOrNonlocalDeclarations = FxHashMap<Name, SmallVec<[NestedDeclaration; 1]>>;

/// Captures cannot be resolved until the enclosing scope is complete because a later binding can
/// make the name local. For example, `inner` captures `outer`'s local `value`, even though the
/// binding appears after `inner` is defined:
///
/// ```python
/// def outer():
///     def inner():
///         return value
///
///     value = 1
/// ```
///
/// Each pending capture remembers the bindings visible when the nested scope was created and, for
/// lazy scopes, any bindings added afterward.
type PendingCaptures = FxHashMap<Name, SmallVec<[PendingCapture; 1]>>;

#[derive(Debug)]
struct PendingCapture {
    nested_scope: FileScopeId,
    laziness: ScopeLaziness,
    binding_definition_ids: SmallVec<[ScopedDefinitionId; 2]>,
}

#[derive(Debug)]
struct UnresolvedCapture {
    /// The scope containing the free symbol use. Retaining this lets final resolution apply class
    /// scope visibility rules correctly as the capture is propagated outward.
    nested_scope: FileScopeId,
    name: Name,
    laziness: ScopeLaziness,
}

impl UnresolvedCapture {
    fn through_scope(mut self, scope_laziness: ScopeLaziness) -> Self {
        if scope_laziness.is_lazy() {
            self.laziness = ScopeLaziness::Lazy;
        }
        self
    }
}

#[derive(Copy, Clone, Debug, get_size2::GetSize)]
pub struct NestedDeclaration {
    pub kind: GlobalOrNonlocal,
    pub file_scope_id: FileScopeId,
    pub range: TextRange,
    pub is_bound: bool,
}

impl NestedDeclaration {
    pub fn is_global(&self) -> bool {
        matches!(self.kind, GlobalOrNonlocal::Global)
    }
}

#[derive(Copy, Clone, Debug, get_size2::GetSize)]
pub enum GlobalOrNonlocal {
    Global,
    Nonlocal,
}

struct ConditionFlowSnapshots {
    truthy: FlowSnapshot,
    falsy: FlowSnapshot,
}

impl ConditionFlowSnapshots {
    fn into_short_circuit_and_continuation(self, op: ast::BoolOp) -> (FlowSnapshot, FlowSnapshot) {
        match op {
            ast::BoolOp::And => (self.falsy, self.truthy),
            ast::BoolOp::Or => (self.truthy, self.falsy),
        }
    }
}

enum ConditionFlowSnapshot {
    Fallback,
    Branches(ConditionFlowSnapshots),
}

impl ConditionFlowSnapshot {
    fn into_truthy(self) -> Option<FlowSnapshot> {
        match self {
            Self::Fallback => None,
            Self::Branches(snapshots) => Some(snapshots.truthy),
        }
    }

    fn into_branches(self) -> Option<ConditionFlowSnapshots> {
        match self {
            Self::Fallback => None,
            Self::Branches(snapshots) => Some(snapshots),
        }
    }
}

/// Whether evaluation produces a result object or chooses a control-flow path.
///
/// In `Value` context, the enclosing code receives the expression's result object. For example,
/// `result = x and y` produces `x` if `x` is falsy, or `y` otherwise. This also applies to expressions
/// that return `bool`: the comparison in `result = x > 0` has value context.
///
/// In `Condition` context, the enclosing code only needs to know which branch to take. For example,
/// CPython evaluates `if x and y` by testing `x` and, only if `x` is truthy, testing `y`. If `x` tests
/// falsy, that one truthiness check is enough to skip the body: `x` is not tested again as the
/// result of `x and y`.
///
/// This distinction matters when an operand's `__bool__` can change between calls:
///
/// ```python
/// if x and False:      # A falsy x skips the body; a truthy x reaches False.
///     ...              # Unreachable in either case.
/// saved = x and False  # Can produce x after checking that it is falsy.
/// if saved:            # Can call x.__bool__ again, which may now return True.
///     ...              # Reachable.
/// ```
///
/// The context propagates through `and`, `or`, `not`, and the branches of conditional expressions.
/// Condition context does not propagate through calls or assignment expressions: in
/// `if f(x and False)`, the call's result controls the branch, but its argument is evaluated in
/// value context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpressionContext {
    /// Produce the expression's result object for the enclosing code to use.
    Value,
    /// Choose the truthy or falsy control-flow path without preserving the result object.
    Condition,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent walk flags and one cached setting, not a state machine"
)]
pub(super) struct SemanticIndexBuilder<'db, 'ast> {
    // Builder state
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    source_type: PySourceType,
    module: &'ast ParsedModuleRef,
    scope_stack: Vec<ScopeInfo<'ast>>,
    /// The assignments we're currently visiting, with
    /// the most recent visit at the end of the Vec.
    current_assignments: Vec<CurrentAssignment<'ast, 'db>>,
    /// The statements we're currently visiting, with
    /// the most recent visit at the end of the Vec.
    current_statements: Vec<CurrentStatement<'ast, 'db>>,
    /// The match case we're currently visiting.
    current_match_case: Option<CurrentMatchCase<'ast, 'db>>,
    /// The statement expressions we're currently visiting, innermost last.
    current_statement_expressions: Vec<CurrentStatementExpression>,
    /// The name of the first function parameter of the innermost function that we're currently visiting.
    current_first_parameter_name: Option<&'ast str>,

    /// Per-scope exception contexts for nested `try` and `with` statements.
    exception_context_stack_manager: ExceptionContextStackManager,

    /// Flags about the file's global scope
    has_future_annotations: bool,
    /// Whether we are currently visiting an `if TYPE_CHECKING` block.
    in_type_checking_block: bool,

    // Used for checking semantic syntax errors
    resolver_environment: ResolverEnvironment<'db>,
    python_version: PythonVersion,
    source_text: OnceCell<SourceText>,
    semantic_checker: SemanticSyntaxChecker,
    /// Whether the current statement is inside a `try` statement, including its `except`, `else`,
    /// and `finally` suites. Used for semantic syntax checks independently of handler activity.
    in_try_statement: bool,

    // Semantic Index fields
    scopes: IndexVec<FileScopeId, Scope>,
    scope_ids_by_scope: IndexVec<FileScopeId, ScopeId<'db>>,
    place_tables: IndexVec<FileScopeId, PlaceTableBuilder>,
    ast_ids: IndexVec<FileScopeId, AstIdsBuilder>,
    // Box to avoid copying large builders when this index grows.
    use_def_maps: IndexVec<FileScopeId, Box<UseDefMapBuilder<'db>>>,
    scopes_by_node: FxHashMap<NodeWithScopeKey, FileScopeId>,
    scopes_by_expression: ExpressionsScopeMapBuilder,
    definitions_by_node: FxHashMap<DefinitionNodeKey, Definitions<'db>>,
    expressions_by_node: FxHashMap<ExpressionNodeKey, Expression<'db>>,
    unpacks_by_target: FxHashMap<ExpressionNodeKey, Unpack<'db>>,
    condition_flow_snapshots_by_node: FxHashMap<ExpressionNodeKey, ConditionFlowSnapshots>,
    statements_by_node: FxHashMap<StatementNodeKey, Statement<'db>>,
    imported_modules: FxHashSet<ModuleName>,
    seen_submodule_imports: FxHashSet<String>,
    // A map from a lambda expression to its enclosing statement.
    enclosing_lambda_statements: FxHashMap<ExpressionNodeKey, Statement<'db>>,
    // A map from a use of a fluid specialization candidate to its definition.
    fluid_candidates_by_use: FxHashMap<ExpressionNodeKey, Definition<'db>>,
    // A map from a fluid specialization candidate definition to its classified uses.
    fluid_uses_by_candidate: FxHashMap<Definition<'db>, Vec<FluidUse<'db>>>,
    /// Ranges of the loop statements enclosing the current traversal position, outermost first.
    loop_ranges: Vec<TextRange>,
    /// Hashset of all [`FileScopeId`]s that correspond to [generator functions].
    ///
    /// [generator functions]: https://docs.python.org/3/glossary.html#term-generator
    generator_functions: FxHashSet<FileScopeId>,
    /// basedpython: the call expressions this file makes as bare statements.
    basedpython_statement_calls: FxHashSet<ExpressionNodeKey>,
    /// Hashset of all [`FileScopeId`]s that correspond to asynchronous comprehensions.
    async_comprehensions: FxHashSet<FileScopeId>,
    /// Snapshots of enclosing-scope place states visible from nested scopes.
    enclosing_snapshots: FxHashMap<EnclosingSnapshotKey, ScopedEnclosingSnapshotId>,
    /// Errors collected by the `semantic_checker`.
    semantic_syntax_errors: RefCell<Vec<SemanticSyntaxError>>,

    /// Maps alias variable names to their narrowing expressions (same-scope only).
    /// TODO: cross-scope alias narrowing support
    narrowing_aliases: FxHashMap<Name, NarrowingAlias<'ast>>,

    /// Alias metadata for predicate leaf names in the current file.
    alias_predicates: FxHashMap<ExpressionNodeKey, NarrowingAliasPredicate<'db>>,

    /// basedpython: what each destructuring binder's pattern needs at inference
    /// time, keyed by the pattern node
    destructures: FxHashMap<NodeKey, Destructure<'db>>,

    /// basedpython: every case-pattern name context-sensitive resolution is
    /// offered — a bare `case A:` name, keyed by its identifier, and a bare class
    /// pattern's `case Circle(r):` name, keyed by its expression.
    case_names: FxHashMap<NodeKey, CaseNamePredicateKind<'db>>,

    /// basedpython: places this file's narrowing return annotations name, computed on
    /// demand. See [`Self::basedpython_guard_targets`].
    basedpython_guard_targets: Option<GuardTargets>,

    /// basedpython: whether a `let` / `var` declaration inside a block is visible
    /// only within that block. See [`Db::block_scoped_declarations`].
    block_scoped_declarations: bool,

    /// basedpython: the blocks of the current scope still being visited, outermost
    /// first, each holding the `let` / `var` declarations written directly inside
    /// it. The innermost is emptied into the scope's table when that block ends,
    /// which is where the names it bound go out of scope.
    open_blocks: Vec<OpenBlock>,

    /// basedpython: every block-scoped declaration of each scope, in the order the
    /// blocks ended. [`Self::build`] sorts each list into source order, which is
    /// what reads them.
    block_declarations_by_scope: FxHashMap<FileScopeId, Vec<BlockScopedDeclaration>>,
}

/// basedpython: the symbols a block took out of scope. Almost always empty, and
/// never large.
type BlockDeclarations = SmallVec<[ScopedSymbolId; 4]>;

/// basedpython: a block being visited, and the declarations that go out of scope
/// when it ends.
#[derive(Debug, Default)]
struct OpenBlock {
    /// The `let` / `var` declarations written directly in this block.
    declared: Vec<PendingBlockDeclaration>,
    /// The symbols that have already gone out of scope inside this block, because a
    /// block nested in it ended.
    ///
    /// An edge that leaves this block from within — an exception, a `break` — was
    /// taken at a point where those were still bound, so they have to go out of
    /// scope on that edge too.
    closed: Vec<ScopedSymbolId>,
}

impl OpenBlock {
    /// Every symbol that is out of scope once this block has been left, in any way.
    fn out_of_scope(&self) -> impl Iterator<Item = ScopedSymbolId> + '_ {
        self.declared
            .iter()
            .map(|declaration| declaration.symbol)
            .chain(self.closed.iter().copied())
    }
}

/// basedpython: a `let` / `var` declaration inside a block that has not ended yet,
/// so the block it is scoped to is not yet known.
#[derive(Debug)]
struct PendingBlockDeclaration {
    /// The symbol the declaration binds.
    symbol: ScopedSymbolId,
    /// Which keyword made the binding block-scoped.
    keyword: BindingKeyword,
    /// Where that keyword is written.
    keyword_range: TextRange,
}

impl<'db, 'ast> SemanticIndexBuilder<'db, 'ast> {
    pub(super) fn new(
        db: &'db dyn Db,
        file: ProgramFile<'db>,
        module_ref: &'ast ParsedModuleRef,
    ) -> Self {
        let mut builder = Self {
            db,
            file,
            source_type: file.file(db).source_type(db),
            module: module_ref,
            scope_stack: Vec::new(),
            current_assignments: Vec::new(),
            current_statements: Vec::new(),
            current_match_case: None,
            current_statement_expressions: Vec::new(),
            current_first_parameter_name: None,
            exception_context_stack_manager: ExceptionContextStackManager::default(),

            has_future_annotations: false,
            in_type_checking_block: false,

            scopes: IndexVec::new(),
            place_tables: IndexVec::new(),
            ast_ids: IndexVec::new(),
            scope_ids_by_scope: IndexVec::new(),
            use_def_maps: IndexVec::new(),

            scopes_by_expression: ExpressionsScopeMapBuilder::new(),
            scopes_by_node: FxHashMap::default(),
            definitions_by_node: FxHashMap::default(),
            expressions_by_node: FxHashMap::default(),
            unpacks_by_target: FxHashMap::default(),
            condition_flow_snapshots_by_node: FxHashMap::default(),
            statements_by_node: FxHashMap::default(),
            enclosing_lambda_statements: FxHashMap::default(),
            fluid_candidates_by_use: FxHashMap::default(),
            fluid_uses_by_candidate: FxHashMap::default(),
            loop_ranges: Vec::new(),

            seen_submodule_imports: FxHashSet::default(),
            imported_modules: FxHashSet::default(),
            generator_functions: FxHashSet::default(),
            basedpython_statement_calls: FxHashSet::default(),
            async_comprehensions: FxHashSet::default(),

            enclosing_snapshots: FxHashMap::default(),

            resolver_environment: file.resolver_environment(db),
            python_version: file.python_version(db),
            source_text: OnceCell::new(),
            semantic_checker: SemanticSyntaxChecker::default(),
            in_try_statement: false,
            semantic_syntax_errors: RefCell::default(),
            narrowing_aliases: FxHashMap::default(),
            alias_predicates: FxHashMap::default(),
            destructures: FxHashMap::default(),
            case_names: FxHashMap::default(),
            basedpython_guard_targets: None,

            block_scoped_declarations: db.block_scoped_declarations(file.file(db)),
            open_blocks: Vec::new(),
            block_declarations_by_scope: FxHashMap::default(),
        };

        builder.push_scope_with_parent(NodeWithScopeRef::Module, None);
        builder
    }

    fn current_scope_info(&self) -> &ScopeInfo<'ast> {
        self.scope_stack
            .last()
            .expect("SemanticIndexBuilder should have created a root scope")
    }

    fn current_scope_info_mut(&mut self) -> &mut ScopeInfo<'ast> {
        self.scope_stack
            .last_mut()
            .expect("SemanticIndexBuilder should have created a root scope")
    }

    fn current_scope(&self) -> FileScopeId {
        self.current_scope_info().file_scope_id
    }

    fn current_scope_id(&self) -> ScopeId<'db> {
        self.scope_ids_by_scope[self.current_scope()]
    }

    fn mark_current_comprehension_async(&mut self) {
        let scope = self.current_scope();
        if self.scopes[scope].kind() == ScopeKind::Comprehension {
            self.async_comprehensions.insert(scope);
        }
    }

    fn expect_single_definition(
        &self,
        definition_key: impl Into<DefinitionNodeKey> + std::fmt::Debug + Copy,
    ) -> Definition<'db> {
        let definitions = &self.definitions_by_node[&definition_key.into()];
        debug_assert_eq!(
            definitions.len(),
            1,
            "Expected exactly one definition to be associated with AST node {definition_key:?} but found {}",
            definitions.len()
        );
        definitions[0]
    }

    /// Returns an iterator over ancestors of `scope` that are visible for name resolution,
    /// starting with `scope` itself. This follows Python's lexical scoping rules where
    /// class scopes are skipped during name resolution (except for the starting scope
    /// if it happens to be a class scope).
    ///
    /// For example, in this code:
    /// ```python
    /// x = 1
    /// class A:
    ///     x = 2
    ///     def method(self):
    ///         print(x)  # Refers to global x=1, not class x=2
    /// ```
    /// The `method` function can see the global scope but not the class scope.
    fn visible_ancestor_scopes(&self, scope: FileScopeId) -> VisibleAncestorsIter<'_> {
        VisibleAncestorsIter::new(&self.scopes, scope)
    }

    /// Returns the scope ID of the current scope if the current scope
    /// is a method inside a class body or an eagerly executed scope inside a method.
    /// Returns `None` otherwise, e.g. if the current scope is a function body outside of a class, or if the current scope is not a
    /// function body.
    fn is_method_or_eagerly_executed_in_method(&self) -> Option<FileScopeId> {
        let mut scopes_rev = self
            .scope_stack
            .iter()
            .rev()
            .skip_while(|scope| self.scopes[scope.file_scope_id].is_eager());
        let current = scopes_rev.next()?;

        if self.scopes[current.file_scope_id].kind() != ScopeKind::Function {
            return None;
        }

        let maybe_method = current.file_scope_id;
        let parent = scopes_rev.next()?;

        match self.scopes[parent.file_scope_id].kind() {
            ScopeKind::Class => Some(maybe_method),
            ScopeKind::TypeParams => {
                // If the function is generic, the parent scope is an annotation scope.
                // In this case, we need to go up one level higher to find the class scope.
                let grandparent = scopes_rev.next()?;

                if self.scopes[grandparent.file_scope_id].kind() == ScopeKind::Class {
                    Some(maybe_method)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Checks if a symbol name is bound in any intermediate eager scopes
    /// between the current scope and the specified method scope.
    ///
    fn is_symbol_bound_in_intermediate_eager_scopes(
        &self,
        symbol_name: &str,
        method_scope_id: FileScopeId,
    ) -> bool {
        for scope_info in self.scope_stack.iter().rev() {
            let scope_id = scope_info.file_scope_id;

            if scope_id == method_scope_id {
                break;
            }

            if let Some(symbol_id) = self.place_tables[scope_id].symbol_id(symbol_name) {
                let symbol = self.place_tables[scope_id].symbol(symbol_id);
                if symbol.is_bound() {
                    return true;
                }
            }
        }

        false
    }

    /// Push a new loop, returning the outer loop, if any.
    fn push_loop(&mut self) -> Option<Loop> {
        let blocks_at_entry = self.open_blocks.len();
        self.current_scope_info_mut().current_loop.replace(Loop {
            blocks_at_entry,
            ..Loop::default()
        })
    }

    /// Pop a loop, replacing with the previous saved outer loop, if any.
    fn pop_loop(&mut self, outer_loop: Option<Loop>) -> Loop {
        std::mem::replace(&mut self.current_scope_info_mut().current_loop, outer_loop)
            .expect("pop_loop() should not be called without a prior push_loop()")
    }

    fn current_loop_mut(&mut self) -> Option<&mut Loop> {
        self.current_scope_info_mut().current_loop.as_mut()
    }

    fn push_scope(&mut self, node: NodeWithScopeRef) {
        self.push_scope_with_parent(node, Some(self.current_scope()));
    }

    fn push_scope_with_parent(&mut self, node: NodeWithScopeRef, parent: Option<FileScopeId>) {
        let children_start = self.scopes.next_index() + 1;

        // Note `node` is guaranteed to be a child of `self.module`
        let node_with_kind = node.to_kind(self.module);

        let scope = Scope::new(parent, node_with_kind, children_start..children_start);
        let scope_kind = scope.kind();
        self.exception_context_stack_manager.enter_nested_scope();

        let file_scope_id = self.scopes.push(scope);
        self.place_tables.push(PlaceTableBuilder::default());
        self.use_def_maps
            .push(Box::new(UseDefMapBuilder::new(scope_kind)));
        let ast_id_scope = self.ast_ids.push(AstIdsBuilder::default());

        let scope_id = ScopeId::new(self.db, self.file, file_scope_id);

        self.scope_ids_by_scope.push(scope_id);
        let previous = self.scopes_by_node.insert(node.node_key(), file_scope_id);
        debug_assert_eq!(previous, None);

        debug_assert_eq!(ast_id_scope, file_scope_id);

        // Save narrowing aliases. They will be restored with `pop_scope` after returning from inspecting the inner scope.
        // TODO: Cross-scope alias narrowing is not supported yet.
        let saved_aliases = std::mem::take(&mut self.narrowing_aliases);
        let saved_open_blocks = std::mem::take(&mut self.open_blocks);
        self.scope_stack.push(ScopeInfo {
            file_scope_id,
            current_loop: None,
            narrowing_aliases: saved_aliases,
            open_blocks: saved_open_blocks,
            nested_global_or_nonlocal_declarations: FxHashMap::default(),
            this_scope_global_or_nonlocal_declarations: FxHashMap::default(),
            pending_captures: FxHashMap::default(),
        });
    }

    // Records snapshots of the place states visible from the current eager scope.
    fn record_eager_snapshots(&mut self, popped_scope_id: FileScopeId) {
        let popped_scope = &self.scopes[popped_scope_id];
        let popped_scope_is_annotation_scope = popped_scope.kind().is_annotation();

        // If the scope that we just popped off is an eager scope, we need to "lock" our view of
        // which bindings reach each of the uses in the scope. Loop through each enclosing scope,
        // looking for any that bind each place.
        // TODO: Bindings in eager nested scopes also need to be recorded. For example:
        // ```python
        // class C:
        //     x: int | None = None
        // c = C()
        // class _:
        //     c.x = 1
        // reveal_type(c.x)  # revealed: Literal[1]
        // ```
        for enclosing_scope_info in self.scope_stack.iter().rev() {
            let enclosing_scope_id = enclosing_scope_info.file_scope_id;
            let is_immediately_enclosing_scope = popped_scope.parent() == Some(enclosing_scope_id);
            let enclosing_scope_kind = self.scopes[enclosing_scope_id].kind();
            let enclosing_place_table = &self.place_tables[enclosing_scope_id];

            for nested_place in self.place_tables[popped_scope_id].iter() {
                // Skip this place if this enclosing scope doesn't contain any bindings for it.
                // Note that even if this place is bound in the popped scope,
                // it may refer to the enclosing scope bindings
                // so we also need to snapshot the bindings of the enclosing scope.

                let Some(enclosing_place_id) = enclosing_place_table.place_id(nested_place) else {
                    continue;
                };
                let enclosing_place = enclosing_place_table.place(enclosing_place_id);

                // Snapshot the state of this place that are visible at this point in this
                // enclosing scope.
                let key = EnclosingSnapshotKey {
                    enclosing_scope: enclosing_scope_id,
                    enclosing_place: enclosing_place_id,
                    nested_scope: popped_scope_id,
                    nested_laziness: ScopeLaziness::Eager,
                };
                let eager_snapshot = self.use_def_maps[enclosing_scope_id]
                    .snapshot_enclosing_state(
                        enclosing_place_id,
                        enclosing_scope_kind,
                        enclosing_place,
                        popped_scope_is_annotation_scope && is_immediately_enclosing_scope,
                    );
                self.enclosing_snapshots.insert(key, eager_snapshot);
            }

            // Lazy scopes are "sticky": once we see a lazy scope we stop doing lookups
            // eagerly, even if we would encounter another eager enclosing scope later on.
            if !enclosing_scope_kind.is_eager() {
                break;
            }
        }
    }

    fn bound_scope(&self, enclosing_scope: FileScopeId, symbol: &Symbol) -> Option<FileScopeId> {
        self.scope_stack
            .iter()
            .rev()
            .skip_while(|scope| scope.file_scope_id != enclosing_scope)
            .find_map(|scope_info| {
                let scope_id = scope_info.file_scope_id;
                let place_table = &self.place_tables[scope_id];
                let place_id = place_table.symbol_id(symbol.name())?;
                place_table.place(place_id).is_bound().then_some(scope_id)
            })
    }

    fn register_pending_capture(&mut self, capture: UnresolvedCapture) {
        let current_scope = self.current_scope();
        let binding_definition_ids = self.place_tables[current_scope]
            .symbol_id(&capture.name)
            .into_iter()
            .flat_map(|symbol| {
                self.use_def_maps[current_scope].symbol_binding_definition_ids(symbol)
            })
            .collect();

        let captures = self
            .current_scope_info_mut()
            .pending_captures
            .entry(capture.name)
            .or_default();

        if let Some(pending) = captures
            .iter_mut()
            .find(|pending| pending.nested_scope == capture.nested_scope)
        {
            pending
                .binding_definition_ids
                .extend(binding_definition_ids);
            if capture.laziness.is_lazy() {
                pending.laziness = ScopeLaziness::Lazy;
            }
        } else {
            captures.push(PendingCapture {
                nested_scope: capture.nested_scope,
                laziness: capture.laziness,
                binding_definition_ids,
            });
        }
    }

    fn record_pending_capture_binding(
        &mut self,
        symbol: ScopedSymbolId,
        definition_id: ScopedDefinitionId,
    ) {
        let current_scope = self.current_scope();
        let name = self.place_tables[current_scope]
            .symbol(symbol)
            .name()
            .clone();

        let Some(captures) = self
            .current_scope_info_mut()
            .pending_captures
            .get_mut(&name)
        else {
            return;
        };

        for capture in captures {
            if capture.laziness.is_lazy() {
                capture.binding_definition_ids.push(definition_id);
            }
        }
    }

    fn finish_pending_captures(
        &mut self,
        popped_scope_id: FileScopeId,
        popped_scope_laziness: ScopeLaziness,
        pending_captures: PendingCaptures,
    ) -> Vec<UnresolvedCapture> {
        let mut unresolved = Vec::new();

        #[expect(
            clippy::iter_over_hash_type,
            reason = "capture resolution and usage marking are order-independent"
        )]
        for (name, captures) in pending_captures {
            for capture in captures {
                if self.resolve_nested_reference_scope(capture.nested_scope, &name)
                    == Some(popped_scope_id)
                {
                    self.use_def_maps[popped_scope_id]
                        .mark_binding_definitions_used(capture.binding_definition_ids);
                } else {
                    unresolved.push(
                        UnresolvedCapture {
                            nested_scope: capture.nested_scope,
                            name: name.clone(),
                            laziness: capture.laziness,
                        }
                        .through_scope(popped_scope_laziness),
                    );
                }
            }
        }

        for symbol in self.place_tables[popped_scope_id].symbols() {
            if symbol.is_used() && !symbol.is_local() && !symbol.is_global() {
                unresolved.push(UnresolvedCapture {
                    nested_scope: popped_scope_id,
                    name: symbol.name().clone(),
                    laziness: popped_scope_laziness,
                });
            }
        }

        unresolved
    }

    // Records snapshots of the place states visible from the current lazy scope.
    fn record_lazy_snapshots(&mut self, popped_scope_id: FileScopeId) {
        for enclosing_scope_info in self.scope_stack.iter().rev() {
            let enclosing_scope_id = enclosing_scope_info.file_scope_id;
            let enclosing_scope_kind = self.scopes[enclosing_scope_id].kind();
            let enclosing_place_table = &self.place_tables[enclosing_scope_id];

            // We don't record lazy snapshots of attributes or subscripts, because these are difficult to track as they modify.
            for nested_symbol in self.place_tables[popped_scope_id].symbols() {
                // For the same reason, we don't snapshot bindings owned by `global`/`nonlocal`
                // forwarding declarations here; `snapshot_enclosing_state` stores only a
                // constraint for those symbols. Also, if the enclosing scope allows its members to
                // be modified from elsewhere, the snapshot will not be recorded.
                // (In the case of class scopes, class variables can be modified from elsewhere, but this has no effect in nested scopes,
                // as class variables are not visible to them)
                if self.scopes[enclosing_scope_id].kind().is_module() {
                    continue;
                }

                // Skip this place if this enclosing scope doesn't contain any bindings for it.
                // Note that even if this place is bound in the popped scope,
                // it may refer to the enclosing scope bindings
                // so we also need to snapshot the bindings of the enclosing scope.
                let Some(enclosed_symbol_id) =
                    enclosing_place_table.symbol_id(nested_symbol.name())
                else {
                    continue;
                };
                let enclosing_place = enclosing_place_table.symbol(enclosed_symbol_id);
                if !enclosing_place.is_bound() {
                    // If the bound scope of a place can be modified from elsewhere, the snapshot will not be recorded.
                    if self
                        .bound_scope(enclosing_scope_id, nested_symbol)
                        .is_none_or(|scope| self.scopes[scope].visibility().is_public())
                    {
                        continue;
                    }
                }

                // Snapshot the state of this place that are visible at this point in this
                // enclosing scope (this may later be invalidated and swept away).
                let key = EnclosingSnapshotKey {
                    enclosing_scope: enclosing_scope_id,
                    enclosing_place: enclosed_symbol_id.into(),
                    nested_scope: popped_scope_id,
                    nested_laziness: ScopeLaziness::Lazy,
                };
                let lazy_snapshot = self.use_def_maps[enclosing_scope_id].snapshot_enclosing_state(
                    enclosed_symbol_id.into(),
                    enclosing_scope_kind,
                    enclosing_place.into(),
                    false,
                );
                self.enclosing_snapshots.insert(key, lazy_snapshot);
            }
        }
    }

    /// Any lazy snapshots of the place that have been reassigned are obsolete, so update them.
    /// ```py
    /// def outer() -> None:
    ///     x = None
    ///
    ///     def inner2() -> None:
    ///         # `inner` can be referenced before its definition,
    ///         # but `inner2` must still be called after the definition of `inner` for this call to be valid.
    ///         inner()
    ///
    ///         # In this scope, `x` may refer to `x = None` or `x = 1`.
    ///         reveal_type(x)  # revealed: None | Literal[1]
    ///
    ///     # Reassignment of `x` after the definition of `inner2`.
    ///     # Update lazy snapshots of `x` for `inner2`.
    ///     x = 1
    ///
    ///     def inner() -> None:
    ///         # In this scope, `x = None` appears as being shadowed by `x = 1`.
    ///         reveal_type(x)  # revealed: Literal[1]
    ///
    ///     # No reassignment of `x` after the definition of `inner`, so we can safely use a lazy snapshot for `inner` as is.
    ///     inner()
    ///     inner2()
    /// ```
    fn update_lazy_snapshots(&mut self, symbol: ScopedSymbolId) {
        let current_scope = self.current_scope();
        let current_place_table = &self.place_tables[current_scope];
        let symbol = current_place_table.symbol(symbol);
        // Optimization: if this is the first binding of the symbol we've seen, there can't be any
        // lazy snapshots of it to update.
        if !symbol.is_reassigned() {
            return;
        }
        #[expect(
            clippy::iter_over_hash_type,
            reason = "each matching snapshot is updated independently"
        )]
        for (key, snapshot_id) in &self.enclosing_snapshots {
            if let Some(enclosing_symbol) = key.enclosing_place.as_symbol() {
                let name = self.place_tables[key.enclosing_scope]
                    .symbol(enclosing_symbol)
                    .name();
                let is_reassignment_of_snapshotted_symbol = || {
                    for (ancestor, _) in self.visible_ancestor_scopes(key.enclosing_scope) {
                        if ancestor == current_scope {
                            return true;
                        }
                        let ancestor_table = &self.place_tables[ancestor];
                        // If there is a symbol binding in an ancestor scope,
                        // then a reassignment in the current scope is not relevant to the snapshot.
                        if ancestor_table
                            .symbol_id(name)
                            .is_some_and(|id| ancestor_table.symbol(id).is_bound())
                        {
                            return false;
                        }
                    }
                    false
                };

                if key.nested_laziness.is_lazy()
                    && symbol.name() == name
                    && is_reassignment_of_snapshotted_symbol()
                {
                    self.use_def_maps[key.enclosing_scope]
                        .update_enclosing_snapshot(*snapshot_id, enclosing_symbol);
                }
            }
        }
    }

    fn sweep_nonlocal_lazy_snapshots(&mut self) {
        self.enclosing_snapshots.retain(|key, _| {
            let place_table = &self.place_tables[key.enclosing_scope];

            let is_bound_and_non_local = || -> bool {
                let ScopedPlaceId::Symbol(symbol_id) = key.enclosing_place else {
                    return false;
                };

                let symbol = place_table.symbol(symbol_id);
                self.scopes
                    .iter_enumerated()
                    .skip_while(|(scope_id, _)| *scope_id != key.enclosing_scope)
                    .any(|(scope_id, _)| {
                        let other_scope_place_table = &self.place_tables[scope_id];
                        let Some(symbol_id) = other_scope_place_table.symbol_id(symbol.name())
                        else {
                            return false;
                        };
                        let symbol = other_scope_place_table.symbol(symbol_id);
                        symbol.is_nonlocal() && symbol.is_bound()
                    })
            };

            key.nested_laziness.is_eager() || !is_bound_and_non_local()
        });
    }

    /// Finds the nearest visible ancestor scope that actually owns a local binding for `name`.
    fn resolve_nested_reference_scope(
        &self,
        nested_scope: FileScopeId,
        name: &str,
    ) -> Option<FileScopeId> {
        self.visible_ancestor_scopes(nested_scope)
            .skip(1)
            .find_map(|(scope_id, _)| {
                let place_table = &self.place_tables[scope_id];
                let symbol_id = place_table.symbol_id(name)?;
                let symbol = place_table.symbol(symbol_id);

                // Only a true local binding in an ancestor scope can be the resolution target.
                // `global`/`nonlocal` here are forwarding declarations, not owning bindings.
                symbol.is_local().then_some(scope_id)
            })
    }

    /// Returns the `NestedGlobalOrNonlocalDeclarations` that are still visible to the enclosing
    /// scope, including those contributed by `global` and `nonlocal` keywords in the popped scope,
    /// but excluding nested `nonlocal`s that resolved to the popped scope.
    fn pop_scope(&mut self) -> NestedGlobalOrNonlocalDeclarations {
        self.exception_context_stack_manager.exit_scope();

        let ScopeInfo {
            file_scope_id: popped_scope_id,
            narrowing_aliases,
            open_blocks,
            mut nested_global_or_nonlocal_declarations,
            this_scope_global_or_nonlocal_declarations,
            pending_captures,
            ..
        } = self
            .scope_stack
            .pop()
            .expect("Root scope should be present");
        self.narrowing_aliases = narrowing_aliases;
        self.open_blocks = open_blocks;

        let children_end = self.scopes.next_index();

        let popped_scope = &mut self.scopes[popped_scope_id];
        popped_scope.extend_descendants(children_end);
        let popped_scope_kind = popped_scope.kind();

        let popped_scope_laziness = popped_scope.kind().laziness();

        if popped_scope_laziness.is_eager() {
            self.record_eager_snapshots(popped_scope_id);
        } else {
            self.record_lazy_snapshots(popped_scope_id);
        }

        let unresolved_captures =
            self.finish_pending_captures(popped_scope_id, popped_scope_laziness, pending_captures);
        if !self.scope_stack.is_empty() {
            for capture in unresolved_captures {
                self.register_pending_capture(capture);
            }
        }

        // If we've popped the module scope, there is no enclosing scope that needs our nested
        // bindings. Short-circuit here and return an empty map.
        if popped_scope_kind.is_module() {
            debug_assert!(self.scope_stack.is_empty());
            return FxHashMap::default();
        }

        // In the common case where we don't have any nested `global` or `nonlocal` declarations at
        // all, short-circuit so that we don't walk the place table for no reason.
        if nested_global_or_nonlocal_declarations.is_empty()
            && this_scope_global_or_nonlocal_declarations.is_empty()
        {
            return FxHashMap::default();
        }

        // For each symbol in the (non-module) scope we just popped:
        // 1. If the popped scope is function-like (not a class body), see whether it resolves any
        //    `nonlocal` declarations (legally or illegally) from further nested scopes.
        // 2. See whether it contributes any nested `global` or `nonlocal` declarations to the
        //    enclosing scope.
        for symbol in self.place_tables[popped_scope_id].symbols() {
            // Filter out any nested `nonlocal` declaration that resolve in the popped scope.
            // Typically these resolve to a defined (bound or declared) symbol, but they can also
            // resolve (illegally) to an unbound `global` declaration. Remove any resolved
            // declarations from the `nested_global_or_nonlocal_declarations` map, both so that we
            // don't try to resolve them again, and so that synthetic nested binding definitions we
            // install in enclosing scopes don't see them. (Note that this doesn't affect any
            // synthetic nested binding definitions installed in the popped scope, which have
            // already recorded these declarations.)
            if popped_scope_kind.is_function_like()
                && (symbol.is_local() || symbol.is_global())
                && let Some(nested_declarations) =
                    nested_global_or_nonlocal_declarations.get_mut(symbol.name())
            {
                nested_declarations.retain(|declaration| {
                    if matches!(declaration.kind, GlobalOrNonlocal::Nonlocal) {
                        // It's a syntax error for a `nonlocal` declaration to resolve to a
                        // `global` statement in an enclosing scope.
                        if symbol.is_global() {
                            self.report_semantic_error(SemanticSyntaxError {
                                kind: SemanticSyntaxErrorKind::NonlocalWithoutBinding(
                                    symbol.name().to_string(),
                                ),
                                range: declaration.range,
                                python_version: self.python_version(),
                            });
                        }
                        // This `nonlocal` is resolved.
                        false
                    } else {
                        // Nested `global` declarations never "resolve" per se. This is both
                        // because we already know they refer to the global scope, and also because
                        // they can "pass through" intervening scopes where they're not visible to
                        // containing scopes where they're visible again.
                        true
                    }
                });
                // If we've resolved all the nested declarations for a symbol, remove it from the
                // map entirely. It wouldn't break anything to keep an empty list, but this avoids
                // pointless allocations in enclosing scopes.
                if nested_declarations.is_empty() {
                    nested_global_or_nonlocal_declarations.remove(symbol.name());
                }
            }

            // Add in any `global` and `nonlocal` declarations from this (non-module) scope.
            if symbol.is_global() || symbol.is_nonlocal() {
                let kind = if symbol.is_global() {
                    GlobalOrNonlocal::Global
                } else {
                    GlobalOrNonlocal::Nonlocal
                };
                nested_global_or_nonlocal_declarations
                    .entry(symbol.name().clone())
                    .or_default()
                    .push(NestedDeclaration {
                        kind,
                        file_scope_id: popped_scope_id,
                        range: *this_scope_global_or_nonlocal_declarations
                            .get(symbol.name())
                            .expect("should have recorded a TextRange"),
                        // This `is_bound` flag is why we wait until now to record these,
                        // rather than doing it when we encounter the keywords.
                        is_bound: symbol.is_bound(),
                    });
            }
        }

        // If the enclosing scope is the module scope, it's a semantic syntax error error to have
        // any remaining unresolved `nonlocal` declarations.
        if self.scope_stack.len() == 1 {
            debug_assert!(
                self.scopes[self.scope_stack[0].file_scope_id]
                    .kind()
                    .is_module(),
                "the last remaining scope should be the module scope",
            );
            #[expect(
                clippy::iter_over_hash_type,
                reason = "iteration order does not affect the semantic errors produced"
            )]
            for (name, nested_declarations) in &nested_global_or_nonlocal_declarations {
                for declaration in nested_declarations {
                    if matches!(declaration.kind, GlobalOrNonlocal::Nonlocal) {
                        self.report_semantic_error(SemanticSyntaxError {
                            kind: SemanticSyntaxErrorKind::NonlocalWithoutBinding(name.to_string()),
                            range: declaration.range,
                            python_version: self.python_version(),
                        });
                    }
                }
            }
        }

        // Now we've updated `nested_global_or_nonlocal_declarations` based on what happened in the
        // popped scope (resolutions and new declarations). Merge the whole map with the caller's
        // own `nested_global_or_nonlocal_declarations` here. Note that we'll *also* return it
        // immediately below, so that the caller can synthesize nested bindings definitions that
        // only respect the bindings within the popped scope, rather than in all the nested scopes
        // they've encountered so far.
        if !popped_scope_kind.is_module() {
            #[expect(
                clippy::iter_over_hash_type,
                reason = "declarations for distinct names are merged independently"
            )]
            for (name, declarations) in &nested_global_or_nonlocal_declarations {
                self.current_scope_info_mut()
                    .nested_global_or_nonlocal_declarations
                    .entry(name.clone())
                    .or_default()
                    .extend_from_slice(declarations);
            }
        }

        // Here's the return described above.
        nested_global_or_nonlocal_declarations
    }

    fn current_place_table(&self) -> &PlaceTableBuilder {
        let scope_id = self.current_scope();
        &self.place_tables[scope_id]
    }

    fn current_place_table_mut(&mut self) -> &mut PlaceTableBuilder {
        let scope_id = self.current_scope();
        &mut self.place_tables[scope_id]
    }

    fn current_use_def_map_mut(&mut self) -> &mut UseDefMapBuilder<'db> {
        let scope_id = self.current_scope();
        &mut self.use_def_maps[scope_id]
    }

    fn current_use_def_map(&self) -> &UseDefMapBuilder<'db> {
        let scope_id = self.current_scope();
        &self.use_def_maps[scope_id]
    }

    fn current_reachability_constraints_mut(&mut self) -> &mut ReachabilityConstraintsBuilder {
        let scope_id = self.current_scope();
        &mut self.use_def_maps[scope_id].reachability_constraints
    }

    fn current_ast_ids(&self) -> &AstIdsBuilder {
        let scope_id = self.current_scope();
        &self.ast_ids[scope_id]
    }

    fn current_ast_ids_mut(&mut self) -> &mut AstIdsBuilder {
        let scope_id = self.current_scope();
        &mut self.ast_ids[scope_id]
    }

    /// If the given expression is a use of a fluid specialization candidate binding,
    /// returns the definition of the candidate.
    fn fluid_candidate_binding(&self, candidate_use: &ast::Expr) -> Option<Definition<'db>> {
        let use_def = self.current_use_def_map();
        let use_id = self.current_ast_ids().try_use_id(candidate_use)?;

        use_def
            .bindings_at_use(use_id)
            .filter_map(|binding| use_def.definition(binding.binding()).definition())
            .filter(|definition| {
                definition
                    .kind(self.db)
                    .as_unannotated_assignment()
                    .is_some_and(|assignment| {
                        is_fluid_specialization_candidate(assignment.value(self.module))
                    })
            })
            // TODO: Support uses that refer to multiple definitions. This currently seems to lead to
            // cycle-related panics.
            .exactly_one()
            .ok()
    }

    /// Try to register a narrowing alias for a simple name assignment.
    ///
    /// Any pre-existing alias entry for the `target` name has already been removed by
    /// [`Self::invalidate_narrowing_aliases_for`] in the binding pathway that ran before
    /// this call, so we only need to decide whether to insert a new entry.
    fn try_register_narrowing_alias(&mut self, target: &ast::Expr, value: Option<&'ast ast::Expr>) {
        let Some(target_name_expr) = target.as_name_expr() else {
            return;
        };
        let Some(value) = value else { return };
        let target_name = &target_name_expr.id;

        if !Self::can_register_narrowing_alias(value) {
            return;
        }

        let place_table = self.current_place_table();
        let narrowed_places =
            PossiblyNarrowedPlacesBuilder::new(self.db, place_table).expression(value);

        // Don't register if the target itself is one of the narrowed places (e.g. `x = x is None`),
        // since the alias would be invalidated immediately by this same assignment.
        let target_is_narrowed = place_table
            .symbol_id(target_name)
            .is_some_and(|symbol| narrowed_places.contains(&symbol.into()));

        if !narrowed_places.is_empty() && !target_is_narrowed {
            self.narrowing_aliases.insert(
                target_name.clone(),
                NarrowingAlias {
                    expression: value,
                    expression_scope: self.current_scope(),
                    narrowed_places,
                },
            );
        }
    }

    /// Invalidate any narrowing aliases affected by a new definition of `place`.
    fn invalidate_narrowing_aliases_for(&mut self, place: ScopedPlaceId) {
        let place_table = &self.place_tables[self.current_scope()];
        let associated_members = place_table.associated_place_ids(place);
        let reassigned_alias_name = place
            .as_symbol()
            .map(|symbol_id| place_table.symbol(symbol_id).name());

        self.narrowing_aliases.retain(|name, alias| {
            // Drop aliases that narrow the reassigned place or any of its members.
            //  e.g. `is_none = x is None and ...; x = 1`
            if alias.narrowed_places.contains(&place) {
                return false;
            }

            //  e.g. `is_none = a.x is None; a = A()`
            if associated_members
                .iter()
                .any(|m| alias.narrowed_places.contains(&(*m).into()))
            {
                return false;
            }

            // Drop the alias whose own variable is the reassigned place.
            // e.g. `is_none = x is None; is_none = False`
            reassigned_alias_name != Some(name)
        });
    }

    fn can_register_narrowing_alias(value: &ast::Expr) -> bool {
        match value {
            // Bare names are too common to treat as alias candidates on every assignment,
            // and doing so would noticeably degrade performance. Excluding them only means
            // we don't infer truthiness narrowing for arbitrary chained aliases.
            ast::Expr::Name(_) => false,
            ast::Expr::Compare(_) | ast::Expr::Call(_) => true,
            ast::Expr::UnaryOp(unary) if unary.op == ast::UnaryOp::Not => {
                Self::can_register_narrowing_alias(&unary.operand)
            }
            ast::Expr::BoolOp(bool_op) => bool_op
                .values
                .iter()
                .any(Self::can_register_narrowing_alias),
            ast::Expr::If(expr_if) => {
                Self::can_register_narrowing_alias(&expr_if.test)
                    || Self::can_register_narrowing_alias(&expr_if.body)
                    || Self::can_register_narrowing_alias(&expr_if.orelse)
            }
            _ => false,
        }
    }

    /// Walk a predicate expression tree, calling `f` on each leaf position
    /// where an alias Name could appear.
    fn walk_narrowing_alias_predicate<'expr>(
        expr: &'expr ast::Expr,
        f: &mut impl FnMut(&'expr ast::Expr),
    ) {
        match expr {
            ast::Expr::Name(_) => f(expr),
            ast::Expr::UnaryOp(unary) if unary.op == ast::UnaryOp::Not => {
                Self::walk_narrowing_alias_predicate(&unary.operand, f);
            }
            ast::Expr::BoolOp(bool_op) => {
                for value in &bool_op.values {
                    Self::walk_narrowing_alias_predicate(value, f);
                }
            }
            ast::Expr::Call(call) => {
                for arg in &call.arguments.args {
                    Self::walk_narrowing_alias_predicate(arg, f);
                }
                for keyword in &call.arguments.keywords {
                    Self::walk_narrowing_alias_predicate(&keyword.value, f);
                }
            }
            ast::Expr::If(expr_if) => {
                Self::walk_narrowing_alias_predicate(&expr_if.test, f);
                Self::walk_narrowing_alias_predicate(&expr_if.body, f);
                Self::walk_narrowing_alias_predicate(&expr_if.orelse, f);
            }
            ast::Expr::Named(expr_named) => {
                Self::walk_narrowing_alias_predicate(&expr_named.value, f);
            }
            _ => {}
        }
    }

    /// Register alias predicates for alias Names found in a predicate expression.
    fn register_narrowing_alias_predicates(&mut self, expr: &'ast ast::Expr) {
        Self::walk_narrowing_alias_predicate(expr, &mut |leaf| {
            let Some(name) = leaf.as_name_expr() else {
                return;
            };
            let Some(alias) = self.narrowing_aliases.get(&name.id) else {
                return;
            };
            if self.current_ast_ids().try_use_id(leaf).is_none() {
                return;
            }

            let aliased_expression = Expression::new(
                self.db,
                self.scope_ids_by_scope[alias.expression_scope],
                AstNodeRef::new(self.module, alias.expression),
                None,
                ExpressionKind::Normal,
            );
            self.alias_predicates.insert(
                ExpressionNodeKey::from(leaf),
                NarrowingAliasPredicate {
                    expression: aliased_expression,
                },
            );
        });
    }

    /// Add narrowed places from aliased expressions to the possibly-narrowed set.
    fn add_alias_narrowed_places(&self, expr: &ast::Expr, places: &mut PossiblyNarrowedPlaces) {
        Self::walk_narrowing_alias_predicate(expr, &mut |leaf| {
            let key = ExpressionNodeKey::from(leaf);
            if let Some(alias_predicate) = self.alias_predicates.get(&key) {
                let aliased_node = alias_predicate
                    .expression
                    .node_ref(self.db)
                    .node(self.module);
                let aliased_places =
                    PossiblyNarrowedPlacesBuilder::new(self.db, self.current_place_table())
                        .expression(aliased_node);
                places.extend(aliased_places);
            }
        });
    }

    fn flow_snapshot(&self) -> FlowSnapshot {
        self.current_use_def_map().snapshot()
    }

    /// Takes specialized truthy/falsy flow states for condition expressions whose evaluation can
    /// leave different bindings behind depending on the condition outcome.
    fn take_condition_flow_snapshots(
        &mut self,
        expr: &ast::Expr,
    ) -> Option<ConditionFlowSnapshots> {
        match expr {
            ast::Expr::BoolOp(_) => self
                .condition_flow_snapshots_by_node
                .remove(&ExpressionNodeKey::from(expr)),
            ast::Expr::UnaryOp(unary_op) if unary_op.op == ast::UnaryOp::Not => {
                let snapshots = self.take_condition_flow_snapshots(&unary_op.operand)?;
                Some(ConditionFlowSnapshots {
                    truthy: snapshots.falsy,
                    falsy: snapshots.truthy,
                })
            }
            _ => None,
        }
    }

    fn flow_snapshot_for_condition(&mut self, condition: &ast::Expr) -> ConditionFlowSnapshot {
        self.record_exception_checkpoint_if(!Self::condition_evaluation_is_known_safe(condition));

        if let Some(snapshots) = self.take_condition_flow_snapshots(condition) {
            ConditionFlowSnapshot::Branches(snapshots)
        } else {
            ConditionFlowSnapshot::Fallback
        }
    }

    fn flow_restore(&mut self, state: FlowSnapshot) {
        self.current_use_def_map_mut().restore(state);
    }

    fn flow_merge(&mut self, state: FlowSnapshot) {
        self.current_use_def_map_mut().merge(state);
    }

    /// Add a symbol to the place table and the use-def map.
    /// Return the [`ScopedPlaceId`] that uniquely identifies the symbol in both.
    fn add_symbol(&mut self, name: Name) -> ScopedSymbolId {
        let (symbol_id, added) = self.current_place_table_mut().add_symbol(Symbol::new(name));
        if added {
            self.current_use_def_map_mut().add_place(symbol_id.into());
        }
        symbol_id
    }

    /// Add a place to the place table and the use-def map.
    /// Return the [`ScopedPlaceId`] that uniquely identifies the place in both.
    fn add_place(&mut self, place_expr: PlaceExpr) -> ScopedPlaceId {
        let (place_id, added) = self.current_place_table_mut().add_place(place_expr);
        if added {
            self.current_use_def_map_mut().add_place(place_id);
        }
        place_id
    }

    #[track_caller]
    fn mark_place_bound(&mut self, id: ScopedPlaceId) {
        self.current_place_table_mut().mark_bound(id);
    }

    /// basedpython: [`Self::mark_place_bound`] for a bare `case A:` capture.
    #[track_caller]
    fn mark_place_bound_by_case_name(&mut self, id: ScopedPlaceId) {
        self.current_place_table_mut().mark_bound_by_case_name(id);
    }

    /// basedpython: [`Self::mark_place_bound`] for a bare assignment in a
    /// trailing lambda block.
    #[track_caller]
    fn mark_place_bound_by_block_assignment(&mut self, id: ScopedPlaceId) {
        self.current_place_table_mut()
            .mark_bound_by_block_assignment(id);
    }

    /// basedpython: whether the scope being built is a trailing lambda block's
    /// body, where a bare assignment writes to the block receiver's member
    /// rather than binding a name of its own.
    fn in_trailing_lambda_block(&self) -> bool {
        matches!(
            self.scopes[self.current_scope()].node(),
            NodeWithScopeKind::Function(function) if function.node(self.module).is_trailing_lambda
        )
    }

    /// basedpython: whether the function whose body this scope is wrote down no return type, so
    /// that its body is the only statement of what it returns.
    ///
    /// A written return type is what the function declares, and nothing about the returned
    /// expression beyond its type is then anybody's business. Without one the body is the only
    /// source there is, and what a returned expression narrows is part of what it says.
    fn enclosing_function_wrote_down_no_return_type(&self) -> bool {
        matches!(
            self.scopes[self.current_scope()].node(),
            NodeWithScopeKind::Function(function)
                if {
                    let function = function.node(self.module);
                    function.returns.is_none() && !function.is_asserts_return
                }
        )
    }

    #[track_caller]
    fn mark_place_declared(&mut self, id: ScopedPlaceId) {
        self.current_place_table_mut().mark_declared(id);
    }

    #[track_caller]
    fn mark_symbol_used(&mut self, id: ScopedSymbolId) {
        self.current_place_table_mut().symbol_mut(id).mark_used();
    }

    fn record_place_use(&mut self, place_id: ScopedPlaceId, expr: &'ast ast::Expr) {
        if let ScopedPlaceId::Symbol(symbol_id) = place_id {
            self.mark_symbol_used(symbol_id);
        }
        let use_id = self.current_ast_ids_mut().record_use(expr);
        self.current_use_def_map_mut().record_use(place_id, use_id);
    }

    /// basedpython: capture the state of the places below the place `value` names, alongside the
    /// use the walk of `value` has just recorded for it.
    ///
    /// Narrowing that established something about `a.b` is recorded on `a.b`, so returning `a`
    /// would ordinarily leave it behind. The recovered return type carries it instead — see
    /// `ty_python_semantic::types::inferred_narrowing`.
    fn record_returned_place_members(&mut self, value: &'ast ast::Expr) {
        let scope = self.current_scope();
        let Some(use_id) = self.ast_ids[scope].try_use_id(value) else {
            return;
        };
        let place_table = &self.place_tables[scope];
        let Some(place_id) =
            PlaceExpr::try_from_expr(value).and_then(|place| place_table.place_id((&place).into()))
        else {
            return;
        };
        let members: Vec<ScopedPlaceId> = place_table
            .associated_place_ids(place_id)
            .iter()
            .copied()
            .map(ScopedPlaceId::from)
            .collect();
        if members.is_empty() {
            return;
        }
        self.use_def_maps[scope].record_places_at_use(members.into_iter(), use_id);
    }

    /// basedpython: records `expr` as a value of the statement expression whose
    /// place is `place`.
    ///
    /// Never inlined: this runs from the tail of [`Self::visit_expr`], so its
    /// locals would otherwise sit in the frame of every expression visited.
    #[inline(never)]
    fn record_statement_expression_value(&mut self, expr: &'ast ast::Expr, place: ScopedPlaceId) {
        // not keyed by AST node (see `is_statement_expression_value`), so the
        // single-definition-per-node invariant `add_definition` checks does not apply
        self.push_additional_definition(place, DefinitionNodeRef::StatementExpressionValue(expr));
    }

    /// basedpython: reports a `break <value>` whose value nothing reads.
    ///
    /// A value only means something when the loop the `break` leaves is a
    /// statement expression, which is exactly when the value was registered as
    /// one of that statement's value positions.
    ///
    /// Never inlined, for the same reason as
    /// [`Self::record_statement_expression_value`] — [`Visitor::visit_stmt`] is
    /// recursive too.
    #[inline(never)]
    fn check_break_value(&mut self, stmt: &'ast ast::Stmt, value: &'ast ast::Expr) {
        let key = ExpressionNodeKey::from(value);
        if self
            .current_statement_expressions
            .iter()
            .any(|current| current.values.contains(&key))
        {
            return;
        }
        self.report_semantic_error(SemanticSyntaxError {
            kind: SemanticSyntaxErrorKind::DiscardedBreakValue,
            range: stmt.range(),
            python_version: self.python_version,
        });
    }

    /// basedpython: `a ?? b`, whose right operand is a branch.
    ///
    /// Kept out of [`Self::visit_expr`] — and never inlined back into it —
    /// because the snapshots it holds would otherwise sit in the stack frame of
    /// *every* expression visited. `visit_expr` recurses once per nesting level,
    /// so a long operator chain multiplies whatever this arm costs (see the
    /// `stack_size` server test, a 2000-deep `1 + 1 + …`).
    #[inline(never)]
    fn visit_coalesce_expression(&mut self, left: &'ast ast::Expr, right: &'ast ast::Expr) {
        self.visit_expr(left);
        let left_was_not_none = self.flow_snapshot();

        // whether the left operand is `None` is not expressible as a narrowing
        // predicate here, so *both* paths are recorded as ambiguous. they have to
        // be complementary: leaving the right-operand-skipped path unconstrained
        // would make the implicit `unbound` binding definitely visible alongside a
        // binding made in the right operand, which is a contradiction
        self.record_ambiguous_reachability();
        self.visit_expr(right);
        let right_ran = self.flow_snapshot();

        self.flow_restore(left_was_not_none);
        self.record_ambiguous_reachability();
        self.flow_merge(right_ran);
    }

    /// basedpython: a statement expression's wrapped statement is visited as an
    /// ordinary statement, so everything it binds and narrows is recorded in the
    /// enclosing scope. Its *value* is modelled as a synthetic place written at
    /// each of the statement's value positions and read at the expression itself,
    /// which gives exhaustiveness and the union of branch types from the existing
    /// flow analysis.
    ///
    /// Kept out of [`Self::visit_expr`] for the same reason as
    /// [`Self::visit_coalesce_expression`].
    #[inline(never)]
    fn visit_statement_expression(
        &mut self,
        expr: &'ast ast::Expr,
        statement: &'ast ast::ExprStatement,
    ) {
        // basedpython: a trailing lambda block produces no tail values — its
        // value is the call it stands for, which the type checker reads off the
        // block's own inference — so it needs no synthetic place to collect them
        if statement.is_trailing_lambda() {
            self.visit_stmt(&statement.stmt);
            return;
        }

        let place = self
            .add_symbol(Name::new(format!(
                "<statement-expression:{}>",
                statement.range.start().to_u32()
            )))
            .into();
        let values = statement_expression_values(&statement.stmt)
            .into_iter()
            .map(|value| ExpressionNodeKey::from(value.expr()))
            .collect();
        self.current_statement_expressions
            .push(CurrentStatementExpression { place, values });

        self.visit_stmt(&statement.stmt);

        self.current_statement_expressions.pop();
        self.record_place_use(place, expr);
    }

    fn record_place_definition(&mut self, place_id: ScopedPlaceId, expr: &'ast ast::Expr) {
        match self.current_assignment() {
            Some(CurrentAssignment::Assign {
                node,
                unpack,
                owner,
            }) => {
                let assignment = self.add_definition(
                    place_id,
                    AssignmentDefinitionNodeRef {
                        unpack,
                        node,
                        value: &node.value,
                        target: expr,
                        sole_target: node.targets.len() == 1,
                        owner,
                    },
                );

                self.add_dict_key_assignment_definitions(&node.targets, &node.value, assignment);
            }
            Some(CurrentAssignment::AnnAssign {
                node: ann_assign,
                pending,
            }) => {
                self.add_standalone_type_expression(&ann_assign.annotation);
                let assignment = if let Some(pending) = pending {
                    self.finish_annotated_assignment(pending)
                } else {
                    self.add_definition(
                        place_id,
                        AnnotatedAssignmentDefinitionNodeRef { node: ann_assign },
                    )
                };

                if let Some(value) = ann_assign.value.as_deref() {
                    self.add_dict_key_assignment_definitions(
                        [&*ann_assign.target],
                        value,
                        assignment,
                    );
                }
            }
            Some(CurrentAssignment::AugAssign(aug_assign)) => {
                self.add_definition(place_id, aug_assign);
            }
            Some(CurrentAssignment::For { node, unpack }) => {
                self.add_definition(
                    place_id,
                    ForStmtDefinitionNodeRef {
                        unpack,
                        node,
                        target: expr,
                    },
                );
            }
            Some(CurrentAssignment::Named(named)) => {
                self.mark_comprehension_named_target(place_id, named.target.range());
                self.add_definition(place_id, named);
            }
            Some(CurrentAssignment::Comprehension {
                unpack,
                node,
                first,
            }) => {
                self.add_definition(
                    place_id,
                    ComprehensionDefinitionNodeRef {
                        unpack,
                        node,
                        target: expr,
                        first,
                    },
                );
            }
            Some(CurrentAssignment::WithItem {
                item,
                is_async,
                unpack,
            }) => {
                self.add_definition(
                    place_id,
                    WithItemDefinitionNodeRef {
                        unpack,
                        item,
                        target: expr,
                        is_async,
                    },
                );
            }
            None => {}
        }
    }

    fn add_entry_for_definition_key(&mut self, key: DefinitionNodeKey) -> &mut Definitions<'db> {
        self.definitions_by_node.entry(key).or_default()
    }

    /// Add a [`Definition`] associated with the `definition_node` AST node.
    ///
    /// ## Panics
    ///
    /// This method panics if `debug_assertions` are enabled and the `definition_node` AST node
    /// already has a [`Definition`] associated with it. This is an important invariant to maintain
    /// for all nodes *except* [`ast::Alias`] nodes representing `*` imports.
    fn add_definition(
        &mut self,
        place: ScopedPlaceId,
        definition_node: impl Into<DefinitionNodeRef<'ast, 'db>> + std::fmt::Debug + Copy,
    ) -> Definition<'db> {
        let definition = self.create_definition(place, definition_node);
        self.record_definition(place, definition, None);
        definition
    }

    /// Create a definition without making its declaration or binding visible in control flow.
    fn create_definition(
        &mut self,
        place: ScopedPlaceId,
        definition_node: impl Into<DefinitionNodeRef<'ast, 'db>> + std::fmt::Debug + Copy,
    ) -> Definition<'db> {
        let (definition, num_definitions) =
            self.create_additional_definition(place, definition_node);
        debug_assert_eq!(
            num_definitions, 1,
            "Attempted to create multiple `Definition`s associated with AST node {definition_node:?}"
        );
        definition
    }

    fn delete_associated_bindings(&mut self, place: ScopedPlaceId) {
        let scope = self.current_scope();
        // Don't delete associated bindings if the scope is a class scope & place is a name (it's never visible to nested scopes)
        if self.scopes[scope].kind() == ScopeKind::Class && place.is_symbol() {
            return;
        }
        for associated_place in self.place_tables[scope]
            .associated_place_ids(place)
            .iter()
            .copied()
        {
            self.use_def_maps[scope].delete_binding(associated_place.into());
        }
    }

    fn delete_binding(&mut self, place: ScopedPlaceId) {
        self.current_use_def_map_mut().delete_binding(place);
    }

    /// Push a new [`Definition`] onto the list of definitions
    /// associated with the `definition_node` AST node.
    ///
    /// Most AST nodes can only be associated with at most one [`Definition`]. Generally prefer
    /// `add_definition` above, which enforces that. This method should currently only be used with
    /// `*` imports and loop headers.
    fn push_additional_definition(
        &mut self,
        place: ScopedPlaceId,
        definition_node: impl Into<DefinitionNodeRef<'ast, 'db>>,
    ) {
        let (definition, _) = self.create_additional_definition(place, definition_node);
        self.record_definition(place, definition, None);
    }

    /// Create a [`Definition`] without recording it in control flow.
    ///
    /// Returns the new definition and the number of definitions now associated with its AST
    /// node. Loop headers are not stored by AST node, so their count is zero. Prefer
    /// [`Self::create_definition`] when the node must have exactly one definition.
    fn create_additional_definition(
        &mut self,
        place: ScopedPlaceId,
        definition_node: impl Into<DefinitionNodeRef<'ast, 'db>>,
    ) -> (Definition<'db>, usize) {
        let definition_node: DefinitionNodeRef<'ast, 'db> = definition_node.into();

        // Note `definition_node` is guaranteed to be a child of `self.module`
        let kind = definition_node.into_owned(self.module);
        let is_loop_header = kind.is_loop_header();
        let is_statement_expression_value = kind.is_statement_expression_value();
        let is_reexported = kind.is_reexported();

        let definition: Definition<'db> =
            Definition::new(self.db, self.current_scope_id(), place, kind, is_reexported);

        let num_definitions = if is_loop_header || is_statement_expression_value {
            // Loop headers are internal use-def definitions. They are retrieved through the loop
            // token rather than by their AST node. Statement expression values are likewise read
            // back through the statement expression's use.
            0
        } else {
            let definitions = self.add_entry_for_definition_key(definition_node.key());
            definitions.push(definition);
            definitions.len()
        };

        (definition, num_definitions)
    }

    /// Records an already-created definition in the current scope.
    ///
    /// `previous_definitions` controls whether a new binding replaces earlier bindings. By
    /// default, ordinary assignments replace them and loop headers keep them. Comprehension
    /// bindings choose explicitly because an assignment that only runs on some paths must keep
    /// the earlier binding.
    fn record_definition(
        &mut self,
        place: ScopedPlaceId,
        definition: Definition<'db>,
        previous_definitions: Option<PreviousDefinitions>,
    ) {
        let kind = definition.kind(self.db);
        let category = kind.category(self.source_type.is_stub(), self.module);
        match category {
            DefinitionCategory::Declaration => {
                self.mark_place_declared(place);
                self.current_use_def_map_mut()
                    .record_declaration(place, definition);
            }
            DefinitionCategory::DeclarationAndBinding => {
                self.mark_place_declared(place);
                self.record_binding_with(definition, |use_def, place| {
                    use_def.record_combined_definition(place, definition, category);
                });
            }
            DefinitionCategory::Binding => {
                let previous = previous_definitions.unwrap_or(if kind.is_loop_header() {
                    PreviousDefinitions::AreKept
                } else {
                    PreviousDefinitions::AreShadowed
                });
                self.record_binding_with(definition, |use_def, place| {
                    use_def.record_binding(
                        place,
                        definition,
                        previous,
                        FutureDefinitions::ShadowThisOne,
                    );
                });
            }
        }
    }

    /// Declare an annotated name assignment whose value will be bound after visiting its RHS.
    /// Other targets and annotations without a RHS are recorded in full by `add_definition`.
    fn begin_annotated_assignment(
        &mut self,
        node: &'ast ast::StmtAnnAssign,
    ) -> Option<PendingAnnotatedAssignment<'db>> {
        let ast::Expr::Name(name) = &*node.target else {
            return None;
        };
        node.value.as_ref()?;

        let place = self.add_symbol(name.id.clone()).into();
        let definition =
            self.create_definition(place, AnnotatedAssignmentDefinitionNodeRef { node });
        self.mark_place_declared(place);
        self.current_use_def_map_mut().record_combined_definition(
            place,
            definition,
            DefinitionCategory::Declaration,
        );
        Some(PendingAnnotatedAssignment { definition })
    }

    /// Bind the value of an annotated assignment whose declaration was recorded before its RHS.
    fn finish_annotated_assignment(
        &mut self,
        pending: PendingAnnotatedAssignment<'db>,
    ) -> Definition<'db> {
        let definition = pending.definition;
        self.record_binding_with(definition, |use_def, place| {
            use_def.record_combined_definition(place, definition, DefinitionCategory::Binding);
        });
        definition
    }

    /// Record one binding while keeping aliases, captures, and lazy snapshots in sync.
    /// The callback receives the definition's place and must append that binding to the current
    /// use-def map.
    fn record_binding_with(
        &mut self,
        definition: Definition<'db>,
        record: impl FnOnce(&mut UseDefMapBuilder<'db>, ScopedPlaceId),
    ) {
        let place = definition.place(self.db);
        let kind = definition.kind(self.db);
        let is_loop_header = kind.is_loop_header();

        // We need to avoid marking places as bound as soon as we encounter a loop header
        // definition for them, because that would lead to false-positive semantic syntax errors in
        // cases like this:
        // ```py
        // while True:
        //     global x  # [invalid-syntax] if `x` is already used or bound
        //     x = 1
        // ```
        let binds_by_case_name = matches!(
            kind,
            DefinitionKind::MatchPattern(match_pattern) if match_pattern.is_case_name()
        );
        // basedpython: a bare `href = …` in a trailing lambda block writes to the
        // receiver's `href` when the receiver has one, so it is not the block
        // taking the name for itself. a `let` or `var` declaration is, which is
        // why only the plain assignment form is set apart here
        let binds_by_block_assignment =
            matches!(kind, DefinitionKind::Assignment(_)) && self.in_trailing_lambda_block();
        if !is_loop_header {
            if binds_by_case_name {
                self.mark_place_bound_by_case_name(place);
            } else if binds_by_block_assignment {
                self.mark_place_bound_by_block_assignment(place);
            } else {
                self.mark_place_bound(place);
            }
            self.invalidate_narrowing_aliases_for(place);
        }

        let definition_id = self.current_use_def_map().next_definition_id();
        record(self.current_use_def_map_mut(), place);

        if !is_loop_header {
            self.delete_associated_bindings(place);
        }

        if let Some(id) = place.as_symbol() {
            self.record_pending_capture_binding(id, definition_id);
            self.update_lazy_snapshots(id);
        }
    }

    // Creates a definition for each key-value assignment in the dictionary.
    //
    // If there are multiple targets, no definitions will be created.
    fn add_dict_key_assignment_definitions(
        &mut self,
        targets: impl IntoIterator<Item = &'ast ast::Expr> + Copy,
        dict: &'ast ast::Expr,
        assignment: Definition<'db>,
    ) {
        // TODO: Although we synthesize place expressions for each dictionary key, the definition
        // is still uniquely associated with the AST node of the key expression, and so multiple target
        // places cannot refer to the same key.
        let Ok(target) = targets.into_iter().exactly_one() else {
            return;
        };

        if let Some(target) = MemberExprBuilder::visit_expr(target.into()) {
            self.add_dict_key_assignment_definitions_impl(&target, dict.into(), assignment);
        }
    }

    fn add_dict_key_assignment_definitions_impl(
        &mut self,
        target: &MemberExprBuilder,
        expr: ast::ExprRef<'ast>,
        assignment: Definition<'db>,
    ) {
        let ruff_python_ast::ExprRef::Dict(dict) = expr else {
            let items = match expr {
                ruff_python_ast::ExprRef::List(list) => &list.elts,
                ruff_python_ast::ExprRef::Tuple(tuple) => &tuple.elts,
                _ => return,
            };

            // Traverse into nested collections that may contain dictionary literals.
            for (i, item) in items
                .iter()
                // Ignore starred expressions and any elements that follow them, as we cannot
                // determine the index to narrow on.
                .take_while(|e| !e.is_starred_expr())
                .enumerate()
            {
                if let Some(target) = MemberExprBuilder::visit_subscript_expr(
                    target,
                    &ast::Expr::NumberLiteral(ast::ExprNumberLiteral {
                        value: ast::Number::Int(ast::Int::from(i as u64)),
                        range: TextRange::default(),
                        node_index: AtomicNodeIndex::NONE,
                    }),
                ) {
                    self.add_dict_key_assignment_definitions_impl(&target, item.into(), assignment);
                }
            }
            return;
        };

        for item in &dict.items {
            let Some(key) = item.key.as_ref() else {
                continue;
            };

            let Some(member_expr) = MemberExprBuilder::visit_subscript_expr(target, key) else {
                continue;
            };

            if let Some(place_expr) = PlaceExpr::try_from_member_expr(member_expr.clone()) {
                let place_id = self.add_place(place_expr);

                self.add_definition(
                    place_id,
                    DictKeyAssignmentNodeRef {
                        key,
                        assignment,
                        value: &item.value,
                    },
                );

                // Recurse into nested dictionaries.
                //
                // Note that we must do this _after_ adding the outer place in order to track
                // sub-member places correctly.
                self.add_dict_key_assignment_definitions_impl(
                    &member_expr,
                    (&item.value).into(),
                    assignment,
                );
            }
        }
    }

    /// Create loop header definitions for places that are bound or invalidated within a loop.
    /// Return the `LoopHeaderId` referenced by those definitions, the set of place IDs, and the
    /// lower bound `ScopedDefinitionId` for definitions created within the loop.
    fn synthesize_loop_header_definitions(
        &mut self,
        loop_stmt: LoopStmtRef<'ast>,
        bound_places: Vec<PlaceExpr>,
    ) -> (LoopHeaderId, FxHashSet<ScopedPlaceId>, ScopedDefinitionId) {
        let loop_header_id = self.current_use_def_map_mut().reserve_loop_header();
        let bound_places: Vec<_> = bound_places
            .into_iter()
            .map(|place| self.add_place(place))
            .collect();

        // Rebinding `x` also invalidates `x.attr` and `x[index]`. These places need their own
        // headers so that the invalidation reaches uses before the assignment on later
        // iterations. Register all explicit targets first to include their associated places.
        let associated_places: Vec<_> = bound_places
            .iter()
            .flat_map(|place| self.current_place_table().associated_place_ids(*place))
            .copied()
            .map(ScopedPlaceId::from)
            .collect();
        let mut bound_place_ids: FxHashSet<ScopedPlaceId> = FxHashSet::default();
        for place_id in bound_places.into_iter().chain(associated_places) {
            if bound_place_ids.insert(place_id) {
                let loop_header_ref = LoopHeaderDefinitionNodeRef {
                    loop_stmt,
                    place: place_id,
                    loop_header_id,
                };
                // Note that `DefinitionKind::LoopHeader` doesn't shadow prior bindings.
                self.push_additional_definition(place_id, loop_header_ref);
            }
        }
        let loop_min_definition_id = self.current_use_def_map_mut().next_definition_id();
        (loop_header_id, bound_place_ids, loop_min_definition_id)
    }

    /// Build a `LoopHeader` that tracks all the variables bound in a loop, which will be visible
    /// to uses in the same loop via "loop header definitions". We call this after merging control
    /// flow from all the loop-back edges, most importantly at the end of the loop body, and also
    /// at any `continue` statements.
    fn populate_loop_header(
        &mut self,
        loop_header_places: &FxHashSet<ScopedPlaceId>,
        loop_header_id: LoopHeaderId,
        loop_min_definition_id: ScopedDefinitionId,
    ) {
        let mut loop_header = LoopHeader::new();
        let use_def = self.current_use_def_map_mut();
        // Collect all the bindings within the loop that reached a loop back edge. Use the minimum
        // definition ID to filter out all the pre-loop bindings. The loop header doesn't shadow
        // them, so there's no need to duplicate them.
        #[expect(
            clippy::iter_over_hash_type,
            reason = "bindings for distinct places are collected independently"
        )]
        for place_id in loop_header_places {
            for live_binding in use_def.current_bindings(*place_id) {
                if live_binding.binding() >= loop_min_definition_id {
                    loop_header.add_binding(*place_id, live_binding);
                }
            }
        }
        // Mark the reachability and narrowing constraints as used.
        #[expect(
            clippy::iter_over_hash_type,
            reason = "marking reachability constraints as used is idempotent"
        )]
        for place_id in loop_header_places {
            for live_binding in loop_header.bindings_for_place(*place_id) {
                use_def
                    .reachability_constraints
                    .mark_used(live_binding.reachability_constraint());
                use_def
                    .narrowing_constraints
                    .mark_used(live_binding.narrowing_constraint());
            }
        }
        use_def.set_loop_header(loop_header_id, loop_header);
    }

    fn synthesize_nested_binding_definitions(
        &mut self,
        nested_bindings: NestedGlobalOrNonlocalDeclarations,
    ) {
        let mut nested_bindings = nested_bindings.into_iter().collect::<Vec<_>>();
        nested_bindings.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));

        for (name, mut declarations) in nested_bindings {
            // Filter down to only the declarations with `is_bound: true`. If there are none left,
            // skip synthesizing a definition for this symbol. (The reason we track these at all is
            // that we reuse some of the same machinery to report semantic syntax errors for
            // invalid `nonlocal`s, and those don't necessarily need a binding.)
            declarations.retain(|d| d.is_bound);
            declarations.shrink_to_fit();
            if declarations.is_empty() {
                continue;
            }

            let place: ScopedPlaceId = self.add_symbol(name.clone()).into();
            let definition = Definition::new(
                self.db,
                self.current_scope_id(),
                place,
                DefinitionKind::NestedBindings(Box::new(NestedBindingsDefinitionKind {
                    name,
                    execution: NestedBindingExecution::Lazy,
                    nested_declarations: declarations,
                })),
                false,
            );

            // Adding a binding typically invalidates narrowing aliases like
            // `is_int = isinstance(x, int)`. However, for the same reason that we retain both
            // `global` and `nonlocal` nested writes -- we don't necessarily know yet which ones
            // are going to be visible in the current scope -- it's also too early to know whether
            // we should invalidate narrowing aliases. Situations where this matters tend to be
            // *very* contrived, though, for example:
            //
            // ```py
            // x: int | str = 1
            // def _(x: int | str):
            //     is_int = isinstance(x, int)
            //     def _():
            //         global x
            //         x = "hello"
            //     if is_int:
            //         # We should narrow `x` to `int` here, because the global `x` is a different variable.
            //         reveal_type(x)
            // ```
            //
            // TODO: We could be more precise here by delaying invalidation until inference time.
            self.invalidate_narrowing_aliases_for(place);

            self.current_use_def_map_mut().record_binding(
                place,
                definition,
                // Nested bindings definitions are like loop headers in that they don't shadow
                // prior bindings, but they're different in that they *also* don't get shadowed by
                // bindings that come later. The idea is that nested functions can be called at any
                // time, so these bindings are effectively always visible after their function
                // definitions.
                PreviousDefinitions::AreKept,
                FutureDefinitions::DontShadowThisOne,
            );
        }
    }

    /// basedpython: whether a trailing-lambda callee's callback parameter is
    /// marked `once` — the block then runs exactly once, so an assignment to an
    /// enclosing name is a definite narrowing; a non-`once` block may run any
    /// number of times, so the write unions with the prior value.
    ///
    /// Resolved syntactically at build time (type inference is not available
    /// yet): a same-file `Name` callee is looked up in the enclosing scopes'
    /// `def`s. Anything else — an import, a method, a non-`Name` callee, or an
    /// unresolved name — is conservatively *not* `once`, which keeps the write a
    /// union (a sound over-approximation; never a spurious definite narrowing).
    fn trailing_lambda_callee_is_once(&self, callee: &ast::Expr) -> bool {
        let source = self.source_text().as_str();
        self.callee_definition(callee)
            .and_then(|def| last_bound_parameter(&def.parameters))
            .is_some_and(|last| parameter_modifiers(source, last).once)
    }

    /// basedpython: the `def` a trailing-lambda callee names, for the parts of a
    /// block that are settled before type inference runs.
    ///
    /// The nearest enclosing `def` of the name decides. A name bound some other
    /// way — an import, a reassignment, a method, a non-`Name` callee — answers
    /// `None`, and each caller says what it assumes in that case.
    fn callee_definition(&self, callee: &ast::Expr) -> Option<&'ast ast::StmtFunctionDef> {
        let ast::Expr::Name(name) = callee else {
            return None;
        };
        let target = name.id.as_str();
        for scope in self.scope_stack.iter().rev() {
            let body = match self.scopes[scope.file_scope_id].node() {
                NodeWithScopeKind::Function(func) => &func.node(self.module).body,
                NodeWithScopeKind::Module => &self.module.syntax().body,
                _ => continue,
            };
            for stmt in body {
                if let ast::Stmt::FunctionDef(def) = stmt
                    && def.name.as_str() == target
                {
                    return Some(def);
                }
            }
        }
        None
    }

    /// basedpython: whether every path through `body` diverges (a `return` /
    /// `raise`, or an `if` whose main body and every branch — including a final
    /// `else` — diverge). Conservative: it only claims divergence for shapes it
    /// can see through, never guessing. Used to tell whether a `once` block's
    /// guaranteed run also guarantees a return out of the enclosing function.
    fn always_returns(body: &[ast::Stmt]) -> bool {
        let Some(last) = body.last() else {
            return false;
        };
        match last {
            ast::Stmt::Return(_) | ast::Stmt::Raise(_) => true,
            ast::Stmt::With(with) => Self::always_returns(&with.body),
            ast::Stmt::If(if_stmt) => {
                if_stmt
                    .elif_else_clauses
                    .iter()
                    .any(|clause| clause.test.is_none())
                    && Self::always_returns(&if_stmt.body)
                    && if_stmt
                        .elif_else_clauses
                        .iter()
                        .all(|clause| Self::always_returns(&clause.body))
            }
            _ => false,
        }
    }

    /// basedpython: record shadowing write-backs for the enclosing-scope names a
    /// trailing-lambda block assigns.
    ///
    /// A trailing-lambda block (`f:` + suite) runs inline at its call site — the
    /// lowering inserts a matching `global` / `nonlocal` — so a name it binds that
    /// is already bound in an enclosing scope should read as the block's value
    /// after the block, like an inline assignment. This reuses the `NestedBindings`
    /// inference (which yields the block's exit value) but records a *shadowing*
    /// binding for a definite result, rather than the union the general nested-write
    /// case keeps (a general nested function may be called at any time; this block
    /// runs right here).
    fn synthesize_trailing_lambda_writebacks(
        &mut self,
        block_scope: FileScopeId,
        body: &[ast::Stmt],
        is_once: bool,
    ) {
        // whether the block unconditionally rebinds `name` on every path — a
        // top-level `name = …` (annotated or augmented too). a definite rebind
        // shadows the enclosing value (`a = 2` → `2`); a conditional one lets the
        // enclosing value survive on the un-taken path (`if c(): a = 2` → `1 | 2`)
        fn definitely_assigns(body: &[ast::Stmt], name: &str) -> bool {
            fn is_target(expr: &ast::Expr, name: &str) -> bool {
                matches!(expr, ast::Expr::Name(name_expr) if name_expr.id.as_str() == name)
            }
            body.iter().any(|stmt| match stmt {
                ast::Stmt::Assign(assign) => {
                    assign.targets.iter().any(|target| is_target(target, name))
                }
                ast::Stmt::AnnAssign(ann) => ann.value.is_some() && is_target(&ann.target, name),
                ast::Stmt::AugAssign(aug) => is_target(&aug.target, name),
                // a `with` body runs unconditionally
                ast::Stmt::With(with) => definitely_assigns(&with.body, name),
                // an `if` chain assigns definitely only when every branch does —
                // the main body, every `elif`, and a final `else` (without one the
                // fall-through path skips the assignment)
                ast::Stmt::If(if_stmt) => {
                    let has_else = if_stmt
                        .elif_else_clauses
                        .iter()
                        .any(|clause| clause.test.is_none());
                    has_else
                        && definitely_assigns(&if_stmt.body, name)
                        && if_stmt
                            .elif_else_clauses
                            .iter()
                            .all(|clause| definitely_assigns(&clause.body, name))
                }
                _ => false,
            })
        }

        let enclosing = self.current_scope();

        // names the block binds locally, excluding any it already declares
        // `global` / `nonlocal` (those flow through the general nested path)
        let candidates: Vec<Name> = self.place_tables[block_scope]
            .symbols()
            .filter(|symbol| symbol.is_bound() && !symbol.is_global() && !symbol.is_nonlocal())
            .map(|symbol| symbol.name().clone())
            .collect();

        for name in candidates {
            // the nearest enclosing scope that already binds *or declares* this
            // name. a declaration counts (`let a: int` with no value) so a block
            // assignment fills it in rather than reading as a fresh local
            let resolved_scope = self
                .scope_stack
                .iter()
                .rev()
                .skip_while(|scope| scope.file_scope_id != enclosing)
                .find_map(|scope| {
                    let table = &self.place_tables[scope.file_scope_id];
                    let place_id = table.symbol_id(&name)?;
                    let place = table.place(place_id);
                    (place.is_bound() || place.is_declared()).then_some(scope.file_scope_id)
                });

            // only a `once` block runs exactly once; a non-`once` block may run
            // any number of times (including zero), so even an unconditional write
            // unions with the prior value rather than shadowing it
            let definite = is_once && definitely_assigns(body, name.as_str());

            // a name bound nowhere outside the block is a genuinely new binding.
            // it survives the boundary only from a `once` block that
            // *unconditionally* binds it (definite) — the lowering then makes it a
            // `nonlocal` / `global` enclosing local. a conditional or non-`once`
            // write would leave it possibly-unbound, which is not yet modeled, so
            // such a name stays a block local for now
            let is_fresh = resolved_scope.is_none();
            if is_fresh && !definite {
                continue;
            }
            let bound_scope = resolved_scope.unwrap_or(enclosing);

            let kind = if bound_scope == FileScopeId::global() {
                GlobalOrNonlocal::Global
            } else {
                GlobalOrNonlocal::Nonlocal
            };

            let place: ScopedPlaceId = self.add_symbol(name.clone()).into();

            // a fresh binding is only in the block's scope so far; mark it
            // *declared* in the enclosing scope so it resolves as a local there
            // (`is_local` accepts a declaration), which the nested-binding
            // inference needs to see the block's write. it is deliberately not
            // marked *bound*, so the lowering still treats it as a fresh name that
            // needs a `nonlocal` pre-init rather than a plain write-through
            if is_fresh {
                self.mark_place_declared(place);
            }
            let definition = Definition::new(
                self.db,
                self.current_scope_id(),
                place,
                DefinitionKind::NestedBindings(Box::new(NestedBindingsDefinitionKind {
                    name,
                    // `Eager` models a comprehension, whose binding is one element of an
                    // iteration and is promoted for that reason. A block's write is an
                    // ordinary assignment — after `a = 2` the enclosing `a` is `2`, not
                    // `int` — so both kinds are `Lazy` here. Whether a `once` block's
                    // write shadows the prior value or unions with it is decided by the
                    // writeback synthesis, not by this flag.
                    execution: NestedBindingExecution::Lazy,
                    nested_declarations: std::iter::once(NestedDeclaration {
                        kind,
                        file_scope_id: block_scope,
                        range: TextRange::default(),
                        is_bound: true,
                    })
                    .collect(),
                })),
                false,
            );

            self.invalidate_narrowing_aliases_for(place);
            // a `once` block runs right here, so an unconditional rebind replaces
            // the enclosing binding; a conditional rebind — or any write in a
            // non-`once` block — is kept alongside the prior value, a union that
            // matches a real `nonlocal` write
            let (previous, future) = if definite {
                (
                    PreviousDefinitions::AreShadowed,
                    FutureDefinitions::ShadowThisOne,
                )
            } else {
                (
                    PreviousDefinitions::AreKept,
                    FutureDefinitions::DontShadowThisOne,
                )
            };
            self.current_use_def_map_mut()
                .record_binding(place, definition, previous, future);
        }
    }

    /// Records assignment-expression bindings from a comprehension in its containing scope.
    ///
    /// The value expression still belongs to the comprehension scope, so the real definition
    /// stays there. The synthetic definition lets the containing scope observe that binding while
    /// retaining the comprehension's scope for type inference.
    ///
    /// ```python
    /// [(last := item) for item in items]
    /// print(last)  # `last` is owned by this containing scope.
    /// ```
    fn synthesize_comprehension_binding_definitions(
        &mut self,
        nested_bindings: NestedGlobalOrNonlocalDeclarations,
    ) {
        let mut nested_bindings = nested_bindings.into_iter().collect::<Vec<_>>();
        nested_bindings.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));

        for (name, mut declarations) in nested_bindings {
            // Ignore declarations used only to validate `nonlocal` syntax.
            declarations.retain(|d| d.is_bound);
            declarations.shrink_to_fit();
            let Some(first_declaration) = declarations.first().copied() else {
                continue;
            };

            let binding_status = self.comprehension_binding_status(&name, &declarations);

            let symbol = self.add_symbol(name.clone());
            debug_assert!(
                declarations
                    .iter()
                    .all(|declaration| declaration.is_global() == first_declaration.is_global())
            );
            self.forward_comprehension_binding(&name, first_declaration, symbol);

            let place: ScopedPlaceId = symbol.into();
            if binding_status == LiveBindingStatus::Unbound {
                self.mark_place_bound(place);
                continue;
            }

            let definition = Definition::new(
                self.db,
                self.current_scope_id(),
                place,
                DefinitionKind::NestedBindings(Box::new(NestedBindingsDefinitionKind {
                    name,
                    execution: NestedBindingExecution::Eager,
                    nested_declarations: declarations,
                })),
                false,
            );
            let previous = if binding_status == LiveBindingStatus::Bound {
                PreviousDefinitions::AreShadowed
            } else {
                PreviousDefinitions::AreKept
            };
            self.record_definition(place, definition, Some(previous));
        }
    }

    /// Summarizes whether the comprehension's live exit paths bind `name`.
    ///
    /// For example, `value` is only possibly bound after this comprehension because the walrus is
    /// skipped when `flag` is false:
    ///
    /// ```python
    /// [(value := item) if flag else None for item in items]
    /// ```
    fn comprehension_binding_status(
        &mut self,
        name: &str,
        declarations: &[NestedDeclaration],
    ) -> LiveBindingStatus {
        let mut status = LiveBindingStatus::Unbound;
        for declaration in declarations {
            let scope_id = declaration.file_scope_id;
            let Some(symbol) = self.place_tables[scope_id].symbol_id(name) else {
                continue;
            };
            match self.use_def_maps[scope_id].symbol_live_binding_status(symbol) {
                LiveBindingStatus::Bound => return LiveBindingStatus::Bound,
                LiveBindingStatus::PossiblyBound => status = LiveBindingStatus::PossiblyBound,
                LiveBindingStatus::Unbound => {}
            }
        }
        status
    }

    /// Passes a walrus binding out through nested comprehensions.
    ///
    /// ```python
    /// [[(last := item) for item in row] for row in rows]
    /// print(last)  # `last` belongs to the scope outside both comprehensions.
    /// ```
    ///
    /// Each comprehension passes the binding out one level. This preserves the order and
    /// conditions under which the assignment is evaluated.
    fn forward_comprehension_binding(
        &mut self,
        name: &Name,
        first_declaration: NestedDeclaration,
        symbol: ScopedSymbolId,
    ) {
        if self.scopes[self.current_scope()].kind() != ScopeKind::Comprehension {
            return;
        }

        self.current_scope_info_mut()
            .nested_global_or_nonlocal_declarations
            .remove(name);

        if first_declaration.is_global() {
            self.current_place_table_mut()
                .symbol_mut(symbol)
                .mark_global();
        } else {
            self.current_place_table_mut()
                .symbol_mut(symbol)
                .mark_nonlocal();
        }
        self.current_scope_info_mut()
            .this_scope_global_or_nonlocal_declarations
            .entry(name.clone())
            .or_insert(first_declaration.range);
    }

    /// Marks a comprehension walrus target as a write to the containing Python scope.
    ///
    /// The iteration variable remains local to the comprehension, while the walrus target does
    /// not:
    ///
    /// ```python
    /// [(result := item) for item in items]
    /// print(result)  # valid
    /// print(item)    # `item` is not defined here
    /// ```
    fn mark_comprehension_named_target(&mut self, place: ScopedPlaceId, range: TextRange) {
        if self.scopes[self.current_scope()].kind() != ScopeKind::Comprehension {
            return;
        }
        if self.semantic_syntax_errors.borrow().iter().any(|error| {
            matches!(
                error.kind,
                SemanticSyntaxErrorKind::ReboundComprehensionVariable
                    | SemanticSyntaxErrorKind::NamedExpressionInComprehensionIterable
            ) && error.range.contains_range(range)
        }) {
            return;
        }

        let Some(symbol) = place.as_symbol() else {
            return;
        };
        let name = self.current_place_table().symbol(symbol).name().clone();
        let Some(containing_scope) = self.scope_stack.iter().rev().find(|scope_info| {
            self.scopes[scope_info.file_scope_id].kind() != ScopeKind::Comprehension
        }) else {
            return;
        };

        let containing_scope_id = containing_scope.file_scope_id;
        let is_global = match self.scopes[containing_scope_id].kind() {
            ScopeKind::Module => true,
            ScopeKind::Function | ScopeKind::Lambda => self.place_tables[containing_scope_id]
                .symbol_id(&name)
                .is_some_and(|symbol| {
                    self.place_tables[containing_scope_id]
                        .symbol(symbol)
                        .is_global()
                }),
            // Assignment expressions are invalid in comprehensions directly contained by these
            // scopes. Leave the recovered target local to the comprehension.
            ScopeKind::Class | ScopeKind::TypeAlias | ScopeKind::TypeParams => return,
            ScopeKind::Comprehension => return,
        };

        if is_global {
            self.current_place_table_mut()
                .symbol_mut(symbol)
                .mark_global();
        } else {
            let (containing_symbol, added) =
                self.place_tables[containing_scope_id].add_symbol(Symbol::new(name.clone()));
            if added {
                self.use_def_maps[containing_scope_id].add_place(containing_symbol.into());
            }

            let containing_symbol =
                self.place_tables[containing_scope_id].symbol_mut(containing_symbol);
            if !containing_symbol.is_nonlocal() && !containing_symbol.is_bound() {
                containing_symbol.mark_bound();
            }

            self.current_place_table_mut()
                .symbol_mut(symbol)
                .mark_nonlocal();
        }
        self.current_scope_info_mut()
            .this_scope_global_or_nonlocal_declarations
            .insert(name, range);
    }

    fn record_expression_narrowing_constraint(
        &mut self,
        predicate_node: &'ast ast::Expr,
    ) -> (PredicateOrLiteral<'db>, ScopedPredicateId) {
        let predicate = self.build_predicate(predicate_node, ExpressionContext::Condition);
        let predicate_id = self.record_narrowing_constraint(predicate);
        (predicate, predicate_id)
    }

    fn build_predicate(
        &mut self,
        predicate_node: &'ast ast::Expr,
        context: ExpressionContext,
    ) -> PredicateOrLiteral<'db> {
        // Some commonly used test expressions are eagerly evaluated as `true`
        // or `false` here for performance reasons. This list does not need to
        // be exhaustive. More complex expressions will still evaluate to the
        // correct value during type-checking.
        fn resolve_to_literal(node: &ast::Expr) -> Option<bool> {
            match node {
                ast::Expr::BooleanLiteral(ast::ExprBooleanLiteral { value, .. }) => Some(*value),
                node if is_if_type_checking(node) => Some(true),
                ast::Expr::NumberLiteral(ast::ExprNumberLiteral {
                    value: ast::Number::Int(n),
                    ..
                }) => Some(*n != 0),
                ast::Expr::EllipsisLiteral(_) => Some(true),
                ast::Expr::Lambda(_) | ast::Expr::Generator(_) => Some(true),
                ast::Expr::NoneLiteral(_) => Some(false),
                ast::Expr::UnaryOp(ast::ExprUnaryOp {
                    op: ast::UnaryOp::Not,
                    operand,
                    ..
                }) => Some(!resolve_to_literal(operand)?),
                _ => None,
            }
        }

        self.register_narrowing_alias_predicates(predicate_node);

        let expression = self.add_standalone_expression(predicate_node);

        match resolve_to_literal(predicate_node) {
            Some(literal) => PredicateOrLiteral::Literal(literal),
            None => PredicateOrLiteral::Predicate(Predicate {
                node: match (context, predicate_node) {
                    (
                        ExpressionContext::Condition,
                        ast::Expr::BoolOp(_)
                        | ast::Expr::If(_)
                        | ast::Expr::UnaryOp(ast::ExprUnaryOp {
                            op: ast::UnaryOp::Not,
                            ..
                        }),
                    ) => PredicateNode::Condition(expression),
                    (ExpressionContext::Condition, ast::Expr::Compare(compare))
                        if compare.ops.len() > 1 =>
                    {
                        PredicateNode::ChainedComparisonCondition(expression)
                    }
                    _ => PredicateNode::Expression(expression),
                },
                is_positive: true,
            }),
        }
    }

    /// Adds a new predicate to the list of all predicates, but does not record it. Returns the
    /// predicate ID for later recording using
    /// [`SemanticIndexBuilder::record_narrowing_constraint_id_for_places`].
    fn add_predicate(&mut self, predicate: PredicateOrLiteral<'db>) -> ScopedPredicateId {
        self.current_use_def_map_mut().add_predicate(predicate)
    }

    /// Negates a predicate and adds it to the list of all predicates, does not record it.
    fn add_negated_predicate(&mut self, predicate: PredicateOrLiteral<'db>) -> ScopedPredicateId {
        self.current_use_def_map_mut()
            .add_predicate(predicate.negated())
    }

    /// Records a previously added narrowing constraint by adding it to the live bindings
    /// of the specified places.
    fn record_narrowing_constraint_id_for_places(
        &mut self,
        predicate: ScopedPredicateId,
        places: &PossiblyNarrowedPlaces,
    ) {
        self.current_use_def_map_mut()
            .record_narrowing_constraint_for_places(predicate, places);
    }

    /// basedpython: the places this file's narrowing return annotations name.
    ///
    /// `def f() -> a is int` narrows a place no call site mentions, and
    /// `def m(self) -> asserts self.data` narrows a member of one, so neither target can be
    /// read off a call's arguments. Resolving them needs the callee's signature, which isn't
    /// available while the semantic index is being built, so every place and every member
    /// chain a guard in this file names is a candidate at every call. A guard declared in
    /// another file names a place in *that* module, which is a different symbol from a
    /// same-named place here, so it is not a candidate.
    fn basedpython_guard_targets(&mut self) -> &GuardTargets {
        if self.basedpython_guard_targets.is_none() {
            let mut targets = GuardTargets::default();
            if self.source_type.is_basedpython() {
                walk_body(
                    &mut GuardTargetCollector {
                        targets: &mut targets,
                    },
                    &self.module.syntax().body,
                );
                targets.scope_places.sort_unstable();
                targets.scope_places.dedup();
                targets.member_chains.sort_unstable();
                targets.member_chains.dedup();
            }
            self.basedpython_guard_targets = Some(targets);
        }
        self.basedpython_guard_targets
            .as_ref()
            .expect("guard targets were just computed")
    }

    /// Register the file's basedpython guard targets in the current scope so a call predicate
    /// can narrow them, and return their places.
    fn possible_guard_target_places(
        &mut self,
        predicate: &PredicateOrLiteral<'db>,
    ) -> Vec<ScopedPlaceId> {
        let PredicateOrLiteral::Predicate(predicate) = predicate else {
            return Vec::new();
        };
        let (PredicateNode::Expression(expression)
        | PredicateNode::AssertsCall(CallableAndCallExpr {
            call_expr: expression,
            ..
        })) = predicate.node
        else {
            return Vec::new();
        };
        let node = expression.node_ref(self.db).node(self.module);
        let mut collector = CallCollector { calls: Vec::new() };
        collector.visit_expr(node);
        if collector.calls.is_empty() {
            return Vec::new();
        }
        let called_names: Vec<&Name> = collector
            .calls
            .iter()
            .filter_map(|call| called_name(call))
            .collect();

        let (scope_targets, member_chains) = {
            let targets = self.basedpython_guard_targets();
            if targets.is_empty() {
                return Vec::new();
            }
            let mut chains = targets.member_chains.clone();
            for name in called_names {
                if let Some(recovered) = targets.recovered_member_chains.get(name) {
                    chains.extend(recovered.iter().cloned());
                }
            }
            (targets.scope_places.clone(), chains)
        };

        // a guard on a parameter or a receiver narrows a member of whatever the call passes
        // there, so every root the call mentions is paired with every member chain
        let member_places: Vec<PlaceExpr> = collector
            .calls
            .iter()
            .flat_map(|call| call_roots(call))
            .flat_map(|root| {
                member_chains
                    .iter()
                    .filter_map(move |chain| PlaceExpr::try_from_expr_with_members(root, chain))
            })
            .collect();
        let scope_places: Vec<PlaceExpr> = scope_targets
            .iter()
            .filter_map(|(name, members)| PlaceExpr::from_symbol_with_members(name, members))
            .collect();

        scope_places
            .into_iter()
            .chain(member_places)
            .map(|place| self.add_place(place))
            .collect()
    }

    /// Adds and records a narrowing constraint for only the places that could possibly be narrowed.
    ///
    /// Returns the `ScopedPredicateId` for the positive predicate, which can later be passed to
    /// `record_negated_narrowing_constraint` to record the opposite result of the same check.
    fn record_narrowing_constraint(
        &mut self,
        predicate: PredicateOrLiteral<'db>,
    ) -> ScopedPredicateId {
        let guard_targets = self.possible_guard_target_places(&predicate);
        let mut possibly_narrowed = self.compute_possibly_narrowed_places(&predicate);
        possibly_narrowed.extend(guard_targets);
        let use_def = self.current_use_def_map_mut();
        let predicate_id = use_def.add_predicate(predicate);
        use_def.record_narrowing_constraint_for_places(predicate_id, &possibly_narrowed);
        predicate_id
    }

    /// Computes the conservative set of places that could possibly be narrowed by a predicate.
    ///
    /// This uses the closure-based approach to avoid calling Salsa queries that depend on
    /// the semantic index (which is still being built).
    fn compute_possibly_narrowed_places(
        &self,
        predicate: &PredicateOrLiteral<'db>,
    ) -> PossiblyNarrowedPlaces {
        match predicate {
            PredicateOrLiteral::Literal(_) => PossiblyNarrowedPlaces::default(),
            PredicateOrLiteral::Predicate(pred) => {
                let place_table = self.current_place_table();

                match pred.node {
                    PredicateNode::Expression(expression)
                    | PredicateNode::Condition(expression)
                    | PredicateNode::ChainedComparisonCondition(expression) => {
                        let expression_node = expression.node_ref(self.db).node(self.module);
                        let mut places = PossiblyNarrowedPlacesBuilder::new(self.db, place_table)
                            .expression(expression_node);
                        self.add_alias_narrowed_places(expression_node, &mut places);
                        places
                    }
                    PredicateNode::Pattern(pattern) => {
                        let module = self.module;
                        PossiblyNarrowedPlacesBuilder::new(self.db, place_table)
                            .pattern(pattern, module)
                    }
                    // basedpython: an assertion guard narrows the argument it was passed
                    PredicateNode::AssertsCall(CallableAndCallExpr { call_expr, .. }) => {
                        match asserted_call(call_expr.node_ref(self.db).node(self.module)) {
                            Some(call) => {
                                PossiblyNarrowedPlacesBuilder::new(self.db, place_table).call(call)
                            }
                            None => PossiblyNarrowedPlaces::default(),
                        }
                    }
                    PredicateNode::SubjectElementPattern(_)
                    | PredicateNode::IsNonTerminalCall(_)
                    | PredicateNode::ContextManagerSuppresses { .. }
                    | PredicateNode::FinallyNormalPathImpossible { .. }
                    | PredicateNode::IsNonEmptyIterable(_)
                    | PredicateNode::OrPatternAlternative(_)
                    | PredicateNode::StarImportPlaceholder(_)
                    | PredicateNode::CaseNameCapture(_) => {
                        // These predicates don't narrow any places
                        PossiblyNarrowedPlaces::default()
                    }
                }
            }
        }
    }

    /// Negates the given predicate and then adds it as a narrowing constraint to the places
    /// that could possibly be narrowed.
    ///
    /// Takes the `ScopedPredicateId` from the positive recording so that both constraints refer to
    /// the same check. This lets the positive and negative cases cancel after a complete
    /// `if`/`else`.
    fn record_negated_narrowing_constraint(
        &mut self,
        predicate: PredicateOrLiteral<'db>,
        predicate_id: ScopedPredicateId,
    ) {
        let guard_targets = self.possible_guard_target_places(&predicate);
        let mut possibly_narrowed = self.compute_possibly_narrowed_places(&predicate);
        possibly_narrowed.extend(guard_targets);
        self.current_use_def_map_mut()
            .record_negated_narrowing_constraint_for_places(predicate_id, &possibly_narrowed);
    }

    /// Records that all remaining statements in the current block are unreachable.
    fn mark_unreachable(&mut self) {
        self.current_use_def_map_mut().mark_unreachable();
    }

    /// Whether — and under which conditions — the current point in the flow is
    /// reached at all.
    fn current_reachability(&self) -> ScopedReachabilityConstraintId {
        self.current_use_def_map().reachability
    }

    /// Records that the current state can enter any active `finally` suites before the current
    /// terminal control-flow transfer reaches its destination.
    fn record_terminal_finally_entry(&mut self) {
        let mut exception_context_stack_manager =
            std::mem::take(&mut self.exception_context_stack_manager);
        exception_context_stack_manager.record_terminal_finally_entry(self);
        self.exception_context_stack_manager = exception_context_stack_manager;
    }

    /// Returns whether an exception raised while evaluating `scope` can propagate directly to its
    /// enclosing scope.
    ///
    /// Generator expressions follow the eager comprehension-scope convention used throughout our
    /// flow model. Although generators are lazy at runtime, their bodies are assumed to execute
    /// immediately, since in practice they are almost always eagerly iterated over.
    ///
    /// ```python
    /// try:
    ///     (may_raise() for _ in [0])
    /// except Exception:
    ///     ...
    /// ```
    fn exception_checkpoint_crosses_scope_boundary(&self, scope_id: FileScopeId) -> bool {
        self.scopes[scope_id].is_eager()
    }

    /// Records the current flow state immediately before an operation that may raise an exception.
    ///
    /// This models exceptions from ordinary operations, not every possible interruption. In
    /// particular, we do not add arbitrary exception points for asynchronously raised exceptions
    /// such as those originating in signal handlers.
    ///
    /// Child expressions must already have been visited, so their completed assignments are
    /// visible if the parent operation fails:
    ///
    /// ```python
    /// state = 0
    /// try:
    ///     may_raise(state := 1)
    /// except Exception:
    ///     reveal_type(state)  # Literal[1]
    /// ```
    ///
    /// Skips snapshot construction when no enclosing `try` or `with` context can handle exceptions.
    fn record_exception_checkpoint(&mut self) {
        if !self
            .exception_context_stack_manager
            .has_active_exception_handler(self)
        {
            return;
        }

        let mut exception_context_stack_manager =
            std::mem::take(&mut self.exception_context_stack_manager);
        exception_context_stack_manager.record_exception_checkpoint(self);
        self.exception_context_stack_manager = exception_context_stack_manager;
    }

    fn record_exception_checkpoint_if(&mut self, can_raise: bool) {
        if can_raise {
            self.record_exception_checkpoint();
        }
    }

    /// Returns whether accessing a name, attribute, or subscript can raise.
    ///
    /// Only a definitely bound name in the current flow state is known to be safe. In particular,
    /// a builtin-looking name may be shadowed by a local binding that has not been visited yet.
    fn place_access_can_raise(&mut self, expr: &ast::Expr, is_use: bool) -> bool {
        let ast::Expr::Name(name) = expr else {
            return true;
        };

        is_use
            && self
                .exception_context_stack_manager
                .has_active_exception_handler(self)
            && self
                .current_place_table()
                .symbol_id(name.id.as_str())
                .is_none_or(|symbol| {
                    self.current_use_def_map_mut()
                        .symbol_live_binding_status(symbol)
                        != LiveBindingStatus::Bound
                })
    }

    /// Returns whether evaluating and truth-testing `expr` cannot invoke Python user code.
    ///
    /// Identity comparisons are safe, but testing an arbitrary value may call `__bool__`:
    ///
    /// ```python
    /// if value is None: ...  # safe
    /// if value: ...  # can raise
    /// ```
    fn condition_evaluation_is_known_safe(expr: &ast::Expr) -> bool {
        if expr.is_literal_expr() || matches!(expr, ast::Expr::Lambda(_)) {
            return true;
        }

        match expr {
            ast::Expr::Named(named) if named.target.is_name_expr() => {
                Self::condition_evaluation_is_known_safe(&named.value)
            }
            ast::Expr::List(_) | ast::Expr::Tuple(_) => {
                Self::expression_evaluation_is_known_safe(expr)
            }
            ast::Expr::BoolOp(ast::ExprBoolOp { values, .. }) => {
                values.iter().all(Self::condition_evaluation_is_known_safe)
            }
            ast::Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::Not,
                operand,
                ..
            }) => Self::condition_evaluation_is_known_safe(operand),
            ast::Expr::Compare(ast::ExprCompare {
                left,
                ops,
                comparators,
                ..
            }) => {
                ops.iter()
                    .all(|op| matches!(op, ast::CmpOp::Is | ast::CmpOp::IsNot))
                    && Self::expression_evaluation_is_known_safe(left)
                    && comparators
                        .iter()
                        .all(Self::expression_evaluation_is_known_safe)
            }
            _ => false,
        }
    }

    /// Returns whether evaluating `expr` cannot invoke Python user code.
    ///
    /// Unlike [`Self::condition_evaluation_is_known_safe`], this does not truth-test the resulting
    /// value, so loading a name is safe even when that value's `__bool__` method could raise.
    fn expression_evaluation_is_known_safe(expr: &ast::Expr) -> bool {
        if expr.is_literal_expr() || matches!(expr, ast::Expr::Name(_) | ast::Expr::Lambda(_)) {
            return true;
        }

        match expr {
            ast::Expr::Named(named) if named.target.is_name_expr() => {
                Self::expression_evaluation_is_known_safe(&named.value)
            }
            ast::Expr::List(ast::ExprList { elts, .. })
            | ast::Expr::Tuple(ast::ExprTuple { elts, .. }) => {
                elts.iter().all(Self::expression_evaluation_is_known_safe)
            }
            ast::Expr::Compare(_) => Self::condition_evaluation_is_known_safe(expr),
            _ => false,
        }
    }

    /// Returns whether iterating `expr` uses an exact builtin iterator that cannot raise anything
    /// other than `StopIteration` (ignoring ambient failures such as `MemoryError`).
    ///
    /// ```python
    /// for value in [1, 2]: ...  # safe
    /// for value in values: ...  # can invoke user-defined iteration
    /// ```
    fn iteration_is_known_safe(expr: &ast::Expr) -> bool {
        matches!(
            expr,
            ast::Expr::StringLiteral(_)
                | ast::Expr::BytesLiteral(_)
                | ast::Expr::List(_)
                | ast::Expr::Tuple(_)
        ) && Self::expression_evaluation_is_known_safe(expr)
    }

    /// Records a reachability constraint that always evaluates to "ambiguous".
    fn record_ambiguous_reachability(&mut self) {
        self.current_use_def_map_mut()
            .record_reachability_constraint(ScopedReachabilityConstraintId::AMBIGUOUS);
    }

    /// Record a constraint that affects the reachability of the current position in the semantic
    /// index analysis. For example, if we encounter a `if test:` branch, we immediately record
    /// a `test` constraint, because if `test` later (during type checking) evaluates to `False`,
    /// we know that all statements that follow in this path of control flow will be unreachable.
    fn record_reachability_constraint(
        &mut self,
        predicate: PredicateOrLiteral<'db>,
    ) -> ScopedReachabilityConstraintId {
        let predicate_id = self.add_predicate(predicate);
        self.record_reachability_constraint_id(predicate_id)
    }

    /// Similar to [`Self::record_reachability_constraint`], but takes a [`ScopedPredicateId`].
    fn record_reachability_constraint_id(
        &mut self,
        predicate_id: ScopedPredicateId,
    ) -> ScopedReachabilityConstraintId {
        let reachability_constraint = self
            .current_reachability_constraints_mut()
            .add_atom(predicate_id);

        self.current_use_def_map_mut()
            .record_reachability_constraint(reachability_constraint);
        reachability_constraint
    }

    /// Record the negation of a given reachability constraint.
    fn record_negated_reachability_constraint(
        &mut self,
        reachability_constraint: ScopedReachabilityConstraintId,
    ) {
        let negated_constraint = self
            .current_reachability_constraints_mut()
            .add_not_constraint(reachability_constraint);
        self.current_use_def_map_mut()
            .record_reachability_constraint(negated_constraint);
    }

    fn push_assignment(&mut self, assignment: CurrentAssignment<'ast, 'db>) {
        self.current_assignments.push(assignment);
    }

    fn pop_assignment(&mut self) {
        let popped_assignment = self.current_assignments.pop();
        debug_assert!(popped_assignment.is_some());
    }

    fn current_assignment(&self) -> Option<CurrentAssignment<'ast, 'db>> {
        self.current_assignments.last().copied()
    }

    fn current_assignment_mut(&mut self) -> Option<&mut CurrentAssignment<'ast, 'db>> {
        self.current_assignments.last_mut()
    }

    fn push_statement(&mut self, statement: CurrentStatement<'ast, 'db>) {
        self.current_statements.push(statement);
    }

    fn pop_statement(&mut self) -> CurrentStatement<'ast, 'db> {
        self.current_statements.pop().unwrap()
    }

    fn current_statement_mut(&mut self) -> Option<&mut CurrentStatement<'ast, 'db>> {
        self.current_statements.last_mut()
    }

    /// Return whether a pattern contains any capture that changes the current flow state.
    fn pattern_has_bindings(pattern: &ast::Pattern) -> bool {
        match pattern {
            ast::Pattern::MatchValue(_) | ast::Pattern::MatchSingleton(_) => false,
            ast::Pattern::MatchSequence(ast::PatternMatchSequence { patterns, .. })
            | ast::Pattern::MatchOr(ast::PatternMatchOr { patterns, .. })
            | ast::Pattern::MatchAnd(ast::PatternMatchAnd { patterns, .. }) => {
                patterns.iter().any(Self::pattern_has_bindings)
            }
            ast::Pattern::MatchMapping(pattern) => {
                pattern.rest.is_some() || pattern.patterns.iter().any(Self::pattern_has_bindings)
            }
            ast::Pattern::MatchClass(pattern) => pattern
                .arguments
                .patterns
                .iter()
                .chain(
                    pattern
                        .arguments
                        .keywords
                        .iter()
                        .map(|keyword| &keyword.pattern),
                )
                .any(Self::pattern_has_bindings),
            ast::Pattern::MatchStar(pattern) => pattern.name.is_some(),
            ast::Pattern::MatchAs(pattern) => {
                pattern.name.is_some()
                    || pattern
                        .pattern
                        .as_deref()
                        .is_some_and(Self::pattern_has_bindings)
            }
        }
    }

    /// Returns whether matching a pattern can invoke Python user code.
    fn pattern_can_raise(pattern: &ast::Pattern) -> bool {
        match pattern {
            ast::Pattern::MatchValue(_)
            | ast::Pattern::MatchSequence(_)
            | ast::Pattern::MatchMapping(_)
            | ast::Pattern::MatchClass(_) => true,
            ast::Pattern::MatchSingleton(_) | ast::Pattern::MatchStar(_) => false,
            ast::Pattern::MatchAs(pattern) => pattern
                .pattern
                .as_deref()
                .is_some_and(Self::pattern_can_raise),
            ast::Pattern::MatchOr(pattern) => pattern.patterns.iter().any(Self::pattern_can_raise),
            // basedpython `case P and Q:` matches every sub-pattern against the same subject
            ast::Pattern::MatchAnd(pattern) => pattern.patterns.iter().any(Self::pattern_can_raise),
        }
    }

    /// The pattern structure type checking needs, and the bare `case A:` names
    /// [context-sensitive resolution](CaseNamePredicateKind) is offered.
    ///
    /// `against_subject` says whether this node is matched against the case's
    /// subject rather than against some part of it: only there does a bare name
    /// stand a chance of being an enum member, and only while it holds does a
    /// [`PatternPredicateKind::CaseName`] get built. Collecting the names here
    /// rather than in a second walk is what keeps the predicate and the bindings
    /// [`Self::visit_pattern`] records from ever disagreeing about which names
    /// those are.
    fn predicate_kind(
        &mut self,
        pattern: &'ast ast::Pattern,
        subject: PatternSubject<'db>,
        against_subject: bool,
        case_names: &mut CaseNames<'ast, 'db>,
    ) -> PatternPredicateKind<'db> {
        // a nested pattern matches a part of the subject, which the name has no
        // expected type for
        let nested = |this: &mut Self, pattern, case_names: &mut _| {
            this.predicate_kind(pattern, subject, false, case_names)
        };
        match pattern {
            ast::Pattern::MatchValue(pattern) => {
                let value = self.add_standalone_expression(&pattern.value);
                PatternPredicateKind::Value(value)
            }
            ast::Pattern::MatchSingleton(singleton) => {
                PatternPredicateKind::Singleton(singleton.value)
            }
            ast::Pattern::MatchClass(pattern) => {
                let cls = self.add_standalone_expression(&pattern.cls);
                // basedpython: `case Circle(r):` names a variant of the subject
                // the same way `case Empty:` names one, so it is offered to
                // context-sensitive resolution too. Unlike a bare name it is
                // already an expression, so the resolution rides on the ordinary
                // name lookup and nothing about the pattern's shape changes
                if against_subject && let ast::Expr::Name(name) = &*pattern.cls {
                    self.case_names.insert(
                        NodeKey::from_node(name),
                        CaseNamePredicateKind {
                            name: name.id.clone(),
                            scope: self.current_scope_id(),
                            subject,
                        },
                    );
                }

                // basedpython `case A(x, *_, y)`: the starred wildcard stands
                // for the positions nobody asked about, so it is not a
                // subpattern of its own — what it contributes is that `y` is
                // read from the end of `__match_args__` instead of position 1
                let positional_from_end = pattern
                    .arguments
                    .patterns
                    .iter()
                    .position(ast::Pattern::is_match_star)
                    .map_or(0, |star| {
                        pattern.arguments.patterns[star + 1..]
                            .iter()
                            .filter(|pattern| !pattern.is_match_star())
                            .count()
                    });

                PatternPredicateKind::Class(ClassPatternPredicateKind {
                    class: cls,
                    positional: pattern
                        .arguments
                        .patterns
                        .iter()
                        .filter(|pattern| !pattern.is_match_star())
                        .map(|pattern| nested(self, pattern, case_names))
                        .collect(),
                    positional_from_end,
                    keywords: pattern
                        .arguments
                        .keywords
                        .iter()
                        .map(|keyword| ClassPatternKeywordPredicateKind {
                            attr: keyword.attr.id.clone(),
                            pattern: nested(self, &keyword.pattern, case_names),
                        })
                        .collect(),
                })
            }
            ast::Pattern::MatchMapping(pattern) => {
                // Retain keyed entries for subject-aware exhaustiveness analysis.
                PatternPredicateKind::Mapping(MappingPatternPredicateKind {
                    entries: pattern
                        .keys
                        .iter()
                        .zip(&pattern.patterns)
                        .map(|(key, pattern)| MappingPatternEntryPredicateKind {
                            key: self.add_standalone_expression(key),
                            pattern: nested(self, pattern, case_names),
                        })
                        .collect(),
                    rest: pattern.rest.as_ref().map(|name| name.id.clone()),
                })
            }
            ast::Pattern::MatchSequence(pattern) => {
                PatternPredicateKind::Sequence(SequencePatternPredicateKind {
                    patterns: pattern
                        .patterns
                        .iter()
                        .map(|pattern| nested(self, pattern, case_names))
                        .collect(),
                })
            }
            // an alternative, a conjunct and an `as` pattern's left-hand side are
            // each matched against whatever the pattern as a whole is
            ast::Pattern::MatchOr(pattern) => {
                let predicates = pattern
                    .patterns
                    .iter()
                    .map(|p| self.predicate_kind(p, subject, against_subject, case_names))
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                PatternPredicateKind::Or(predicates)
            }
            ast::Pattern::MatchAnd(pattern) => {
                let predicates = pattern
                    .patterns
                    .iter()
                    .map(|p| self.predicate_kind(p, subject, against_subject, case_names))
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                PatternPredicateKind::And(predicates)
            }
            ast::Pattern::MatchAs(ast::PatternMatchAs {
                pattern: None,
                name: Some(name),
                ..
            }) if against_subject => {
                let kind = CaseNamePredicateKind {
                    name: name.id.clone(),
                    scope: self.current_scope_id(),
                    subject,
                };
                self.case_names
                    .insert(NodeKey::from_node(name), kind.clone());
                case_names.push((name, kind.clone()));
                PatternPredicateKind::CaseName(kind)
            }
            ast::Pattern::MatchAs(pattern) => PatternPredicateKind::As(
                pattern.pattern.as_ref().map(|p| {
                    Box::new(self.predicate_kind(p, subject, against_subject, case_names))
                }),
                pattern.name.as_ref().map(|name| name.id.clone()),
            ),
            ast::Pattern::MatchStar(pattern) => {
                PatternPredicateKind::Star(pattern.name.as_ref().map(|name| name.id.clone()))
            }
        }
    }

    /// Collects the places a pattern match against `subject` can narrow, together
    /// with the bindings each place is read from. A subject is evaluated once, so
    /// retaining those bindings keeps a pattern predicate constraining the value
    /// that was matched rather than a later rebinding.
    ///
    /// The second element covers the elements of a list/tuple subject display,
    /// which a sequence pattern narrows individually.
    fn match_subject_targets(&mut self, subject: &'ast ast::Expr) -> MatchSubjectTargets {
        let subject_places = match_subject_place_expressions(subject)
            .into_iter()
            .filter_map(|expression| {
                let place = PlaceExpr::try_from_expr(expression)
                    .and_then(|place| self.current_place_table().place_id((&place).into()))?;
                Some((place, self.current_ast_ids().try_use_id(expression)))
            })
            .collect::<SmallVec<[_; 2]>>();
        let mut subject_targets =
            SmallVec::<[(ScopedPlaceId, SmallVec<[ScopedDefinitionId; 2]>); 2]>::new();
        for &(place, use_id) in &subject_places {
            let bindings = if let Some(use_id) = use_id {
                self.current_use_def_map()
                    .bindings_at_use(use_id)
                    .map(LiveBinding::binding)
                    .collect()
            } else {
                // A named-expression subject creates its target binding instead of reading
                // one, so snapshot the binding that was just created.
                self.current_use_def_map_mut()
                    .current_bindings(place)
                    .map(|binding| LiveBinding::binding(&binding))
                    .collect()
            };
            subject_targets.push((place, bindings));
        }

        let places = self.current_place_table();
        let ast_ids = self.current_ast_ids();
        let mut sequence_subject_targets =
            SmallVec::<[(ScopedPlaceId, ScopedUseId, ExpressionNodeKey); 2]>::new();
        let mut subject_elements: Vec<&ast::Expr> = match subject {
            ast::Expr::List(list) => list.elts.iter().collect(),
            ast::Expr::Tuple(tuple) => tuple.elts.iter().collect(),
            _ => Vec::new(),
        };
        while let Some(element) = subject_elements.pop() {
            match element {
                ast::Expr::List(list) => subject_elements.extend(&list.elts),
                ast::Expr::Tuple(tuple) => subject_elements.extend(&tuple.elts),
                _ => {
                    let Some(target) = PlaceExpr::try_from_expr(element)
                        .and_then(|place| places.place_id((&place).into()))
                        .zip(ast_ids.try_use_id(element))
                    else {
                        continue;
                    };
                    sequence_subject_targets.push((
                        target.0,
                        target.1,
                        ExpressionNodeKey::from(element),
                    ));
                }
            }
        }

        (subject_targets, sequence_subject_targets)
    }

    /// Visits the condition of an `if` / `elif` clause and records its narrowing
    /// constraints, leaving the flow in the truthy branch. Returns the falsy
    /// snapshot and the clause's predicate.
    ///
    /// With a pattern the clause is a basedpython `if let <pattern> := <subject>:`
    /// and records exactly what a single `match` case against `subject` records:
    /// the captures are bound in the enclosing scope and the subject is narrowed
    /// by the pattern (negated on the way into the following clauses).
    fn visit_if_condition(
        &mut self,
        pattern: Option<&'ast ast::Pattern>,
        test: &'ast ast::Expr,
    ) -> (FlowSnapshot, PredicateOrLiteral<'db>, ScopedPredicateId) {
        let pattern = pattern.map(|pattern| (pattern, self.add_standalone_expression(test)));

        if pattern.is_some() {
            // basedpython `if let P := subject`: the pattern is matched against the
            // subject's result object, so the subject is evaluated for its value
            self.visit_expr(test);
        } else {
            self.visit_expr_with_context(test, ExpressionContext::Condition);
        }

        let pattern =
            pattern.map(|(pattern, subject)| (pattern, subject, self.match_subject_targets(test)));

        // A condition is evaluated whether or not its branch is taken.
        let condition_flow_snapshot = self.flow_snapshot_for_condition(test);
        let falsy = if let Some(snapshots) = condition_flow_snapshot.into_branches() {
            self.flow_restore(snapshots.truthy);
            snapshots.falsy
        } else {
            self.flow_snapshot()
        };

        let (predicate, narrowing_id) = if let Some((
            pattern,
            subject,
            (subject_targets, sequence_subject_targets),
        )) = pattern
        {
            // `if let` is a test, so a bare name in it may be a value
            let (pattern_predicate, case_names) = self.create_pattern_predicate(
                PatternSubject::Expression(subject),
                pattern,
                None,
                None,
                true,
            );
            let outer_match_case = self.current_match_case.replace(CurrentMatchCase::new(
                pattern,
                pattern_predicate,
                case_names,
            ));
            self.visit_pattern(pattern);
            self.current_match_case = outer_match_case;
            // never a catchall, even for an irrefutable pattern. That shortcut
            // exists so an *exhaustive* `match` collapses `P1 OR (~P1 AND P2) OR
            // (~P1 AND ~P2)` back to the pre-match type; an `if` chain has no
            // such collapse to preserve, because the merge of its clause
            // snapshots (plus the synthesized no-op `else`) already restores the
            // subject's type after the statement
            self.add_pattern_narrowing_constraint(
                pattern_predicate,
                &subject_targets,
                &sequence_subject_targets,
                false,
            )
        } else {
            self.record_expression_narrowing_constraint(test)
        };

        (falsy, predicate, narrowing_id)
    }

    /// basedpython: records the captures a destructuring binder's pattern binds.
    ///
    /// The binder — a `for` target, a `with` item's target, a parameter — already
    /// holds the value, so the pattern is matched against the binder's own
    /// definition rather than against an expression. Everything a `match` case
    /// records is recorded here too, minus the branch: a binder's pattern has to
    /// be irrefutable, so its captures are bound unconditionally.
    fn add_destructure_definitions(
        &mut self,
        pattern: &'ast ast::Pattern,
        binder: &'ast ast::ExprName,
    ) {
        self.add_destructure_definitions_for(pattern, self.expect_single_definition(binder));
    }

    /// [`Self::add_destructure_definitions`] for a binder whose definition the
    /// caller already has.
    fn add_destructure_definitions_for(
        &mut self,
        pattern: &'ast ast::Pattern,
        binder: Definition<'db>,
    ) {
        let (predicate, case_names) = self.create_pattern_predicate(
            PatternSubject::Binder(binder),
            pattern,
            None,
            None,
            false,
        );
        self.record_destructure(pattern, predicate, None);
        let outer_match_case = self
            .current_match_case
            .replace(CurrentMatchCase::new(pattern, predicate, case_names));
        self.visit_pattern(pattern);
        self.current_match_case = outer_match_case;
    }

    /// Records what inference needs to check a destructuring binder's pattern.
    fn record_destructure(
        &mut self,
        pattern: &'ast ast::Pattern,
        predicate: PatternPredicate<'db>,
        after_orelse: Option<ScopedReachabilityConstraintId>,
    ) {
        self.destructures.insert(
            NodeKey::from_node(pattern),
            Destructure {
                predicate,
                after_orelse,
            },
        );
    }

    /// `names_may_be_values` says whether a bare name matched against the subject
    /// may be an enum member rather than a capture — true wherever basedpython
    /// lets the pattern fail, and false for the irrefutable binders, whose whole
    /// purpose is to bind.
    fn create_pattern_predicate(
        &mut self,
        subject: PatternSubject<'db>,
        pattern: &'ast ast::Pattern,
        guard: Option<&ast::Expr>,
        previous_pattern: Option<PatternPredicate<'db>>,
        names_may_be_values: bool,
    ) -> (PatternPredicate<'db>, CaseNames<'ast, 'db>) {
        // This is called for the top-level pattern of each match arm. We need to create a
        // standalone expression for each arm of a match statement, since they can introduce
        // constraints on the match subject. (Or more accurately, for the match arm's pattern,
        // since its the pattern that introduces any constraints, not the body.) Ideally, that
        // standalone expression would wrap the match arm's pattern as a whole. But a standalone
        // expression can currently only wrap an ast::Expr, which patterns are not. So, we need to
        // choose an Expr that can "stand in" for the pattern, which we can wrap in a standalone
        // expression.
        //
        // See the comment in TypeInferenceBuilder::infer_match_pattern for more details.

        let mut case_names = Vec::new();
        let against_subject = names_may_be_values && self.source_type.is_basedpython();
        let kind = self.predicate_kind(pattern, subject, against_subject, &mut case_names);
        let guard = guard.map(|guard| self.add_standalone_expression(guard));

        let predicate = PatternPredicate::new(
            self.db,
            self.file,
            self.current_scope(),
            subject,
            kind,
            guard,
            previous_pattern.map(Box::new),
        );
        (predicate, case_names)
    }

    fn add_pattern_narrowing_constraint(
        &mut self,
        pattern_predicate: PatternPredicate<'db>,
        subject_targets: &[(ScopedPlaceId, SmallVec<[ScopedDefinitionId; 2]>)],
        sequence_subject_targets: &[(ScopedPlaceId, ScopedUseId, ExpressionNodeKey)],
        is_catchall: bool,
    ) -> (PredicateOrLiteral<'db>, ScopedPredicateId) {
        let predicate = PredicateOrLiteral::Predicate(Predicate {
            node: PredicateNode::Pattern(pattern_predicate),
            is_positive: true,
        });

        // For the last catchall case (irrefutable wildcard without guard), we skip
        // recording the narrowing constraint from the pattern. The accumulated negated
        // constraints from earlier cases (~P1, ~P2, ...) are sufficient. This ensures
        // `P1 OR (~P1 AND P2) OR (~P1 AND ~P2)` simplifies to ALWAYS_TRUE, preserving
        // the original type after an exhaustive match. The reachability and pattern
        // predicates are still created normally for proper control flow tracking.
        let predicate_id = if is_catchall {
            ScopedPredicateId::ALWAYS_TRUE
        } else if subject_targets.is_empty() && sequence_subject_targets.is_empty() {
            self.record_narrowing_constraint(predicate)
        } else {
            let predicate_id = self.add_predicate(predicate);
            for (place, bindings) in subject_targets {
                self.current_use_def_map_mut()
                    .record_narrowing_constraint_for_bindings(predicate_id, *place, bindings);
            }
            for &(place, use_id, target) in sequence_subject_targets {
                let subject_element_id =
                    self.add_predicate(PredicateOrLiteral::Predicate(Predicate {
                        node: PredicateNode::SubjectElementPattern(
                            SubjectElementPatternPredicate {
                                pattern: pattern_predicate,
                                target,
                            },
                        ),
                        is_positive: true,
                    }));
                self.current_use_def_map_mut()
                    .record_narrowing_constraint_for_bindings_at_use(
                        subject_element_id,
                        place,
                        use_id,
                    );
            }
            predicate_id
        };
        (predicate, predicate_id)
    }

    /// Record an expression that needs to be a Salsa ingredient, because we need to infer its type
    /// standalone (type narrowing tests, RHS of an assignment.)
    fn add_standalone_expression(&mut self, expression_node: &ast::Expr) -> Expression<'db> {
        self.add_standalone_expression_impl(expression_node, ExpressionKind::Normal, None)
    }

    /// Record an expression that is immediately assigned to a target, and that needs to be a Salsa
    /// ingredient, because we need to infer its type standalone (type narrowing tests, RHS of an
    /// assignment.)
    fn add_standalone_assigned_expression(
        &mut self,
        expression_node: &ast::Expr,
        assigned_to: &ast::StmtAssign,
    ) -> Expression<'db> {
        self.add_standalone_expression_impl(
            expression_node,
            ExpressionKind::Normal,
            Some(assigned_to),
        )
    }

    /// Same as [`SemanticIndexBuilder::add_standalone_expression`], but marks the expression as a
    /// *type* expression, which makes sure that it will later be inferred as such.
    fn add_standalone_type_expression(&mut self, expression_node: &ast::Expr) -> Expression<'db> {
        self.add_standalone_expression_impl(expression_node, ExpressionKind::TypeExpression, None)
    }

    fn add_standalone_expression_impl(
        &mut self,
        expression_node: &ast::Expr,
        expression_kind: ExpressionKind,
        assigned_to: Option<&ast::StmtAssign>,
    ) -> Expression<'db> {
        let expression = Expression::new(
            self.db,
            self.current_scope_id(),
            AstNodeRef::new(self.module, expression_node),
            assigned_to.map(|assigned_to| AstNodeRef::new(self.module, assigned_to)),
            expression_kind,
        );
        self.expressions_by_node
            .insert(expression_node.into(), expression);
        expression
    }

    fn add_standalone_statement(&mut self, statement_node: &ast::Stmt) -> Statement<'db> {
        // Avoid allocating a salsa ingredient if the statement represents an existing
        // definition or standalone expression.
        let statement = match statement_node {
            ast::Stmt::FunctionDef(function) => Some(Statement::Definition(
                self.expect_single_definition(function),
            )),
            ast::Stmt::ClassDef(class) => {
                Some(Statement::Definition(self.expect_single_definition(class)))
            }
            ast::Stmt::Expr(ast::StmtExpr { value, .. }) => {
                Some(Statement::Expression(self.add_standalone_expression(value)))
            }
            ast::Stmt::Assign(assign) => {
                if let [ast::Expr::Name(name)] = &assign.targets[..] {
                    Some(Statement::Definition(self.expect_single_definition(name)))
                } else {
                    None
                }
            }
            ast::Stmt::AnnAssign(assign) if assign.target.is_name_expr() => {
                Some(Statement::Definition(self.expect_single_definition(assign)))
            }
            ast::Stmt::AugAssign(assign) if assign.target.is_name_expr() => {
                Some(Statement::Definition(self.expect_single_definition(assign)))
            }
            ast::Stmt::TypeAlias(alias) => {
                Some(Statement::Definition(self.expect_single_definition(alias)))
            }
            _ => None,
        };

        let statement = if let Some(statement) = statement {
            statement
        } else {
            Statement::Other(StatementInner::new(
                self.db,
                self.file,
                self.current_scope(),
                AstNodeRef::new(self.module, statement_node),
            ))
        };

        self.statements_by_node
            .insert(statement_node.into(), statement);

        statement
    }

    /// basedpython: records the `case` blocks of a match type alias in the alias's own scope.
    ///
    /// Each case is a separate branch off the state before the cases, so a name captured by
    /// one case's pattern is in scope for that case's body and nowhere else — referring to
    /// another case's capture is an unresolved reference, as it should be.
    fn visit_type_match_cases(&mut self, cases: &'ast [ast::MatchCase]) {
        if cases.is_empty() {
            return;
        }

        let before_cases = self.flow_snapshot();
        let mut post_case_snapshots = Vec::with_capacity(cases.len());

        for (index, case) in cases.iter().enumerate() {
            // the state is restored *between* cases, not after the last one — that one
            // becomes the state the others are merged into
            if index > 0 {
                self.flow_restore(before_cases.clone());
            }
            // whether a case matches is decided when the alias is applied, so from the index's
            // point of view every case is a branch that may or may not be taken
            self.record_ambiguous_reachability();
            self.add_type_match_captures(&case.pattern);
            for stmt in &case.body {
                // the parser rejects anything else; a malformed body simply contributes no
                // expressions rather than being recorded as a statement of the alias scope
                if let ast::Stmt::Expr(expr_stmt) = stmt {
                    self.visit_expr(&expr_stmt.value);
                }
            }
            post_case_snapshots.push(self.flow_snapshot());
        }

        for snapshot in post_case_snapshots {
            self.flow_merge(snapshot);
        }
    }

    /// basedpython: binds every name a match type's `case` pattern captures.
    fn add_type_match_captures(&mut self, pattern: &'ast ast::Pattern) {
        let capture = |builder: &mut Self, identifier: &'ast ast::Identifier, is_variadic| {
            let symbol = builder.add_symbol(identifier.id.clone());
            builder.add_definition(
                symbol.into(),
                TypeMatchCaptureDefinitionNodeRef {
                    identifier,
                    is_variadic,
                },
            );
        };

        match pattern {
            ast::Pattern::MatchAs(ast::PatternMatchAs {
                pattern: inner,
                name,
                ..
            }) => {
                if let Some(inner) = inner.as_deref() {
                    self.add_type_match_captures(inner);
                }
                if let Some(name) = name {
                    capture(self, name, false);
                }
            }
            ast::Pattern::MatchStar(ast::PatternMatchStar { name, .. }) => {
                if let Some(name) = name {
                    capture(self, name, true);
                }
            }
            ast::Pattern::MatchSequence(ast::PatternMatchSequence { patterns, .. })
            | ast::Pattern::MatchOr(ast::PatternMatchOr { patterns, .. })
            | ast::Pattern::MatchAnd(ast::PatternMatchAnd { patterns, .. }) => {
                for pattern in patterns {
                    self.add_type_match_captures(pattern);
                }
            }
            ast::Pattern::MatchValue(_) | ast::Pattern::MatchSingleton(_) => {}
            ast::Pattern::MatchMapping(_) | ast::Pattern::MatchClass(_) => {}
        }
    }

    fn with_type_params<T>(
        &mut self,
        with_scope: NodeWithScopeRef,
        type_params: Option<&'ast ast::TypeParams>,
        nested: impl FnOnce(&mut Self) -> T,
    ) -> T {
        if let Some(type_params) = type_params {
            self.push_scope(with_scope);

            for type_param in &type_params.type_params {
                let (name, lower_bound, bound, default) = match type_param {
                    ast::TypeParam::TypeVar(ast::TypeParamTypeVar {
                        range: _,
                        node_index: _,
                        name,
                        lower_bound,
                        bound,
                        is_type_mapping: _,
                        default,
                        variance: _,
                        is_reified: _,
                        is_some_hole: _,
                    }) => (name, lower_bound, bound, default),
                    // basedpython: `**Kwargs: int` bounds every field of a keyword-variadic pack
                    ast::TypeParam::ParamSpec(ast::TypeParamParamSpec {
                        name,
                        bound,
                        default,
                        ..
                    }) => (name, &None, bound, default),
                    // basedpython: `*Ts: int` bounds every element of the pack
                    ast::TypeParam::TypeVarTuple(ast::TypeParamTypeVarTuple {
                        name,
                        bound,
                        default,
                        ..
                    }) => (name, &None, bound, default),
                };
                self.scopes_by_expression
                    .record_expression(name, self.current_scope());
                let symbol = self.add_symbol(name.id.clone());
                // TODO create Definition for PEP 695 typevars
                // note that the "bound" on the typevar is a totally different thing than whether
                // or not a name is "bound" by a typevar declaration; the latter is always true.
                self.mark_place_bound(symbol.into());
                self.mark_place_declared(symbol.into());
                if let Some(lower_bound) = lower_bound {
                    self.visit_expr(lower_bound);
                }
                if let Some(bounds) = bound {
                    self.visit_expr(bounds);
                }
                if let Some(default) = default {
                    self.visit_expr(default);
                }
                match type_param {
                    ast::TypeParam::TypeVar(node) => self.add_definition(symbol.into(), node),
                    ast::TypeParam::ParamSpec(node) => self.add_definition(symbol.into(), node),
                    ast::TypeParam::TypeVarTuple(node) => self.add_definition(symbol.into(), node),
                };
            }
        }

        let nested_scope = nested(self);

        if type_params.is_some() {
            self.pop_scope();
        }

        nested_scope
    }

    /// This method does several things:
    /// - It pushes a new scope onto the stack for visiting
    ///   a list/dict/set comprehension or generator expression
    /// - Inside that scope, it visits a list of [`Comprehension`] nodes,
    ///   assumed to be the "generators" that compose a comprehension
    ///   (that is, the `for x in y` and `for y in z` parts of `x for x in y for y in z`).
    /// - Inside that scope, it also calls a closure for visiting the outer `elt`
    ///   of a list/dict/set comprehension or generator expression
    /// - It then pops the new scope off the stack
    ///
    /// [`Comprehension`]: ast::Comprehension
    fn with_generators_scope(
        &mut self,
        scope: NodeWithScopeRef,
        generators: &'ast [ast::Comprehension],
        visit_outer_elt: impl FnOnce(&mut Self),
    ) -> FileScopeId {
        let mut generators_iter = generators.iter();

        let Some(generator) = generators_iter.next() else {
            unreachable!("Expression must contain at least one generator");
        };

        // The `iter` of the first generator is evaluated in the outer scope, while all subsequent
        // nodes are evaluated in the inner scope.
        let value = self.add_standalone_expression(&generator.iter);
        self.visit_expr(&generator.iter);
        let first_iteration_can_raise =
            generator.is_async || !Self::iteration_is_known_safe(&generator.iter);
        self.record_exception_checkpoint_if(first_iteration_can_raise);
        let mut loopback_can_raise = first_iteration_can_raise || !generator.target.is_name_expr();

        // Clear the assignment stack before entering the comprehension scope.
        // If the comprehension appears inside an assignment target (e.g., error-recovered
        // `arr[::[x for *b in y for (b: _` is parsed as `StmtAnnAssign`), the outer
        // assignment context must not leak into the inner scope.
        let saved_assignments = std::mem::take(&mut self.current_assignments);

        self.push_scope(scope);
        let comprehension_scope = self.current_scope();

        if generators.iter().any(|generator| generator.is_async) {
            self.async_comprehensions.insert(comprehension_scope);
        }

        self.add_unpackable_assignment(
            &Unpackable::Comprehension {
                node: generator,
                first: true,
            },
            &generator.target,
            value,
        );

        let mut filtered_out_paths = Vec::new();
        for if_expr in &generator.ifs {
            filtered_out_paths.push(self.visit_comprehension_filter(if_expr));
        }

        for generator in generators_iter {
            let value = self.add_standalone_expression(&generator.iter);
            self.visit_expr(&generator.iter);
            let iteration_can_raise =
                generator.is_async || !Self::iteration_is_known_safe(&generator.iter);
            self.record_exception_checkpoint_if(iteration_can_raise);
            loopback_can_raise |= iteration_can_raise || !generator.target.is_name_expr();

            self.add_unpackable_assignment(
                &Unpackable::Comprehension {
                    node: generator,
                    first: false,
                },
                &generator.target,
                value,
            );

            for if_expr in &generator.ifs {
                filtered_out_paths.push(self.visit_comprehension_filter(if_expr));
            }
        }

        visit_outer_elt(self);
        for filtered_out_path in filtered_out_paths {
            self.flow_merge(filtered_out_path);
        }
        self.record_exception_checkpoint_if(loopback_can_raise);
        let nested_bindings = self.pop_scope();
        self.synthesize_comprehension_binding_definitions(nested_bindings);
        self.record_exception_checkpoint();

        self.current_assignments = saved_assignments;

        comprehension_scope
    }

    /// Visits a comprehension filter on its truthy path and returns the filtered-out path.
    ///
    /// A false filter skips the rest of the current iteration, but assignments performed while
    /// evaluating the filter remain observable:
    ///
    /// ```python
    /// [item for item in items if (last := item)]
    /// print(last)
    /// ```
    fn visit_comprehension_filter(&mut self, if_expr: &'ast ast::Expr) -> FlowSnapshot {
        self.visit_expr_with_context(if_expr, ExpressionContext::Condition);
        let condition_flow_snapshot = self.flow_snapshot_for_condition(if_expr);
        let filtered_out = if let Some(snapshots) = condition_flow_snapshot.into_branches() {
            self.flow_restore(snapshots.truthy);
            snapshots.falsy
        } else {
            self.flow_snapshot()
        };

        let (predicate, narrowing_id) = self.record_expression_narrowing_constraint(if_expr);
        let reachability_constraint = self.record_reachability_constraint_id(narrowing_id);
        let included_path = self.flow_snapshot();

        self.flow_restore(filtered_out);
        self.record_negated_narrowing_constraint(predicate, narrowing_id);
        self.record_negated_reachability_constraint(reachability_constraint);
        let filtered_out = self.flow_snapshot();

        self.flow_restore(included_path);
        filtered_out
    }

    fn declare_parameters(&mut self, parameters: &'ast ast::Parameters) {
        for parameter in parameters.iter_non_variadic_params() {
            self.declare_parameter(parameter);
        }
        if let Some(vararg) = parameters.vararg.as_ref() {
            let symbol = self.add_symbol(vararg.name.id().clone());
            self.current_place_table_mut()
                .symbol_mut(symbol)
                .mark_parameter();
            self.add_definition(
                symbol.into(),
                ParameterDefinitionNodeRef::VariadicPositionalParameter(vararg),
            );
        }
        if let Some(kwarg) = parameters.kwarg.as_ref() {
            let symbol = self.add_symbol(kwarg.name.id().clone());
            self.current_place_table_mut()
                .symbol_mut(symbol)
                .mark_parameter();
            self.add_definition(
                symbol.into(),
                ParameterDefinitionNodeRef::VariadicKeywordParameter(kwarg),
            );
        }
    }

    fn declare_parameter(&mut self, parameter: &'ast ast::ParameterWithDefault) {
        let symbol = self.add_symbol(parameter.name().id().clone());

        let definition = self.add_definition(
            symbol.into(),
            ParameterDefinitionNodeRef::Parameter(parameter),
        );

        self.current_place_table_mut()
            .symbol_mut(symbol)
            .mark_parameter();

        // basedpython: the argument went to the parameter's binder; the pattern
        // destructures it from there
        if let Some(pattern) = parameter.parameter.pattern.as_deref() {
            self.add_destructure_definitions_for(pattern, definition);
        }
    }

    fn declare_lambda_parameters(
        &mut self,
        parameters: &'ast ast::Parameters,
        lambda: &'ast ast::ExprLambda,
    ) {
        let mut index = 0;
        for parameter in &parameters.posonlyargs {
            self.declare_lambda_parameter(index, parameter, lambda);
            index += 1;
        }
        for parameter in &parameters.args {
            self.declare_lambda_parameter(index, parameter, lambda);
            index += 1;
        }
        if let Some(vararg) = parameters.vararg.as_ref() {
            let symbol = self.add_symbol(vararg.name.id().clone());
            self.current_place_table_mut()
                .symbol_mut(symbol)
                .mark_parameter();
            self.add_definition(
                symbol.into(),
                LambdaParameterDefinitionNodeRef {
                    index,
                    lambda,
                    parameter: ParameterDefinitionNodeRef::VariadicPositionalParameter(vararg),
                },
            );
            index += 1;
        }
        for parameter in &parameters.kwonlyargs {
            self.declare_lambda_parameter(index, parameter, lambda);
            index += 1;
        }
        if let Some(kwarg) = parameters.kwarg.as_ref() {
            let symbol = self.add_symbol(kwarg.name.id().clone());
            self.current_place_table_mut()
                .symbol_mut(symbol)
                .mark_parameter();
            self.add_definition(
                symbol.into(),
                LambdaParameterDefinitionNodeRef {
                    index,
                    lambda,
                    parameter: ParameterDefinitionNodeRef::VariadicKeywordParameter(kwarg),
                },
            );
        }
    }

    fn declare_lambda_parameter(
        &mut self,
        index: usize,
        parameter: &'ast ast::ParameterWithDefault,
        lambda: &'ast ast::ExprLambda,
    ) {
        let symbol = self.add_symbol(parameter.name().id().clone());

        self.add_definition(
            symbol.into(),
            LambdaParameterDefinitionNodeRef {
                index,
                lambda,
                parameter: ParameterDefinitionNodeRef::Parameter(parameter),
            },
        );

        self.current_place_table_mut()
            .symbol_mut(symbol)
            .mark_parameter();
    }

    /// Add an unpackable assignment for the given [`Unpackable`].
    ///
    /// This method handles assignments that can contain unpacking like assignment statements,
    /// for statements, etc.
    fn add_unpackable_assignment(
        &mut self,
        unpackable: &Unpackable<'ast>,
        target: &'ast ast::Expr,
        value: Expression<'db>,
    ) {
        self.record_exception_checkpoint_if(matches!(
            target,
            ast::Expr::List(_) | ast::Expr::Tuple(_)
        ));

        let current_assignment = match target {
            ast::Expr::List(_) | ast::Expr::Tuple(_) => {
                if matches!(unpackable, Unpackable::Comprehension { .. }) {
                    debug_assert_eq!(
                        self.scopes[self.current_scope()].node().scope_kind(),
                        ScopeKind::Comprehension
                    );
                }
                // The first iterator of the comprehension is evaluated in the outer scope, while all subsequent
                // nodes are evaluated in the inner scope.
                // SAFETY: The current scope is the comprehension, and the comprehension scope must have a parent scope.
                let value_file_scope =
                    if let Unpackable::Comprehension { first: true, .. } = unpackable {
                        self.scope_stack
                            .iter()
                            .rev()
                            .nth(1)
                            .expect("The comprehension scope must have a parent scope")
                            .file_scope_id
                    } else {
                        self.current_scope()
                    };
                let unpack = Unpack::new(
                    self.db,
                    self.file,
                    value_file_scope,
                    self.current_scope(),
                    // Note `target` belongs to the `self.module` tree
                    AstNodeRef::new(self.module, target),
                    UnpackValue::new(unpackable.kind(), value),
                );
                self.unpacks_by_target.insert(target.into(), unpack);
                Some(unpackable.as_current_assignment(Some(unpack)))
            }
            ast::Expr::Name(_)
            | ast::Expr::Starred(_)
            | ast::Expr::Attribute(_)
            | ast::Expr::Subscript(_) => Some(unpackable.as_current_assignment(None)),
            _ => None,
        };

        if let Some(current_assignment) = current_assignment {
            self.push_assignment(current_assignment);
        }

        self.visit_expr(target);

        if current_assignment.is_some() {
            // Only need to pop in the case where we pushed something
            self.pop_assignment();
        }
    }

    /// basedpython: records that `stmt` declares `symbol` with a binding keyword, if
    /// it does, and — when a block is open to scope it to — that `symbol` goes out
    /// of scope when that block ends.
    ///
    /// Only a plain name is ever block-scoped: an attribute or a subscript belongs
    /// to whatever object it hangs off, which outlives the block either way, so the
    /// caller only reaches here for a `Name` target.
    fn record_binding_keyword(&mut self, stmt: &ast::StmtAnnAssign, symbol: ScopedSymbolId) {
        // only a basedpython file can carry one, and asking costs a read of the
        // source that a python file would otherwise never need here
        if !self.source_type.is_basedpython() {
            return;
        }
        let Some((keyword, keyword_range)) = binding_keyword(stmt, self.source_text().as_str())
        else {
            return;
        };
        self.current_place_table_mut().mark_keyword_declared(symbol);

        if !self.block_scoped_declarations {
            return;
        }
        if let Some(block) = self.open_blocks.last_mut() {
            block.declared.push(PendingBlockDeclaration {
                symbol,
                keyword,
                keyword_range,
            });
        }
    }

    /// basedpython: visits the body of a block statement — an `if` or `elif` or
    /// `else` clause, a loop body, a `with` body, a `try` clause, a `match` case.
    ///
    /// Python has no block scopes: a name bound anywhere in a function is a local
    /// of that whole function, and the python this lowers to keeps it that way. So
    /// a `let` / `var` written in a block is *visible* only within the block, which
    /// is modeled by unbinding the names it declared once the block has been
    /// walked. Every shape of control flow then falls out of the flow analysis
    /// already there, exactly as a `del` at the end of the block would.
    fn visit_block_body(&mut self, body: &'ast [ast::Stmt]) -> BlockDeclarations {
        if !self.block_scoped_declarations {
            self.visit_body(body);
            return BlockDeclarations::default();
        }

        let Some(range) = body
            .first()
            .zip(body.last())
            .map(|(first, last)| TextRange::new(first.start(), last.end()))
        else {
            // an empty body — the synthetic `else` the `if` handling adds when the
            // source has none — declares nothing
            self.visit_body(body);
            return BlockDeclarations::default();
        };

        self.open_blocks.push(OpenBlock::default());
        self.visit_body(body);
        let block = self
            .open_blocks
            .pop()
            .expect("the block pushed above is still on the stack");

        let out_of_scope: BlockDeclarations = block.out_of_scope().collect();
        if out_of_scope.is_empty() {
            return out_of_scope;
        }

        if !block.declared.is_empty() {
            let scope = self.current_scope();
            let recorded = self.block_declarations_by_scope.entry(scope).or_default();
            recorded.reserve(block.declared.len());
            for PendingBlockDeclaration {
                symbol,
                keyword,
                keyword_range,
            } in block.declared
            {
                recorded.push(BlockScopedDeclaration {
                    symbol,
                    keyword,
                    keyword_range,
                    block: range,
                });
            }
        }

        // the names the block itself declared are unbound where it ends; the ones a
        // nested block declared were already unbound where *that* block ended, and
        // are carried up only so an edge out of an enclosing block can unbind them
        // as well
        self.unbind_block_declarations(&out_of_scope);
        if let Some(enclosing) = self.open_blocks.last_mut() {
            enclosing.closed.extend(out_of_scope.iter().copied());
        }
        out_of_scope
    }

    /// basedpython: takes `symbols` out of scope, as leaving the block that declared
    /// them does.
    fn unbind_block_declarations(&mut self, symbols: &BlockDeclarations) {
        for symbol in symbols {
            self.invalidate_narrowing_aliases_for((*symbol).into());
            self.delete_binding((*symbol).into());
        }
    }

    /// basedpython: takes out of scope every name declared in a block the current
    /// `break` or `continue` jumps out of.
    ///
    /// The jump leaves each of those blocks just as running off its end does, but
    /// from the inside, where the block has not unbound anything yet.
    fn unbind_blocks_left_by_jump(&mut self) {
        let Some(current_loop) = self.current_scope_info().current_loop.as_ref() else {
            return;
        };
        let left: BlockDeclarations = self.open_blocks[current_loop.blocks_at_entry..]
            .iter()
            .flat_map(OpenBlock::out_of_scope)
            .collect();
        self.unbind_block_declarations(&left);
    }

    pub(super) fn build(mut self) -> SemanticIndex<'db> {
        self.visit_body(self.module.suite());

        // Pop the root scope
        self.pop_scope();
        self.sweep_nonlocal_lazy_snapshots();
        assert!(self.scope_stack.is_empty());

        assert_eq!(&self.current_assignments, &[]);

        let ast_ids = super::ast_ids::AstIds::from_builders(self.ast_ids);

        let mut semantic_syntax_errors = self.semantic_syntax_errors.into_inner();
        semantic_syntax_errors.shrink_to_fit();
        let fluid_uses_by_candidate = FrozenMap::from_entries(
            self.fluid_uses_by_candidate
                .into_iter()
                .map(|(definition, uses)| (definition, uses.into_boxed_slice()))
                .collect(),
        );

        let mut use_def_map_interner = UseDefMapInterner::default();

        SemanticIndex {
            place_tables: self
                .place_tables
                .into_iter()
                .map(|builder| Arc::new(builder.finish()))
                .collect(),
            scopes: self.scopes.into(),
            definitions_by_node: DefinitionsByNode::from_map(self.definitions_by_node),
            expressions_by_node: self.expressions_by_node,
            unpacks_by_target: FrozenMap::from(self.unpacks_by_target),
            statements_by_node: self.statements_by_node,
            scope_ids_by_scope: self.scope_ids_by_scope.into(),
            ast_ids,
            scopes_by_expression: self.scopes_by_expression.build(),
            scopes_by_node: self.scopes_by_node,
            use_def_maps: self
                .use_def_maps
                .into_iter()
                .map(|builder| use_def_map_interner.intern(builder.finish()))
                .collect(),
            enclosing_lambda_statements: FrozenMap::from(self.enclosing_lambda_statements),
            fluid_candidates_by_use: FrozenMap::from(self.fluid_candidates_by_use),
            fluid_uses_by_candidate,
            imported_modules: FrozenSet::from(self.imported_modules),
            has_future_annotations: self.has_future_annotations,
            enclosing_snapshots: FrozenMap::from(self.enclosing_snapshots),
            semantic_syntax_errors,
            generator_functions: FrozenSet::from(self.generator_functions),
            basedpython_statement_calls: FrozenSet::from(self.basedpython_statement_calls),
            async_comprehensions: FrozenSet::from(self.async_comprehensions),
            narrowing_alias_predicates: FrozenMap::from(self.alias_predicates),
            destructures: FrozenMap::from(self.destructures),
            case_names: FrozenMap::from(self.case_names),
            block_scoped_declarations: FrozenMap::from_entries(
                self.block_declarations_by_scope
                    .into_iter()
                    .map(|(scope, mut declarations)| {
                        // a block records when it ends, so a nested one records
                        // first even though it was written later
                        declarations
                            .sort_unstable_by_key(|declaration| declaration.keyword_range.start());
                        (scope, declarations.into_boxed_slice())
                    })
                    .collect(),
            ),
        }
    }

    fn with_semantic_checker(&mut self, f: impl FnOnce(&mut SemanticSyntaxChecker, &Self)) {
        let mut checker = std::mem::take(&mut self.semantic_checker);
        f(&mut checker, self);
        self.semantic_checker = checker;
    }

    fn source_text(&self) -> &SourceText {
        self.source_text
            .get_or_init(|| source_text(self.db, self.file.file(self.db)))
    }

    fn visit_expr_with_context(&mut self, expr: &'ast ast::Expr, context: ExpressionContext) {
        self.with_semantic_checker(|semantic, builder| semantic.visit_expr(expr, builder));

        self.scopes_by_expression
            .record_expression(expr, self.current_scope());

        match expr {
            ast::Expr::Name(ast::ExprName { ctx, .. })
            | ast::Expr::Attribute(ast::ExprAttribute { ctx, .. })
            | ast::Expr::Subscript(ast::ExprSubscript { ctx, .. }) => {
                // Record place effects after walking the expression. For names, this is
                // equivalent because `walk_expr` is a no-op; for attribute/subscript places,
                // child evaluation can introduce bindings (for example via walrus operators),
                // and those bindings need to exist before we register parent/member associations.
                let mut deferred_effects = None;
                if let Some(mut place_expr) = PlaceExpr::try_from_expr(expr) {
                    if let Some(method_scope_id) = self.is_method_or_eagerly_executed_in_method()
                        && let PlaceExpr::Member(member) = &mut place_expr
                        && member.is_instance_attribute_candidate()
                        && let Some(attribute) = expr.as_attribute_expr()
                    {
                        // We specifically mark direct attribute assignments to the first
                        // parameter of a method, i.e. typically `self` or `cls`.
                        // However, we must check that the symbol hasn't been shadowed by an
                        // intermediate scope (e.g., a comprehension variable: `for self in [...]`)
                        // and that the AST base is still the original name rather than a
                        // rebinding expression such as `(self := other).x`.
                        let accessed_object_refers_to_first_parameter =
                            self.current_first_parameter_name.is_some_and(|first| {
                                attribute
                                    .value
                                    .as_name_expr()
                                    .is_some_and(|name| name.id == first)
                                    && !self.is_symbol_bound_in_intermediate_eager_scopes(
                                        first,
                                        method_scope_id,
                                    )
                            });

                        if accessed_object_refers_to_first_parameter {
                            member.mark_instance_attribute();
                        }
                    }

                    let (is_use, is_definition) = match (ctx, self.current_assignment()) {
                        (ast::ExprContext::Store, Some(CurrentAssignment::AugAssign(_))) => {
                            // Record the target load now; the definition is recorded separately
                            // after visiting the right-hand side.
                            (true, false)
                        }
                        (ast::ExprContext::Load, _) => (true, false),
                        (ast::ExprContext::Store, _) => (false, true),
                        (ast::ExprContext::Del, _) => (true, true),
                        (ast::ExprContext::Invalid, _) => (false, false),
                    };
                    deferred_effects = Some((place_expr, is_use, is_definition));
                }

                walk_expr(self, expr);

                let is_use = deferred_effects
                    .as_ref()
                    .is_some_and(|(_, is_use, _)| *is_use);
                let can_raise = self.place_access_can_raise(expr, is_use);
                self.record_exception_checkpoint_if(can_raise);

                if let Some((place_expr, is_use, is_definition)) = deferred_effects {
                    let place_id = self.add_place(place_expr);

                    if is_use {
                        self.record_place_use(place_id, expr);

                        // Keep track of any uses of fluid specialization candidates.
                        if let Some(candidate_def) = self.fluid_candidate_binding(expr) {
                            let loops: Box<[TextRange]> = self.loop_ranges.as_slice().into();
                            if let Some(current_statement) = self.current_statements.last_mut() {
                                current_statement.fluid_uses.push((
                                    candidate_def,
                                    expr.into(),
                                    expr.range(),
                                    loops,
                                ));
                            }
                        }
                    }

                    if is_definition {
                        self.record_place_definition(place_id, expr);
                    }

                    if let Some(unpack_position) = self
                        .current_assignment_mut()
                        .and_then(CurrentAssignment::unpack_position_mut)
                    {
                        *unpack_position = UnpackPosition::Other;
                    }
                }
            }
            ast::Expr::Named(node) => {
                // basedpython: anonymous named tuples and Parameters specs use
                // `Expr::Named` to represent `name: type` field labels. these
                // aren't walrus assignments — the inner Name has
                // `ExprContext::Invalid` to suppress place-effects — so we
                // skip the assignment scope entirely. without this, ty's
                // scope inference would call `expect_single_definition` on
                // the named expr and panic
                if matches!(node.target.as_ref(), ast::Expr::Name(n) if matches!(n.ctx, ast::ExprContext::Invalid))
                {
                    self.visit_expr(&node.value);
                    return;
                }
                self.visit_expr(&node.value);

                // See https://peps.python.org/pep-0572/#differences-between-assignment-expressions-and-assignment-statements
                if node.target.is_name_expr() {
                    self.push_assignment(CurrentAssignment::Named(node));
                    self.visit_expr(&node.target);
                    self.pop_assignment();
                } else {
                    self.visit_expr(&node.target);
                }
            }
            ast::Expr::Lambda(lambda) => {
                self.current_statement_mut()
                    .expect("every lambda expression is part of a statement")
                    .lambda_expressions
                    .push(lambda);

                if let Some(parameters) = &lambda.parameters {
                    // The default value of the parameters needs to be evaluated in the
                    // enclosing scope.
                    for default in parameters
                        .iter_non_variadic_params()
                        .filter_map(|param| param.default.as_deref())
                    {
                        self.visit_expr(default);
                    }
                    self.visit_parameters(parameters);
                }
                // return type annotation evaluated in enclosing scope, matching function defs
                if let Some(returns) = &lambda.returns {
                    self.visit_annotation(returns);
                }
                self.push_scope(NodeWithScopeRef::Lambda(lambda));

                // Add symbols and definitions for the parameters to the lambda scope.
                if let Some(parameters) = lambda.parameters.as_ref() {
                    self.declare_lambda_parameters(parameters, lambda);
                }

                self.visit_expr(lambda.body.as_ref());
                self.pop_scope();
            }
            ast::Expr::If(node) => self.visit_if_expression(node, context),
            ast::Expr::ListComp(
                list_comprehension @ ast::ExprListComp {
                    elt, generators, ..
                },
            ) => {
                let scope = self.with_generators_scope(
                    NodeWithScopeRef::ListComprehension(list_comprehension),
                    generators,
                    |builder| builder.visit_expr(elt),
                );
                if self.async_comprehensions.contains(&scope) {
                    self.mark_current_comprehension_async();
                }
            }
            ast::Expr::SetComp(
                set_comprehension @ ast::ExprSetComp {
                    elt, generators, ..
                },
            ) => {
                let scope = self.with_generators_scope(
                    NodeWithScopeRef::SetComprehension(set_comprehension),
                    generators,
                    |builder| builder.visit_expr(elt),
                );
                if self.async_comprehensions.contains(&scope) {
                    self.mark_current_comprehension_async();
                }
            }
            ast::Expr::Generator(
                generator @ ast::ExprGenerator {
                    elt, generators, ..
                },
            ) => {
                self.with_generators_scope(
                    NodeWithScopeRef::GeneratorExpression(generator),
                    generators,
                    |builder| builder.visit_expr(elt),
                );
            }
            ast::Expr::DictComp(
                dict_comprehension @ ast::ExprDictComp {
                    key,
                    value,
                    generators,
                    ..
                },
            ) => {
                let scope = self.with_generators_scope(
                    NodeWithScopeRef::DictComprehension(dict_comprehension),
                    generators,
                    |builder| {
                        if let Some(key) = key {
                            builder.visit_expr(key);
                        }
                        builder.visit_expr(value);
                    },
                );
                if self.async_comprehensions.contains(&scope) {
                    self.mark_current_comprehension_async();
                }
            }
            // basedpython: `a ?? b` evaluates `b` only when `a` is `None`, so `b`
            // is a branch — a binding it makes is only possibly bound afterwards,
            // and a `raise` or `return` in it does not end the enclosing flow
            ast::Expr::BinOp(ast::ExprBinOp {
                left,
                op: ast::Operator::Coalesce,
                right,
                ..
            }) => self.visit_coalesce_expression(left, right),
            ast::Expr::Call(_) | ast::Expr::BinOp(_) => {
                walk_expr(self, expr);
                self.record_exception_checkpoint();
            }
            ast::Expr::UnaryOp(unary) => {
                self.visit_expr_with_context(
                    &unary.operand,
                    if unary.op == ast::UnaryOp::Not {
                        context
                    } else {
                        ExpressionContext::Value
                    },
                );
                self.record_exception_checkpoint_if(
                    unary.op != ast::UnaryOp::Not
                        || !Self::condition_evaluation_is_known_safe(&unary.operand),
                );
            }
            ast::Expr::Compare(ast::ExprCompare {
                left,
                ops,
                comparators,
                ..
            }) => {
                self.visit_expr(left);
                for (op, comparator) in ops.iter().zip(comparators) {
                    self.visit_expr(comparator);
                    self.record_exception_checkpoint_if(!matches!(
                        op,
                        ast::CmpOp::Is | ast::CmpOp::IsNot
                    ));
                }
            }
            ast::Expr::BoolOp(node) => self.visit_bool_expression(node, context),
            ast::Expr::StringLiteral(_) => {
                walk_expr(self, expr);
            }
            ast::Expr::Yield(_) | ast::Expr::YieldFrom(_) => {
                let scope = self.current_scope();
                if self.scopes[scope].kind() == ScopeKind::Function {
                    self.generator_functions.insert(scope);
                }
                walk_expr(self, expr);
                self.record_exception_checkpoint();
            }
            ast::Expr::Await(_) => {
                self.mark_current_comprehension_async();
                walk_expr(self, expr);
                self.record_exception_checkpoint();
            }
            // basedpython: a statement expression's wrapped statement is visited
            // as an ordinary statement, so everything it binds and narrows is
            // recorded in the enclosing scope. Its *value* is modelled as a
            // synthetic place written at each of the statement's value positions
            // and read at the expression itself, which gives exhaustiveness and
            // the union of branch types from the existing flow analysis.
            ast::Expr::Statement(statement) => self.visit_statement_expression(expr, statement),
            _ => {
                walk_expr(self, expr);
            }
        }

        // basedpython: this expression may produce the value of the statement
        // expression currently being visited
        if let Some(current) = self.current_statement_expressions.last()
            && current.values.contains(&ExpressionNodeKey::from(expr))
        {
            self.record_statement_expression_value(expr, current.place);
        }
    }

    /// Visits a conditional expression without reserving its flow snapshots in every recursive
    /// expression-visitor frame. This matters for deeply nested expressions in unoptimized builds.
    fn visit_if_expression(&mut self, node: &'ast ast::ExprIf, context: ExpressionContext) {
        let ast::ExprIf {
            body, test, orelse, ..
        } = node;
        self.visit_expr_with_context(test, ExpressionContext::Condition);
        let condition_flow_snapshot = self.flow_snapshot_for_condition(test);
        let falsy = if let Some(snapshots) = condition_flow_snapshot.into_branches() {
            self.flow_restore(snapshots.truthy);
            snapshots.falsy
        } else {
            self.flow_snapshot()
        };
        let (predicate, predicate_id) = self.record_expression_narrowing_constraint(test);
        let reachability_constraint = self.record_reachability_constraint_id(predicate_id);
        let in_type_checking_block = self.in_type_checking_block;
        self.current_use_def_map_mut()
            .record_range_reachability(body.range(), in_type_checking_block);
        self.visit_expr_with_context(body, context);
        let post_body = self.flow_snapshot();
        self.flow_restore(falsy);

        self.record_negated_narrowing_constraint(predicate, predicate_id);
        self.record_negated_reachability_constraint(reachability_constraint);
        let in_type_checking_block = self.in_type_checking_block;
        self.current_use_def_map_mut()
            .record_range_reachability(orelse.range(), in_type_checking_block);
        self.visit_expr_with_context(orelse, context);
        self.flow_merge(post_body);
    }

    /// Keeps short-circuit flow snapshots out of the common recursive expression-visitor frame.
    fn visit_bool_expression(&mut self, node: &'ast ast::ExprBoolOp, context: ExpressionContext) {
        let ast::ExprBoolOp { values, op, .. } = node;
        let mut snapshots = vec![];
        let mut reachability_constraints = vec![];
        let mut last_condition_flow_snapshots = None;

        for (index, value) in values.iter().enumerate() {
            for id in &reachability_constraints {
                self.current_use_def_map_mut()
                    .record_reachability_constraint(*id); // TODO: nicer API
            }

            let in_type_checking_block = self.in_type_checking_block;
            self.current_use_def_map_mut()
                .record_range_reachability(value.range(), in_type_checking_block);
            self.visit_expr_with_context(value, context);

            // Only non-final values can short-circuit this boolean operation. The final
            // value can still have its own outcome-specific flow if it is nested.
            if index < values.len() - 1 {
                self.record_exception_checkpoint_if(!Self::condition_evaluation_is_known_safe(
                    value,
                ));
                let condition_flow_snapshots = self.take_condition_flow_snapshots(value);
                let predicate = self.build_predicate(value, context);
                let possibly_narrowed = self.compute_possibly_narrowed_places(&predicate);
                let predicate_id = match op {
                    ast::BoolOp::And => self.add_predicate(predicate),
                    ast::BoolOp::Or => self.add_negated_predicate(predicate),
                };
                let reachability_constraint = self
                    .current_reachability_constraints_mut()
                    .add_atom(predicate_id);

                let continuation = if let Some(condition_flow_snapshots) = condition_flow_snapshots
                {
                    let (short_circuit, continuation) =
                        condition_flow_snapshots.into_short_circuit_and_continuation(*op);
                    self.flow_restore(short_circuit);
                    continuation
                } else {
                    self.flow_snapshot()
                };

                // We first model the short-circuiting behavior. We take the short-circuit
                // path here if all of the previous short-circuit paths were not taken, so
                // we record all previously existing reachability constraints, and negate the
                // one for the current expression.

                self.record_negated_reachability_constraint(reachability_constraint);
                snapshots.push(self.flow_snapshot());

                // Then we model the non-short-circuiting behavior. Here, we need to delay
                // the application of the reachability constraint until after the expression
                // has been evaluated, so we only push it onto the stack here.
                self.flow_restore(continuation);
                self.record_narrowing_constraint_id_for_places(predicate_id, &possibly_narrowed);
                reachability_constraints.push(reachability_constraint);
            } else {
                last_condition_flow_snapshots = self.take_condition_flow_snapshots(value);
            }
        }

        let has_specialized_last = last_condition_flow_snapshots.is_some();
        let (last_short_circuit, no_short_circuit) =
            if let Some(condition_flow_snapshots) = last_condition_flow_snapshots {
                let (short_circuit, no_short_circuit) =
                    condition_flow_snapshots.into_short_circuit_and_continuation(*op);
                (Some(short_circuit), Some(no_short_circuit))
            } else {
                (
                    None,
                    values
                        .iter()
                        .any(|value| any_over_expr(value, &ast::Expr::is_named_expr))
                        .then(|| self.flow_snapshot()),
                )
            };

        if let Some(last_short_circuit) = last_short_circuit {
            self.flow_restore(last_short_circuit);
        }

        for snapshot in snapshots {
            self.flow_merge(snapshot);
        }

        if let Some(no_short_circuit) = no_short_circuit {
            let bool_op_key = ExpressionNodeKey::from(ast::ExprRef::BoolOp(node));
            let maybe_short_circuit = self.flow_snapshot();

            if has_specialized_last {
                // Restore the merged post-expression flow after constructing the two
                // outcome-specific snapshots.
                self.flow_merge(no_short_circuit.clone());
            }

            let (truthy, falsy) = match op {
                ast::BoolOp::And => (no_short_circuit, maybe_short_circuit),
                ast::BoolOp::Or => (maybe_short_circuit, no_short_circuit),
            };

            self.condition_flow_snapshots_by_node
                .insert(bool_op_key, ConditionFlowSnapshots { truthy, falsy });
        }
    }

    fn visit_stmt_impl(&mut self, stmt: &'ast ast::Stmt) {
        self.with_semantic_checker(|semantic, context| semantic.visit_stmt(stmt, context));

        let in_type_checking_block = self.in_type_checking_block;
        self.current_use_def_map_mut()
            .record_range_reachability(stmt.range(), in_type_checking_block);

        match stmt {
            ast::Stmt::FunctionDef(function_def) => {
                let ast::StmtFunctionDef {
                    decorator_list,
                    parameters,
                    type_params,
                    name,
                    returns,
                    raises,
                    body,
                    is_async: _,
                    is_trailing_lambda,
                    is_asserts_return: _,
                    range: _,
                    node_index: _,
                } = function_def;
                for decorator in decorator_list {
                    self.visit_decorator(decorator);
                }

                // basedpython: a trailing lambda's synthetic decorator holds the
                // called expression. its callee is a standalone expression so
                // the lambda's implicit `it` parameter can read the callee's
                // type without depending on the enclosing definition inference
                if let Some(callee) = function_def.trailing_lambda_callee() {
                    self.add_standalone_expression(callee);
                }

                // Evaluate default args before we visit the body. If the default expression ends
                // up looking at locally bound variables, `nonlocal` or `global` assignments in the
                // body shouldn't affect their inferred values. For example:
                // ```
                // x = 1
                // def f(y=reveal_type(x)):  # Literal[1]
                //     global x
                //     x = 2
                // reveal_type(x)  # Literal[1, 2]
                // ```
                for default in parameters
                    .iter_non_variadic_params()
                    .filter_map(|param| param.default.as_deref())
                {
                    self.visit_expr(default);
                }

                let (nested_bindings, block_scope) = self.with_type_params(
                    NodeWithScopeRef::FunctionTypeParameters(function_def),
                    type_params.as_deref(),
                    |builder| {
                        builder.visit_parameters(parameters);
                        if let Some(returns) = returns {
                            builder.visit_annotation(returns);
                        }
                        // basedpython: the `raises` clause is a type expression, and
                        // sits in the same scope as the return annotation
                        if let Some(raises) = raises {
                            builder.visit_annotation(raises);
                        }

                        builder.push_scope(NodeWithScopeRef::Function(function_def));
                        let block_scope = builder.current_scope();

                        builder.declare_parameters(parameters);

                        let mut first_parameter_name = parameters
                            .iter_non_variadic_params()
                            .next()
                            .map(|first_param| first_param.parameter.name.id().as_str());
                        std::mem::swap(
                            &mut builder.current_first_parameter_name,
                            &mut first_parameter_name,
                        );

                        builder.visit_body(body);

                        builder.current_first_parameter_name = first_parameter_name;
                        (builder.pop_scope(), block_scope)
                    },
                );

                // The nested bindings returned by `pop_scope` are exactly the ones that are
                // potentially visible at this point. That is, they include `global` and `nonlocal`
                // declarations in the popped functions body and any nested bodies, but they omit
                // the ones that resolved to the popped body. Synthesize a definition to record
                // them. This definition type has special shadowing behavior, so it doesn't shadow
                // prior bindings, and it remains visible after subsequent bindings. This
                // represents the fact that the nested function could be called at any time.
                //
                // NOTE: This is deliberately somewhat unsound. For example, bindings from parent
                // functions and sibling functions can also be visible at any point, depending on
                // when different functions get invoked. However, we really want examples like this
                // to do what users expect, so we accept the unsoundness here:
                //
                //     def f():
                //         x = 1
                //         def g():
                //             nonlocal x
                //             x = 2
                //         def h():
                //             nonlocal x
                //             x = 3
                //             # Technically `g` could get called at any time, including right
                //             # here. But inferring `Literal[2, 3]` here would be confusing.
                //             reveal_type(x)  # revealed: Literal[3]
                //          x = 4
                //          # On the other hand, users probably want to see 2 and 3 here, because
                //          # they're nested within this scope? Hopefully it's not too confusing.
                //          reveal_type(x)  # revealed: Literal[2, 3, 4]
                //
                // In other cases it can also be unsound that we only consider nested bindings to
                // be visible after their function definition, when in practice they could be
                // visible "before" (because nested functions can escape their lexical scope and
                // get called more than once). For more discussion of all these behaviors, see the
                // mdtest case "Visibility of `nonlocal` bindings from nested and sibling scopes"
                // and its `global` counterpart.
                self.synthesize_nested_binding_definitions(nested_bindings);

                // basedpython: a trailing-lambda block (`f:` + suite) runs inline at
                // its call site, so an assignment to an enclosing name writes through
                // to that binding (the lowering inserts the matching `global` /
                // `nonlocal`), reflected in `reveal_type` after the block. a `once`
                // block runs exactly once, so an unconditional write shadows; a
                // non-`once` block may run any number of times, so it unions.
                if *is_trailing_lambda {
                    let is_once = function_def
                        .trailing_lambda_callee()
                        .is_some_and(|callee| self.trailing_lambda_callee_is_once(callee));
                    self.synthesize_trailing_lambda_writebacks(block_scope, body, is_once);

                    // a `once` block runs exactly once; if it always returns, the
                    // enclosing function returns through it (the lowering
                    // propagates the return), so code after the block is
                    // unreachable — just like a `return` here
                    if is_once && Self::always_returns(body) {
                        self.record_terminal_finally_entry();
                        self.mark_unreachable();
                    }
                }

                // Decorator application can raise after defaults and annotations are evaluated.
                self.record_exception_checkpoint_if(!decorator_list.is_empty());

                // The symbol for the function name itself has to be evaluated
                // at the end to match the runtime evaluation of parameter defaults
                // and return-type annotations.
                let symbol = self.add_symbol(name.id.clone());

                // Record a use of the function name in the scope that it is defined in, so that it
                // can be used to find previously defined functions with the same name. This is
                // used to collect all the overloaded definitions of a function. This needs to be
                // done on the `Identifier` node as opposed to `ExprName` because that's what the
                // AST uses.
                let use_id = self.current_ast_ids_mut().record_use(name);
                self.current_use_def_map_mut()
                    .record_use(symbol.into(), use_id);

                self.add_definition(symbol.into(), function_def);
                self.mark_symbol_used(symbol);
            }
            ast::Stmt::ClassDef(class) => {
                for decorator in &class.decorator_list {
                    self.visit_decorator(decorator);
                }

                let nested_bindings = self.with_type_params(
                    NodeWithScopeRef::ClassTypeParameters(class),
                    class.type_params.as_deref(),
                    |builder| {
                        if let Some(arguments) = &class.arguments {
                            builder.visit_arguments(arguments);
                        }

                        builder.push_scope(NodeWithScopeRef::Class(class));
                        builder.visit_body(&class.body);

                        builder.pop_scope()
                    },
                );

                // We currently treat nested `global` and `nonlocal` bindings from class bodies the
                // same way as ones from function bodies above. That's correct in the common case
                // where they actually come from a function within the class. But when they appear
                // directly within a class body, this isn't quite correct, because these synthetic
                // definitions behave lazily, while class bodies are actually evaluated eagerly.
                self.synthesize_nested_binding_definitions(nested_bindings);

                // Class construction and decorator application can raise after the body executes.
                self.record_exception_checkpoint();

                // In Python runtime semantics, a class is registered after its scope is evaluated.
                // an `extension list:` block references the extended type rather than
                // declaring it, so it binds a mangled, per-statement symbol — invisible
                // to name resolution (`<` cannot appear in an identifier) but still
                // enumerable, so `extensions_in_module` can find every extension
                let symbol_name = if class.is_extension() {
                    Name::new(format!(
                        "<extension:{}:{}>",
                        class.name.id,
                        class.range.start().to_u32()
                    ))
                } else {
                    class.name.id.clone()
                };
                let symbol = self.add_symbol(symbol_name);
                self.add_definition(symbol.into(), class);
            }
            ast::Stmt::TypeAlias(type_alias) => {
                let symbol = self.add_symbol(
                    type_alias
                        .name
                        .as_name_expr()
                        .map(|name| name.id.clone())
                        .unwrap_or("<unknown>".into()),
                );
                self.add_definition(symbol.into(), type_alias);
                self.visit_expr(&type_alias.name);

                self.with_type_params(
                    NodeWithScopeRef::TypeAliasTypeParameters(type_alias),
                    type_alias.type_params.as_deref(),
                    |builder| {
                        builder.push_scope(NodeWithScopeRef::TypeAlias(type_alias));
                        builder.visit_expr(&type_alias.value);
                        builder.visit_type_match_cases(&type_alias.cases);
                        builder.pop_scope()
                    },
                );
            }
            ast::Stmt::Import(node) => {
                for (alias_index, alias) in node.names.iter().enumerate() {
                    self.record_exception_checkpoint();

                    // Mark the imported module, and all of its parents, as being imported in this
                    // file.
                    //
                    // basedpython: a static resource names a file rather than a
                    // module, and a path can read as a module name even when it
                    // is not one — `"config.json"` has two valid identifiers in
                    // it and names no module at all
                    if !alias.is_resource
                        && let Some(module_name) = ModuleName::new(&alias.name)
                    {
                        self.imported_modules.extend(module_name.ancestors());
                    }

                    let (symbol_name, is_reexported) = if let Some(asname) = &alias.asname {
                        self.scopes_by_expression
                            .record_expression(asname, self.current_scope());
                        (asname.id.clone(), asname.id == alias.name.id)
                    } else {
                        (Name::new(alias.name.id.split('.').next().unwrap()), false)
                    };

                    let symbol = self.add_symbol(symbol_name);
                    self.add_definition(
                        symbol.into(),
                        ImportDefinitionNodeRef {
                            node,
                            alias_index,
                            is_reexported,
                        },
                    );
                }
            }
            ast::Stmt::ImportFrom(node) => {
                self.record_exception_checkpoint();

                // If we see:
                //
                // * `from .x.y import z` (or `from whatever.thispackage.x.y`)
                // * And we are in an `__init__.py(i)` (hereafter `thispackage`)
                // * And this is the first time we've seen `from .x` in this module
                // * And we're in the global scope
                //
                // We introduce a local definition `x = <module 'thispackage.x'>` that occurs
                // before the `z = ...` declaration the import introduces. This models the fact
                // that the *first* time that you import 'thispackage.x' the python runtime creates
                // `x` as a variable in the global scope of `thispackage`.
                //
                // This is not a perfect simulation of actual runtime behaviour for *various*
                // reasons but it works well for most practical purposes. In particular it's nice
                // that `x` can be freely overwritten, and that we don't assume that an import
                // in one function is visible in another function.
                let mut is_self_import = false;
                let source_file = self.file.file(self.db);
                let resolver_environment = self.resolver_environment;
                if source_file.is_package(self.db)
                    && let Ok(module_name) = ModuleName::from_identifier_parts(
                        self.db,
                        ImportingFile::File(source_file, resolver_environment),
                        node.module.as_deref(),
                        node.level,
                    )
                    && let Ok(thispackage) = ModuleName::package_for_file(
                        self.db,
                        ImportingFile::File(source_file, resolver_environment),
                    )
                {
                    // Record whether this is equivalent to `from . import ...`
                    is_self_import = module_name == thispackage;

                    if node.module.is_some()
                        && let Some(relative_submodule) = module_name.relative_to(&thispackage)
                        && let Some(direct_submodule) = relative_submodule.components().next()
                        && !self.seen_submodule_imports.contains(direct_submodule)
                        && self.current_scope().is_global()
                    {
                        self.seen_submodule_imports
                            .insert(direct_submodule.to_owned());

                        let is_immediately_shadowed = node.names.iter().any(|alias| {
                            if &alias.name == "*" {
                                return false;
                            }

                            let bound_name = alias.asname.as_ref().unwrap_or(&alias.name);
                            bound_name.id.as_str() == direct_submodule
                        });

                        if !is_immediately_shadowed {
                            let direct_submodule_name = Name::new(direct_submodule);
                            let symbol = self.add_symbol(direct_submodule_name);

                            let module_index = if node.level == 0 {
                                // "whatever.thispackage.x.y" we want `x`
                                thispackage.components().count()
                            } else {
                                // ".x.y" we want `x` (level 1 => index 0)
                                // "..x.y" we want `y` (level 2 => index 1)
                                // (The Identifier doesn't include the prefix dots)
                                node.level as usize - 1
                            };
                            self.add_definition(
                                symbol.into(),
                                ImportFromSubmoduleDefinitionNodeRef { node, module_index },
                            );
                        }
                    }
                }

                let mut found_star = false;
                for (alias_index, alias) in node.names.iter().enumerate() {
                    // Loading each imported name can fail after the module import and any earlier
                    // names or package-submodule side effects have completed.
                    self.record_exception_checkpoint();

                    if &alias.name == "*" {
                        // The following line maintains the invariant that every AST node that
                        // implements `Into<DefinitionNodeKey>` must have an entry in the
                        // `definitions_by_node` map. Maintaining this invariant ensures that
                        // `SemanticIndex::definitions` can always look up the definitions for a
                        // given AST node without panicking.
                        //
                        // The reason why maintaining this invariant requires special handling here
                        // is that some `Alias` nodes may be associated with 0 definitions:
                        // - If the import statement has invalid syntax: multiple `*` names in the `names` list
                        //   (e.g. `from foo import *, bar, *`)
                        // - If the `*` import refers to a module that has 0 exported names.
                        // - If the module being imported from cannot be resolved.
                        self.add_entry_for_definition_key(alias.into());

                        if found_star {
                            continue;
                        }

                        found_star = true;

                        // Wildcard imports are invalid syntax everywhere except the top-level scope,
                        // and thus do not bind any definitions anywhere else
                        if !self.in_module_scope() {
                            continue;
                        }

                        let Some(module) = resolve_module_for_import_from(
                            self.db,
                            ImportingFile::File(source_file, resolver_environment),
                            node,
                        ) else {
                            continue;
                        };

                        let Some(referenced_file) = module.file(self.db) else {
                            continue;
                        };
                        let referenced_program_file =
                            ProgramFile::new(self.db, referenced_file, self.file.program(self.db));
                        // In order to understand the reachability of definitions created by a `*` import,
                        // we need to know the reachability of the global-scope definitions in the
                        // `referenced_module` the symbols imported from. Much like predicates for `if`
                        // statements can only have their reachability constraints resolved at type-inference
                        // time, the reachability of these global-scope definitions in the external module
                        // cannot be resolved at this point. As such, we essentially model each definition
                        // stemming from a `from exporter *` import as something like:
                        //
                        // ```py
                        // if <external_definition_is_visible>:
                        //     from exporter import name
                        // ```
                        //
                        // For more details, see the doc-comment on `StarImportPlaceholderPredicate`.
                        for export in exported_names(self.db, referenced_program_file) {
                            let symbol_id = self.add_symbol(export.clone());
                            let node_ref = StarImportDefinitionNodeRef { node, symbol_id };
                            let star_import = StarImportPlaceholderPredicate::new(
                                self.db,
                                self.file,
                                symbol_id,
                                referenced_program_file,
                            );

                            let star_import_predicate = self.add_predicate(star_import.into());

                            let scope = self.current_scope();
                            let associated_member_ids = self.place_tables[scope]
                                .associated_place_ids(ScopedPlaceId::Symbol(symbol_id));
                            let pre_definition = self.use_def_maps[scope]
                                .single_symbol_snapshot(symbol_id, associated_member_ids);

                            let pre_definition_reachability =
                                self.current_use_def_map().reachability;

                            // Temporarily modify the reachability to include the star import predicate,
                            // in order for the new definition to pick it up.
                            let reachability_constraints =
                                &mut self.current_use_def_map_mut().reachability_constraints;
                            let star_import_reachability =
                                reachability_constraints.add_atom(star_import_predicate);
                            let definition_reachability = reachability_constraints
                                .add_and_constraint(
                                    pre_definition_reachability,
                                    star_import_reachability,
                                );
                            self.current_use_def_map_mut().reachability = definition_reachability;

                            self.push_additional_definition(symbol_id.into(), node_ref);

                            self.current_use_def_map_mut()
                                .record_and_negate_star_import_reachability_constraint(
                                    star_import_reachability,
                                    symbol_id,
                                    pre_definition,
                                );

                            // Restore the reachability to its pre-definition state
                            self.current_use_def_map_mut().reachability =
                                pre_definition_reachability;
                        }

                        continue;
                    }

                    let (symbol_name, is_reexported) = if let Some(asname) = &alias.asname {
                        self.scopes_by_expression
                            .record_expression(asname, self.current_scope());
                        // It's re-exported if it's `from ... import x as x`
                        (&asname.id, asname.id == alias.name.id)
                    } else {
                        // As a non-standard rule to handle stubs in the wild, we consider
                        // `from . import x` and `from whatever.thispackage import x` in an
                        // `__init__.pyi` to re-export `x` (as long as it wasn't renamed).
                        // basedpython's `from ... export x` says so outright
                        (&alias.name.id, is_self_import || node.is_export)
                    };

                    // Look for eager imports `from __future__ import annotations`, ignore `as ...`
                    // We intentionally don't enforce the rules about location of `__future__`
                    // imports here, we assume the user's intent was to apply the `__future__`
                    // import, so we still check using it (and will also emit a diagnostic about a
                    // miss-placed `__future__` import.)
                    self.has_future_annotations |= !node.is_lazy
                        && alias.name.id == "annotations"
                        && node.module.as_deref() == Some("__future__");

                    let symbol = self.add_symbol(symbol_name.clone());

                    self.add_definition(
                        symbol.into(),
                        ImportFromDefinitionNodeRef {
                            node,
                            alias_index,
                            is_reexported,
                        },
                    );
                }
            }

            ast::Stmt::Assert(ast::StmtAssert {
                test,
                msg,
                range: _,
                node_index: _,
            }) => {
                // We model an `assert test, msg` statement here. Conceptually, we can think of
                // this as being equivalent to the following:
                //
                // ```py
                // if not test:
                //     msg
                //     <halt>
                //
                // <whatever code comes after>
                // ```
                //
                // Importantly, the `msg` expression is only evaluated if the `test` expression is
                // falsy. This is why we apply the negated `test` predicate as a narrowing and
                // reachability constraint on the `msg` expression.
                //
                // The other important part is the `<halt>`. This lets us skip merging the
                // `msg` branch back into the following flow, since there is no way of getting out
                // of that branch. Code after the assertion starts from the condition's truthy flow.

                self.visit_expr_with_context(test, ExpressionContext::Condition);
                let condition_flow_snapshot = self.flow_snapshot_for_condition(test);
                let predicate = self.build_predicate(test, ExpressionContext::Condition);

                if msg.is_some()
                    || self
                        .exception_context_stack_manager
                        .has_active_exception_handler(self)
                {
                    let truthy = if let Some(snapshots) = condition_flow_snapshot.into_branches() {
                        self.flow_restore(snapshots.falsy);
                        snapshots.truthy
                    } else {
                        self.flow_snapshot()
                    };
                    let negated_predicate = predicate.negated();
                    let predicate_id = self.record_narrowing_constraint(negated_predicate);
                    self.record_reachability_constraint_id(predicate_id);
                    if let Some(msg) = msg {
                        self.visit_expr(msg);
                    }
                    self.record_exception_checkpoint();
                    self.flow_restore(truthy);
                } else if let Some(truthy) = condition_flow_snapshot.into_truthy() {
                    self.flow_restore(truthy);
                }

                let predicate_id = self.record_narrowing_constraint(predicate);
                self.record_reachability_constraint_id(predicate_id);
            }

            ast::Stmt::Assign(node) => {
                debug_assert_eq!(&self.current_assignments, &[]);

                // basedpython: a decorator written above the assignment is an
                // ordinary expression written before it, and reads the names it
                // names there
                for decorator in &node.decorator_list {
                    self.visit_decorator(decorator);
                }

                self.visit_expr(&node.value);

                // Collection-literal fluid candidates must be standalone expressions to
                // participate in full-scope bidirectional inference. Call candidates are
                // not made standalone: their assignments must go through definition
                // inference so that special forms (`TypeVar(...)`, `NamedTuple(...)`,
                // ...) are still recognized.
                if node.targets.len() == 1
                    && matches!(
                        node.value.as_ref(),
                        ast::Expr::List(_) | ast::Expr::Set(_) | ast::Expr::Dict(_)
                    )
                {
                    self.add_standalone_assigned_expression(&node.value, node);
                }

                // Optimization for the common case: if there's just one target, and it's not an
                // unpacking, and the target is a simple name, we don't need the RHS to be a
                // standalone expression at all.
                if let [target] = &node.targets[..]
                    && target.is_name_expr()
                {
                    self.push_assignment(CurrentAssignment::Assign {
                        node,
                        unpack: None,
                        owner: BindingsOwner::Definition,
                    });
                    self.visit_expr(target);
                    self.pop_assignment();

                    self.try_register_narrowing_alias(target, Some(&node.value));
                } else {
                    let value = self.add_standalone_assigned_expression(&node.value, node);

                    for target in &node.targets {
                        self.add_unpackable_assignment(&Unpackable::Assign(node), target, value);
                    }
                }
            }
            ast::Stmt::AnnAssign(node) => {
                debug_assert_eq!(&self.current_assignments, &[]);
                // basedpython: as on a plain assignment, a decorator above the
                // declaration is read where it is written
                for decorator in &node.decorator_list {
                    self.visit_decorator(decorator);
                }
                // For an assignment with a value, an exception from the annotation or RHS must
                // not discard the declared type. The value is still bound only after the RHS
                // completes, so a handler can observe an earlier binding (or an unbound name).
                let pending = self.begin_annotated_assignment(node);
                self.visit_expr(&node.annotation);
                if let Some(value) = &node.value {
                    self.visit_expr(value);
                    // basedpython: a trailing lambda block defines a function,
                    // which a standalone expression cannot own
                    if self.is_method_or_eagerly_executed_in_method().is_some()
                        && !is_trailing_lambda_value(value)
                    {
                        // Record the right-hand side of the assignment as a standalone expression
                        // if we're inside a method. This allows type inference to infer the type
                        // of the value for annotated assignments like `self.CONSTANT: Final = 1`,
                        // where the type itself is not part of the annotation.
                        self.add_standalone_expression(value);
                    }
                }

                if let ast::Expr::Name(name) = &*node.target {
                    let symbol_id = self.add_symbol(name.id.clone());
                    self.record_binding_keyword(node, symbol_id);
                    let symbol = self.current_place_table().symbol(symbol_id);
                    // Check whether the variable has been declared global.
                    if symbol.is_global() {
                        self.report_semantic_error(SemanticSyntaxError {
                            kind: SemanticSyntaxErrorKind::AnnotatedGlobal(name.id.as_str().into()),
                            range: name.range,
                            python_version: self.python_version(),
                        });
                    }
                    // Check whether the variable has been declared nonlocal.
                    if symbol.is_nonlocal() {
                        self.report_semantic_error(SemanticSyntaxError {
                            kind: SemanticSyntaxErrorKind::AnnotatedNonlocal(
                                name.id.as_str().into(),
                            ),
                            range: name.range,
                            python_version: self.python_version(),
                        });
                    }
                }

                // See https://docs.python.org/3/library/ast.html#ast.AnnAssign
                if matches!(
                    *node.target,
                    ast::Expr::Attribute(_) | ast::Expr::Subscript(_) | ast::Expr::Name(_)
                ) {
                    self.push_assignment(CurrentAssignment::AnnAssign { node, pending });
                    self.visit_expr(&node.target);
                    self.pop_assignment();

                    self.try_register_narrowing_alias(&node.target, node.value.as_deref());
                } else {
                    self.visit_expr(&node.target);
                }
            }
            ast::Stmt::AugAssign(
                aug_assign @ ast::StmtAugAssign {
                    range: _,
                    node_index: _,
                    target,
                    op,
                    value,
                },
            ) => {
                debug_assert_eq!(&self.current_assignments, &[]);

                // An augmented assignment loads its target before evaluating the right-hand side,
                // but only defines the target after the operation succeeds.
                let is_place_target = matches!(
                    &**target,
                    ast::Expr::Name(_) | ast::Expr::Attribute(_) | ast::Expr::Subscript(_)
                );
                if is_place_target {
                    self.push_assignment(CurrentAssignment::AugAssign(aug_assign));
                    self.visit_expr(target);
                    self.pop_assignment();
                } else {
                    self.visit_expr(target);
                }

                self.visit_expr(value);

                if let ast::Expr::Name(ast::ExprName { id, .. }) = &**target
                    && id == "__all__"
                    && op.is_add()
                    && self.in_module_scope()
                    && let ast::Expr::Attribute(ast::ExprAttribute {
                        value: module,
                        attr,
                        ..
                    }) = &**value
                    && attr == "__all__"
                {
                    self.add_standalone_expression(module);
                }

                self.record_exception_checkpoint();

                if is_place_target
                    && let Some(place_expr) = PlaceExpr::try_from_expr(target.as_ref())
                {
                    let place_id = self.add_place(place_expr);
                    self.push_assignment(CurrentAssignment::AugAssign(aug_assign));
                    self.record_place_definition(place_id, target);
                    self.pop_assignment();
                }
            }
            // basedpython `let <pattern> := <subject> [else: ...]`: a single
            // `match` case that binds in the enclosing scope. Without an `else`
            // block the pattern has to be irrefutable — inference reports one
            // that is not — so the captures are bound unconditionally; with one,
            // the block is what runs when the pattern did not match
            ast::Stmt::Let(ast::StmtLet {
                pattern,
                value,
                orelse,
                range: _,
                node_index: _,
            }) => {
                let subject = self.add_standalone_expression(value);
                self.visit_expr(value);
                let (subject_targets, sequence_subject_targets) = self.match_subject_targets(value);

                // taken before the pattern is visited, so the captures it binds
                // are not visible on the path where nothing matched
                let no_match = self.flow_snapshot();

                // without an `else` block the pattern has to be irrefutable, so
                // a bare name there is only ever the capture it looks like
                let (predicate, case_names) = self.create_pattern_predicate(
                    PatternSubject::Expression(subject),
                    pattern,
                    None,
                    None,
                    !orelse.is_empty(),
                );
                let outer_match_case = self
                    .current_match_case
                    .replace(CurrentMatchCase::new(pattern, predicate, case_names));
                self.visit_pattern(pattern);
                self.current_match_case = outer_match_case;

                let (match_predicate, narrowing_id) = self.add_pattern_narrowing_constraint(
                    predicate,
                    &subject_targets,
                    &sequence_subject_targets,
                    false,
                );
                let reachability = self.record_reachability_constraint(match_predicate);

                let after_orelse_reachability = if orelse.is_empty() {
                    None
                } else {
                    let matched = self.flow_snapshot();
                    self.flow_restore(no_match);
                    self.record_negated_narrowing_constraint(match_predicate, narrowing_id);
                    self.record_negated_reachability_constraint(reachability);
                    self.visit_block_body(orelse);

                    // the block has to diverge, which is this point being
                    // unreachable. Recorded for inference to check, and left in
                    // the flow: when the block does not diverge, merging it back
                    // is what makes the captures possibly unbound
                    let after_orelse_reachability = self.current_reachability();

                    let after_orelse = self.flow_snapshot();
                    self.flow_restore(matched);
                    self.flow_merge(after_orelse);
                    Some(after_orelse_reachability)
                };

                self.record_destructure(pattern, predicate, after_orelse_reachability);
            }
            ast::Stmt::If(node) => {
                let (mut falsy, mut last_predicate, mut last_narrowing_id) =
                    self.visit_if_condition(node.pattern.as_deref(), &node.test);
                let mut last_reachability_constraint =
                    self.record_reachability_constraint_id(last_narrowing_id);

                let is_outer_block_in_type_checking = self.in_type_checking_block;

                let if_block_in_type_checking = is_if_type_checking(&node.test);

                // Track if we're in a chain that started with "not TYPE_CHECKING"
                let mut is_in_not_type_checking_chain = is_if_not_type_checking(&node.test);

                self.in_type_checking_block =
                    if_block_in_type_checking || is_outer_block_in_type_checking;

                self.visit_block_body(&node.body);

                let mut post_clauses: Vec<FlowSnapshot> = vec![];
                let elif_else_clauses = node.elif_else_clauses.iter().map(|clause| {
                    (
                        clause
                            .test
                            .as_ref()
                            .map(|test| (clause.pattern.as_deref(), test)),
                        clause.body.as_slice(),
                    )
                });
                let has_else = node
                    .elif_else_clauses
                    .last()
                    .is_some_and(|clause| clause.test.is_none());
                let elif_else_clauses = elif_else_clauses.chain(if has_else {
                    // if there's an `else` clause already, we don't need to add another
                    None
                } else {
                    // if there's no `else` branch, we should add a no-op `else` branch
                    Some((None, Default::default()))
                });

                for (clause_test, clause_body) in elif_else_clauses {
                    // snapshot after every block except the last; the last one will just become
                    // the state that we merge the other snapshots into
                    post_clauses.push(self.flow_snapshot());
                    // we can only take an elif/else branch if none of the previous ones were
                    // taken
                    self.flow_restore(falsy);

                    self.record_negated_narrowing_constraint(last_predicate, last_narrowing_id);
                    self.record_negated_reachability_constraint(last_reachability_constraint);

                    let next_falsy = if let Some((clause_pattern, elif_test)) = clause_test {
                        let next_falsy;
                        (next_falsy, last_predicate, last_narrowing_id) =
                            self.visit_if_condition(clause_pattern, elif_test);

                        last_reachability_constraint =
                            self.record_reachability_constraint_id(last_narrowing_id);

                        Some(next_falsy)
                    } else {
                        None
                    };

                    // Determine if this clause is in type checking context
                    let clause_in_type_checking = if let Some((_, elif_test)) = clause_test {
                        if is_if_type_checking(elif_test) {
                            // This block has "TYPE_CHECKING" condition
                            true
                        } else if is_if_not_type_checking(elif_test) {
                            // This block has "not TYPE_CHECKING" condition so we update the chain state for future blocks
                            is_in_not_type_checking_chain = true;
                            false
                        } else {
                            // This block has some other condition
                            // It's in type checking only if we're in a "not TYPE_CHECKING" chain
                            is_in_not_type_checking_chain
                        }
                    } else {
                        is_in_not_type_checking_chain
                    };

                    // Nested conditional clauses inherit an enclosing TYPE_CHECKING context.
                    self.in_type_checking_block =
                        is_outer_block_in_type_checking || clause_in_type_checking;

                    self.visit_block_body(clause_body);

                    let Some(next_falsy) = next_falsy else {
                        break;
                    };
                    falsy = next_falsy;
                }

                for post_clause_state in post_clauses {
                    self.flow_merge(post_clause_state);
                }

                self.in_type_checking_block = is_outer_block_in_type_checking;
            }
            ast::Stmt::While(
                while_stmt @ ast::StmtWhile {
                    test,
                    body,
                    orelse,
                    range: _,
                    node_index: _,
                },
            ) => {
                // Pre-walk the loop to collect all the bound places, then create a loop header
                // definition for each bound place. See `struct LoopHeader` for more on this. Loop
                // header definitions store the ID of a reserved `LoopHeader` that we populate
                // after walking the body.
                let bound_places = loop_bindings_visitor::collect_while_loop_bindings(while_stmt);
                let mut maybe_loop_header_info = None;
                // Avoid allocating a `LoopHeader` if there are no bound places in this loop.
                if !bound_places.is_empty() {
                    maybe_loop_header_info = Some(self.synthesize_loop_header_definitions(
                        LoopStmtRef::While(while_stmt),
                        bound_places,
                    ));
                }

                // Visit the test expression after creating loop headers, so that loop-back values
                // are visible.
                self.visit_expr_with_context(test, ExpressionContext::Condition);
                let condition_flow_snapshot = self.flow_snapshot_for_condition(test);

                // Take the pre_loop snapshot from the post-test fallback flow before restoring the
                // condition's truthy flow for the body. This preserves the zero-iteration path for
                // the loop exit merge below.
                let pre_loop = self.flow_snapshot();
                if let Some(truthy) = condition_flow_snapshot.into_truthy() {
                    self.flow_restore(truthy);
                }
                let (predicate, predicate_id) = self.record_expression_narrowing_constraint(test);
                self.record_reachability_constraint_id(predicate_id);

                let outer_loop = self.push_loop();
                self.visit_block_body(body);
                let this_loop = self.pop_loop(outer_loop);

                // Loop-back bindings include everything that's visible if/when control reaches the
                // end of the loop body, and they also include everything that's visible to a
                // `continue` statement. Merge the `continue` states before collecting bindings.
                for continue_state in this_loop.continue_states {
                    self.flow_merge(continue_state);
                }

                // Collect all the loop-back bindings (including the `continue` states we just
                // merged) and populate the `LoopHeader`.
                if let Some((header_id, bound_place_ids, loop_min_definition_id)) =
                    maybe_loop_header_info
                {
                    self.populate_loop_header(&bound_place_ids, header_id, loop_min_definition_id);
                }

                self.record_exception_checkpoint_if(!Self::condition_evaluation_is_known_safe(
                    test,
                ));

                // We execute the `else` branch once the condition evaluates to false. This could
                // happen without ever executing the body, if the condition is false the first time
                // it's tested. Or it could happen if a _later_ evaluation of the condition yields
                // false. So we merge in the pre-loop state here into the post-body state:
                self.flow_merge(pre_loop);

                // The `else` branch can only be reached if the loop condition *can* be false. To
                // model this correctly, we need a second copy of the while condition constraint,
                // since the first and later evaluations might produce different results. We would
                // otherwise simplify `predicate AND ~predicate` to `False`.
                let later_predicate_id = self.current_use_def_map_mut().add_predicate(predicate);
                let later_reachability_constraint = self
                    .current_reachability_constraints_mut()
                    .add_atom(later_predicate_id);
                self.record_negated_reachability_constraint(later_reachability_constraint);

                self.record_negated_narrowing_constraint(predicate, predicate_id);

                self.visit_block_body(orelse);

                // Breaking out of a while loop bypasses the `else` clause, so merge in the break
                // states after visiting `else`.
                for break_state in this_loop.break_states {
                    self.flow_merge(break_state);
                }
            }
            ast::Stmt::With(ast::StmtWith {
                items,
                body,
                is_async,
                ..
            }) => {
                for item @ ast::WithItem {
                    range: _,
                    node_index: _,
                    context_expr,
                    optional_vars,
                    pattern,
                } in items
                {
                    self.visit_expr(context_expr);
                    self.record_exception_checkpoint();

                    self.exception_context_stack_manager
                        .push_context_manager_context();

                    if let Some(optional_vars) = optional_vars.as_deref() {
                        let context_manager = self.add_standalone_expression(context_expr);
                        self.add_unpackable_assignment(
                            &Unpackable::WithItem {
                                item,
                                is_async: *is_async,
                            },
                            optional_vars,
                            context_manager,
                        );
                        // basedpython: the bound value went to the item's binder;
                        // the pattern destructures it from there
                        if let Some(pattern) = pattern.as_deref()
                            && let ast::Expr::Name(binder) = optional_vars
                        {
                            self.add_destructure_definitions(pattern, binder);
                        }
                    }
                }

                self.visit_block_body(body);

                for item in items.iter().rev() {
                    let mut exceptional_entries = self
                        .exception_context_stack_manager
                        .finish_context_manager_context()
                        .into_iter();

                    if let Some(exceptional_entry) = exceptional_entries.next() {
                        let normal_exit = self.flow_snapshot();
                        if normal_exit.is_always_unreachable() {
                            self.exception_context_stack_manager
                                .record_deferred_terminal_context_manager_exit();
                        }
                        let context_expr = &item.context_expr;
                        let expression = self
                            .expressions_by_node
                            .get(&ExpressionNodeKey::from(context_expr))
                            .copied()
                            .unwrap_or_else(|| self.add_standalone_expression(context_expr));
                        let predicate = PredicateOrLiteral::Predicate(Predicate {
                            node: PredicateNode::ContextManagerSuppresses {
                                expression,
                                is_async: *is_async,
                            },
                            is_positive: true,
                        });
                        let predicate_id = self.add_predicate(predicate);

                        self.flow_restore(exceptional_entry);
                        for exceptional_entry in exceptional_entries {
                            self.flow_merge(exceptional_entry);
                        }

                        self.record_ambiguous_reachability();
                        let reachability_constraint = self
                            .current_reachability_constraints_mut()
                            .add_atom(predicate_id);
                        let narrowing_constraint = self
                            .current_use_def_map_mut()
                            .narrowing_constraints
                            .add_atom(predicate_id);
                        self.current_use_def_map_mut()
                            .record_non_terminal_call_constraints(
                                reachability_constraint,
                                narrowing_constraint,
                            );

                        self.flow_merge(normal_exit);
                    }

                    // A manager cannot suppress an exception raised by its own exit method, but
                    // an earlier manager or enclosing `try` statement can still receive it.
                    self.record_exception_checkpoint();
                }
            }

            ast::Stmt::For(
                for_stmt @ ast::StmtFor {
                    range: _,
                    node_index: _,
                    is_async,
                    target,
                    pattern,
                    iter,
                    body,
                    orelse,
                },
            ) => {
                debug_assert_eq!(&self.current_assignments, &[]);

                let iter_expr = self.add_standalone_expression(iter);
                self.visit_expr(iter);
                let iteration_can_raise = *is_async || !Self::iteration_is_known_safe(iter);
                self.record_exception_checkpoint_if(iteration_can_raise);

                let literal_iterable_is_non_empty = (!*is_async)
                    .then(|| literal_iterable_truthiness(iter))
                    .and_then(Truthiness::into_bool);

                let (after_empty_iter, non_empty_range_constraint) =
                    match literal_iterable_is_non_empty {
                        Some(false) => {
                            let after_iter = self.flow_snapshot();
                            self.mark_unreachable();
                            (Some(after_iter), None)
                        }
                        Some(true) => (None, None),
                        None if is_direct_range_call(iter) => {
                            let after_iter = self.flow_snapshot();
                            let constraint = self.record_reachability_constraint(
                                PredicateOrLiteral::Predicate(Predicate {
                                    node: PredicateNode::IsNonEmptyIterable(iter_expr),
                                    is_positive: true,
                                }),
                            );

                            (None, Some((after_iter, constraint)))
                        }
                        None => {
                            self.record_ambiguous_reachability();
                            (None, None)
                        }
                    };

                let pre_loop = self.flow_snapshot();

                // Pre-walk the loop to collect all the bound places, then create a loop header
                // definition for each bound place. See `struct LoopHeader` for more on this. Loop
                // header definitions store the ID of a reserved `LoopHeader` that we populate
                // after walking the body.
                let bound_places = loop_bindings_visitor::collect_for_loop_bindings(for_stmt);
                let mut maybe_loop_header_info = None;
                // Avoid allocating a `LoopHeader` if there are no bound places in this loop.
                if !bound_places.is_empty() {
                    maybe_loop_header_info = Some(self.synthesize_loop_header_definitions(
                        LoopStmtRef::For(for_stmt),
                        bound_places,
                    ));
                }

                self.add_unpackable_assignment(&Unpackable::For(for_stmt), target, iter_expr);

                // basedpython: the element went to the loop's binder; the pattern
                // destructures it from there
                if let Some(pattern) = pattern.as_deref()
                    && let ast::Expr::Name(binder) = &**target
                {
                    self.add_destructure_definitions(pattern, binder);
                }

                let outer_loop = self.push_loop();
                self.visit_block_body(body);
                let this_loop = self.pop_loop(outer_loop);

                // Loop-back bindings include everything that's visible if/when control reaches the
                // end of the loop body, and they also include everything that's visible to a
                // `continue` statement. Merge the `continue` states before collecting bindings.
                for continue_state in this_loop.continue_states {
                    self.flow_merge(continue_state);
                }

                // Collect all the loop-back bindings (including the `continue` states we just
                // merged) and populate the `LoopHeader`.
                if let Some((header_id, bound_place_ids, loop_min_definition_id)) =
                    maybe_loop_header_info
                {
                    self.populate_loop_header(&bound_place_ids, header_id, loop_min_definition_id);
                }

                self.record_exception_checkpoint_if(iteration_can_raise || !target.is_name_expr());

                if let Some(after_iter) = after_empty_iter {
                    self.flow_restore(after_iter);
                } else if literal_iterable_is_non_empty.is_none() {
                    // We may execute the `else` clause without ever executing the body, so merge
                    // in a zero-iteration state before visiting `else`.
                    if let Some((after_iter, non_empty_range_constraint)) =
                        non_empty_range_constraint
                    {
                        let post_loop_body = self.flow_snapshot();
                        self.flow_restore(after_iter);
                        self.record_negated_reachability_constraint(non_empty_range_constraint);
                        let no_iteration = self.flow_snapshot();
                        self.flow_restore(post_loop_body);
                        self.flow_merge(no_iteration);
                    } else {
                        self.flow_merge(pre_loop);
                    }
                }
                self.visit_block_body(orelse);

                // Breaking out of a `for` loop bypasses the `else` clause, so merge in the break
                // states after visiting `else`.
                for break_state in this_loop.break_states {
                    self.flow_merge(break_state);
                }
            }
            ast::Stmt::Match(ast::StmtMatch {
                subject,
                cases,
                range: _,
                node_index: _,
            }) => {
                debug_assert_eq!(self.current_match_case, None);

                let subject_expr = self.add_standalone_expression(subject);
                self.visit_expr(subject);
                if cases.is_empty() {
                    return;
                }

                let (subject_targets, sequence_subject_targets) =
                    self.match_subject_targets(subject);

                let mut no_case_matched = self.flow_snapshot();

                let has_catchall = cases
                    .last()
                    .is_some_and(|case| case.guard.is_none() && case.pattern.is_wildcard());

                let mut post_case_snapshots = vec![];
                let mut previous_pattern: Option<PatternPredicate<'_>> = None;

                for (i, case) in cases.iter().enumerate() {
                    let (match_pattern_predicate, case_names) = self.create_pattern_predicate(
                        PatternSubject::Expression(subject_expr),
                        &case.pattern,
                        case.guard.as_deref(),
                        previous_pattern,
                        true,
                    );
                    // basedpython: `case A:` looks like a wildcard but is not one
                    // when the name resolves to an enum member, and that is not
                    // known until type checking. The shortcut below is a
                    // precision optimization, so the conservative answer is to
                    // give it up for any case that offered a name at all
                    let offers_case_names = !case_names.is_empty();
                    self.current_match_case = Some(CurrentMatchCase::new(
                        &case.pattern,
                        match_pattern_predicate,
                        case_names,
                    ));
                    self.record_exception_checkpoint_if(Self::pattern_can_raise(&case.pattern));
                    self.visit_pattern(&case.pattern);
                    self.current_match_case = None;
                    // unlike in [Stmt::If], we don't reset [no_case_matched]
                    // here because the effects of visiting a pattern is binding
                    // symbols, and this doesn't occur unless the pattern
                    // actually matches
                    let is_catchall = has_catchall && i == cases.len() - 1 && !offers_case_names;
                    let (match_predicate, match_narrowing_id) = self
                        .add_pattern_narrowing_constraint(
                            match_pattern_predicate,
                            &subject_targets,
                            &sequence_subject_targets,
                            is_catchall,
                        );
                    previous_pattern = Some(match_pattern_predicate);
                    let reachability_constraint =
                        self.record_reachability_constraint_id(match_narrowing_id);

                    // For a pattern `P` and guard `G`, the case body is reached through `P && G`,
                    // while the next case is reached through `!P || (P && !G)`. Save `P && !G`
                    // separately so it can be merged with the pattern-failure state after the body.
                    let match_success_guard_failure = case.guard.as_ref().map(|guard| {
                        self.visit_expr_with_context(guard, ExpressionContext::Condition);
                        let condition_flow_snapshot = self.flow_snapshot_for_condition(guard);
                        let falsy = if let Some(snapshots) = condition_flow_snapshot.into_branches()
                        {
                            self.flow_restore(snapshots.truthy);
                            snapshots.falsy
                        } else {
                            self.flow_snapshot()
                        };

                        let (guard_predicate, guard_predicate_id) =
                            self.record_expression_narrowing_constraint(guard);
                        let guard_reachability_constraint =
                            self.record_reachability_constraint_id(guard_predicate_id);
                        let guard_success = self.flow_snapshot();

                        self.flow_restore(falsy);
                        self.record_negated_narrowing_constraint(
                            guard_predicate,
                            guard_predicate_id,
                        );
                        self.record_negated_reachability_constraint(guard_reachability_constraint);
                        let match_success_guard_failure = self.flow_snapshot();
                        self.flow_restore(guard_success);
                        match_success_guard_failure
                    });

                    self.visit_block_body(&case.body);

                    post_case_snapshots.push(self.flow_snapshot());

                    if i != cases.len() - 1 || !has_catchall {
                        // We need to restore the state after each case, but not after the last
                        // one. The last one will just become the state that we merge the other
                        // snapshots into.
                        self.flow_restore(no_case_matched.clone());
                        self.record_negated_narrowing_constraint(
                            match_predicate,
                            match_narrowing_id,
                        );
                        self.record_negated_reachability_constraint(reachability_constraint);
                        if let Some(match_success_guard_failure) = match_success_guard_failure {
                            self.flow_merge(match_success_guard_failure);
                        } else {
                            assert!(case.guard.is_none());
                        }
                    } else {
                        debug_assert!(match_success_guard_failure.is_none());
                        debug_assert!(case.guard.is_none());
                    }

                    no_case_matched = self.flow_snapshot();
                }

                for post_clause_state in post_case_snapshots {
                    self.flow_merge(post_clause_state);
                }
            }
            ast::Stmt::Try(ast::StmtTry {
                body,
                handlers,
                orelse,
                finalbody,
                is_star,
                range: _,
                node_index: _,
            }) => {
                let was_in_try_statement = std::mem::replace(&mut self.in_try_statement, true);
                self.record_ambiguous_reachability();

                let exception_handlers = if handlers.is_empty() {
                    ExceptionHandlers::None
                } else if handlers.iter().any(|handler| {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    handler.type_.is_none()
                }) {
                    ExceptionHandlers::catch_all()
                } else {
                    ExceptionHandlers::propagating()
                };
                self.exception_context_stack_manager
                    .push_try_context(exception_handlers, !finalbody.is_empty());

                // Visit the `try` block!
                let try_block_declarations = self.visit_block_body(body);

                let mut post_except_states = vec![];

                // Take all checkpoints recorded immediately before operations in the `try` suite
                // that may raise. Keep the context itself on the stack so that terminal statements
                // in `except` and `else` suites can still be recorded as entries to the associated
                // `finally` suite.
                let try_block_snapshots = self.exception_context_stack_manager.end_try_suite();

                if !handlers.is_empty() {
                    // Save the state immediately *after* visiting the `try` block
                    // but *before* we prepare for visiting the `except` block(s).
                    //
                    // We will revert to this state prior to visiting the `else` block,
                    // as there necessarily must have been 0 `except` blocks executed
                    // if we hit the `else` block.
                    let post_try_block_state = self.flow_snapshot();

                    // Prepare for visiting the `except` block(s). If the `try` suite contained no
                    // exception checkpoints, its handlers are unreachable.
                    let mut try_block_snapshots = try_block_snapshots.into_iter();
                    if let Some(first_snapshot) = try_block_snapshots.next() {
                        self.flow_restore(first_snapshot);
                        for snapshot in try_block_snapshots {
                            self.flow_merge(snapshot);
                        }
                    } else {
                        self.flow_restore(post_try_block_state.clone());
                        self.mark_unreachable();
                    }

                    // basedpython: an exception leaves the `try` block from inside
                    // it, at a point where the block had not unbound its own
                    // declarations yet. A handler is a sibling block, so those names
                    // are out of scope in it either way.
                    self.unbind_block_declarations(&try_block_declarations);

                    let pre_except_state = self.flow_snapshot();
                    let num_handlers = handlers.len();

                    for (i, except_handler) in handlers.iter().enumerate() {
                        let ast::ExceptHandler::ExceptHandler(except_handler) = except_handler;
                        let ast::ExceptHandlerExceptHandler {
                            name: symbol_name,
                            type_: handled_exceptions,
                            body: handler_body,
                            range: _,
                            node_index: _,
                        } = except_handler;

                        if let Some(handled_exceptions) = handled_exceptions {
                            self.visit_expr(handled_exceptions);
                        }

                        // If `handled_exceptions` above was `None`, it's something like `except as e:`,
                        // which is invalid syntax. However, it's still pretty obvious here that the user
                        // *wanted* `e` to be bound, so we should still create a definition here nonetheless.
                        let symbol = if let Some(symbol_name) = symbol_name {
                            let symbol = self.add_symbol(symbol_name.id.clone());

                            self.add_definition(
                                symbol.into(),
                                DefinitionNodeRef::ExceptHandler(ExceptHandlerDefinitionNodeRef {
                                    handler: except_handler,
                                    is_star: *is_star,
                                }),
                            );
                            Some(symbol)
                        } else {
                            None
                        };

                        self.visit_block_body(handler_body);
                        // The caught exception is cleared at the end of the except clause
                        if let Some(symbol) = symbol {
                            self.delete_binding(symbol.into());
                        }
                        // Each `except` block is mutually exclusive with all other `except` blocks.
                        post_except_states.push(self.flow_snapshot());

                        // It's unnecessary to do the `self.flow_restore()` call for the final except handler,
                        // as we'll immediately call `self.flow_restore()` to a different state
                        // as soon as this loop over the handlers terminates.
                        if i < (num_handlers - 1) {
                            self.flow_restore(pre_except_state.clone());
                        }
                    }

                    // If we get to the `else` block, we know that 0 of the `except` blocks can have been executed,
                    // and the entire `try` block must have been executed:
                    self.flow_restore(post_try_block_state);
                }

                self.visit_block_body(orelse);

                for post_except_state in post_except_states {
                    self.flow_merge(post_except_state);
                }

                let normal_pre_finally_state = self.flow_snapshot();
                let (
                    terminal_finally_entry_snapshots,
                    has_escaping_exception,
                    has_deferred_terminal_context_manager_exit,
                ) = self
                    .exception_context_stack_manager
                    .pop_try_context()
                    .into_finally_entry_state();
                // TODO: there's lots of complexity here that isn't yet handled by our model.
                // In order to accurately model the semantics of `finally` suites, we in fact need to visit
                // the suite twice: once under the (current) assumption that either the `try + else` suite
                // ran to completion or exactly one `except` branch ran to completion, and then again under
                // the assumption that potentially none of the branches ran to completion and we in fact
                // jumped from a `try`, `else` or `except` branch straight into the `finally` branch.
                // This requires rethinking some fundamental assumptions semantic indexing makes.
                // For more details, see:
                // - https://astral-sh.notion.site/Exception-handler-control-flow-11348797e1ca80bb8ce1e9aedbbe439d
                // - https://github.com/astral-sh/ruff/pull/13633#discussion_r1788626702
                if normal_pre_finally_state.is_always_unreachable()
                    && !terminal_finally_entry_snapshots.is_empty()
                {
                    let mut snapshots = terminal_finally_entry_snapshots.into_iter();
                    let first_snapshot = snapshots.next().expect("checked non-empty snapshots");
                    self.flow_restore(first_snapshot);
                    for snapshot in snapshots {
                        self.flow_merge(snapshot);
                    }
                    self.visit_block_body(finalbody);
                    if !self.flow_snapshot().is_always_unreachable() {
                        if !finalbody.is_empty() && has_escaping_exception {
                            self.record_exception_checkpoint();
                        }
                        self.record_terminal_finally_entry();
                    }
                    self.mark_unreachable();
                } else {
                    let mut post_finally_terminal_predicate = None;
                    let mut terminal_snapshots = terminal_finally_entry_snapshots.into_iter();
                    if has_deferred_terminal_context_manager_exit
                        && let Some(snapshot) = terminal_snapshots.next()
                    {
                        let continuation = self.current_use_def_map().reachability;
                        self.current_reachability_constraints_mut()
                            .mark_used(continuation);
                        let predicate_id =
                            self.add_predicate(PredicateOrLiteral::Predicate(Predicate {
                                node: PredicateNode::FinallyNormalPathImpossible {
                                    scope: self.current_scope_id(),
                                    continuation,
                                },
                                is_positive: true,
                            }));

                        self.flow_restore(snapshot);
                        for snapshot in terminal_snapshots {
                            self.flow_merge(snapshot);
                        }

                        let reachability_constraint = self
                            .current_reachability_constraints_mut()
                            .add_atom(predicate_id);
                        let narrowing_constraint = self
                            .current_use_def_map_mut()
                            .narrowing_constraints
                            .add_atom(predicate_id);
                        self.current_use_def_map_mut()
                            .record_non_terminal_call_constraints(
                                reachability_constraint,
                                narrowing_constraint,
                            );

                        if finalbody.is_empty() {
                            let terminal_snapshot = self.flow_snapshot();
                            self.flow_restore(normal_pre_finally_state);
                            self.exception_context_stack_manager
                                .propagate_deferred_terminal_context_manager_exit(
                                    terminal_snapshot,
                                );
                        } else {
                            self.flow_merge(normal_pre_finally_state);
                            post_finally_terminal_predicate = Some(predicate_id);
                        }
                    }
                    // Mixed normal and terminal entry states are still handled by the normal path
                    // only. See the corresponding TODO tests in `terminal_statements.md`.
                    self.visit_block_body(finalbody);
                    if !finalbody.is_empty()
                        && has_escaping_exception
                        && self.current_use_def_map().reachability
                            != ScopedReachabilityConstraintId::ALWAYS_FALSE
                    {
                        self.record_exception_checkpoint();
                    }

                    if let Some(predicate_id) = post_finally_terminal_predicate
                        && self.current_use_def_map().reachability
                            != ScopedReachabilityConstraintId::ALWAYS_FALSE
                    {
                        let post_finally_state = self.flow_snapshot();
                        let terminal_reachability = self
                            .current_reachability_constraints_mut()
                            .add_atom(predicate_id);
                        let terminal_narrowing = self
                            .current_use_def_map_mut()
                            .narrowing_constraints
                            .add_atom(predicate_id);
                        self.current_use_def_map_mut()
                            .record_non_terminal_call_constraints(
                                terminal_reachability,
                                terminal_narrowing,
                            );
                        let terminal_snapshot = self.flow_snapshot();
                        self.flow_restore(post_finally_state);
                        self.exception_context_stack_manager
                            .propagate_deferred_terminal_context_manager_exit(terminal_snapshot);

                        let normal_reachability = self
                            .current_reachability_constraints_mut()
                            .add_not_constraint(terminal_reachability);
                        let normal_narrowing = self
                            .current_use_def_map_mut()
                            .narrowing_constraints
                            .add_negated_atom(predicate_id);
                        self.current_use_def_map_mut()
                            .record_non_terminal_call_constraints(
                                normal_reachability,
                                normal_narrowing,
                            );
                    }
                }
                self.in_try_statement = was_in_try_statement;
            }

            ast::Stmt::Raise(_) => {
                walk_stmt(self, stmt);
                self.record_exception_checkpoint();
                self.record_terminal_finally_entry();
                // Everything in the current block after a terminal statement is unreachable.
                self.mark_unreachable();
            }

            ast::Stmt::Return(_) => {
                let recovers_from_body = self.enclosing_function_wrote_down_no_return_type();
                if let ast::Stmt::Return(ast::StmtReturn {
                    value: Some(value), ..
                }) = stmt
                    && recovers_from_body
                {
                    // basedpython: a returned expression says more than its own type does —
                    // `return a is int` tells every caller what a truthy result means about the
                    // argument. Reading that is the narrowing machinery's job, and it evaluates
                    // a predicate over a standalone expression, so record one for it
                    self.add_standalone_expression(value);
                }
                walk_stmt(self, stmt);
                // and what narrowing established about the members of a returned place is part of
                // what is handed back. Nothing between the walk of the value and here changes any
                // binding, so this is still the state the `return` sees
                if let ast::Stmt::Return(ast::StmtReturn {
                    value: Some(value), ..
                }) = stmt
                    && recovers_from_body
                {
                    self.record_returned_place_members(value);
                }
                self.record_terminal_finally_entry();
                // Everything in the current block after a terminal statement is unreachable.
                self.mark_unreachable();
            }

            ast::Stmt::Continue(_) | ast::Stmt::Break(_) => {
                // the value is evaluated before control leaves the loop, so it is
                // visited before the break's flow effect is recorded
                if let ast::Stmt::Break(ast::StmtBreak {
                    value: Some(value), ..
                }) = stmt
                {
                    self.check_break_value(stmt, value);
                    self.visit_expr(value);
                }
                if self
                    .exception_context_stack_manager
                    .has_context_manager_exception_checkpoint()
                {
                    self.record_ambiguous_reachability();
                }
                self.unbind_blocks_left_by_jump();
                let snapshot = self.flow_snapshot();
                if let Some(current_loop) = self.current_loop_mut() {
                    if stmt.is_continue_stmt() {
                        current_loop.continue_states.push(snapshot);
                    } else {
                        current_loop.break_states.push(snapshot);
                    }
                }
                self.record_terminal_finally_entry();
                // Everything in the current block after a terminal statement is unreachable.
                self.mark_unreachable();
            }
            ast::Stmt::Global(ast::StmtGlobal {
                range,
                node_index: _,
                names,
            }) => {
                for name in names {
                    self.scopes_by_expression
                        .record_expression(name, self.current_scope());
                    let symbol_id = self.add_symbol(name.id.clone());
                    let symbol = self.current_place_table().symbol(symbol_id);
                    // Check whether the variable has already been accessed in this scope.
                    if (symbol.is_bound() || symbol.is_declared() || symbol.is_used())
                        && !symbol.is_parameter()
                    {
                        self.report_semantic_error(SemanticSyntaxError {
                            kind: SemanticSyntaxErrorKind::LoadBeforeGlobalDeclaration {
                                name: name.to_string(),
                                start: name.range.start(),
                            },
                            range: name.range,
                            python_version: self.python_version(),
                        });
                    }
                    // Check whether the variable has also been declared nonlocal.
                    if symbol.is_nonlocal() {
                        self.report_semantic_error(SemanticSyntaxError {
                            kind: SemanticSyntaxErrorKind::NonlocalAndGlobal(name.to_string()),
                            range: name.range,
                            python_version: self.python_version(),
                        });
                        // Never mark a symbol both global and nonlocal, even in this error case.
                        continue;
                    }
                    // Check whether this is the module scope, where `global` has no effect.
                    let scope_id = self.current_scope();
                    if scope_id.is_global() {
                        // It's important that we don't `mark_global` here, because we error on
                        // type annotations on places that are marked global, but it's actually
                        // legal to write `global x; x: int = 42` at the module level.
                        continue;
                    }
                    // Assuming none of the rules above are violated, repeated `global`
                    // declarations are allowed and ignored.
                    if symbol.is_global() {
                        continue;
                    }
                    self.current_place_table_mut()
                        .symbol_mut(symbol_id)
                        .mark_global();
                    self.current_scope_info_mut()
                        .this_scope_global_or_nonlocal_declarations
                        .insert(name.id.clone(), *range);
                }
                walk_stmt(self, stmt);
            }
            ast::Stmt::Nonlocal(ast::StmtNonlocal {
                range,
                node_index: _,
                names,
            }) => {
                for name in names {
                    self.scopes_by_expression
                        .record_expression(name, self.current_scope());
                    let symbol_id = self.add_symbol(name.id.clone());
                    let symbol = self.current_place_table().symbol(symbol_id);
                    // Check whether the variable has already been accessed in this scope.
                    if (symbol.is_bound() || symbol.is_declared() || symbol.is_used())
                        && !symbol.is_parameter()
                    {
                        self.report_semantic_error(SemanticSyntaxError {
                            kind: SemanticSyntaxErrorKind::LoadBeforeNonlocalDeclaration {
                                name: name.to_string(),
                                start: name.range.start(),
                            },
                            range: name.range,
                            python_version: self.python_version(),
                        });
                    }
                    // Check whether the variable has also been declared global.
                    if symbol.is_global() {
                        self.report_semantic_error(SemanticSyntaxError {
                            kind: SemanticSyntaxErrorKind::NonlocalAndGlobal(name.to_string()),
                            range: name.range,
                            python_version: self.python_version(),
                        });
                        // Never mark a symbol both global and nonlocal, even in this error case.
                        continue;
                    }
                    // Check whether this is the module scope, where `nonlocal` isn't allowed.
                    let scope_id = self.current_scope();
                    if scope_id.is_global() {
                        // The SemanticSyntaxChecker will report an error for this.
                        continue;
                    }
                    // Assuming none of the rules above are violated, repeated `nonlocal`
                    // declarations are allowed and ignored.
                    if symbol.is_nonlocal() {
                        continue;
                    }
                    self.current_place_table_mut()
                        .symbol_mut(symbol_id)
                        .mark_nonlocal();
                    self.current_scope_info_mut()
                        .this_scope_global_or_nonlocal_declarations
                        .insert(name.id.clone(), *range);
                }
                walk_stmt(self, stmt);
            }
            ast::Stmt::Delete(ast::StmtDelete {
                targets,
                range: _,
                node_index: _,
            }) => {
                // We will check the target expressions and then delete them.
                walk_stmt(self, stmt);
                for target in targets {
                    if let Some(mut target) = PlaceExpr::try_from_expr(target) {
                        if let PlaceExpr::Symbol(symbol) = &mut target {
                            // `del x` behaves like an assignment in that it forces all references
                            // to `x` in the current scope (including *prior* references) to refer
                            // to the current scope's binding (unless `x` is declared `global` or
                            // `nonlocal`). For example, this is an UnboundLocalError at runtime:
                            //
                            // ```py
                            // x = 1
                            // def foo():
                            //     print(x)  # can't refer to global `x`
                            //     if False:
                            //         del x
                            // foo()
                            // ```
                            symbol.mark_bound();
                            symbol.mark_used();
                        }

                        let place_id = self.add_place(target);
                        self.invalidate_narrowing_aliases_for(place_id);
                        self.delete_binding(place_id);
                    }
                }
            }
            ast::Stmt::Expr(ast::StmtExpr {
                value,
                range: _,
                node_index: _,
            }) => {
                if self.in_module_scope() {
                    if let Some(expr) = dunder_all_extend_argument(value) {
                        self.add_standalone_expression(expr);
                    }
                }

                self.visit_expr(value);

                // basedpython `<value> cast <type>` / `<value> cast! <type>` as a bare
                // statement narrows the value place to the target type for the rest of
                // the scope, like an unconditional `assert isinstance(value, type)`.
                // `cast?` is left out: it yields `None` rather than asserting anything.
                // The synthetic `cast` callee is unresolved and never `NoReturn`, so the
                // terminal call analysis below is skipped for it.
                if let ast::Expr::Call(call) = value.as_ref()
                    && matches!(
                        call.cast_kind,
                        Some(ast::CastKind::Static | ast::CastKind::Checked)
                    )
                {
                    let predicate = self.build_predicate(value, ExpressionContext::Value);
                    self.record_narrowing_constraint(predicate);
                    return;
                }

                // If the statement is a call (or an `await` wrapping a call), it could
                // possibly be a call to a function marked with `NoReturn` (for example,
                // `sys.exit()` or `await async_exit()`). In this case, we use a special
                // kind of constraint to mark the following code as unreachable.
                //
                // Ideally, these constraints should be added for every call expression, even those in
                // sub-expressions. But doing so makes the number of such constraints so high that
                // it significantly degrades performance. We thus cut scope here and add these
                // constraints only at statement-level function calls, like `sys.exit()`, and not
                // within sub-expressions like `3 + sys.exit()` etc.
                let call_info = match value.as_ref() {
                    ast::Expr::Call(ast::ExprCall { func, .. }) => {
                        Some((func.as_ref(), value.as_ref(), false))
                    }
                    ast::Expr::Await(ast::ExprAwait { value: inner, .. }) => match inner.as_ref() {
                        ast::Expr::Call(ast::ExprCall { func, .. }) => {
                            Some((func.as_ref(), value.as_ref(), true))
                        }
                        _ => None,
                    },
                    _ => None,
                };

                if let Some((func, expr, is_await)) = call_info {
                    // Avoid creating reachability nodes for calls on fluid specialization
                    // candidates. Without this short-circuit, performing reachability analysis
                    // can lead to quadratic blowup of cycle dependencies during full-scope
                    // fluid specialization inference, as Salsa flattens the dependencies of all
                    // cycle participants, and the reachability analysis of a given use of the
                    // candidate may create dependencies on all previous uses, leading to
                    // significant performance regressions.
                    //
                    // Note that built-in collection types do not have methods that explicitly
                    // return `Never`, so this rarely has a meaningful semantic impact.
                    //
                    // basedpython: the fluid short-circuit is about reachability only. An
                    // assertion guard called on such a receiver (`a = A(); a.f()`) still has
                    // to narrow, so its predicate is recorded either way.
                    let is_terminal_call_candidate = func
                        .as_attribute_expr()
                        .and_then(|attribute| self.fluid_candidate_binding(&attribute.value))
                        .is_none();
                    let is_guard_call_candidate = self.source_type.is_basedpython();

                    if !self.source_type.is_stub()
                        && (is_terminal_call_candidate || is_guard_call_candidate)
                    {
                        let callable =
                            self.add_standalone_expression_impl(func, ExpressionKind::Callee, None);
                        let call_expr = self.add_standalone_expression(expr);

                        if is_terminal_call_candidate {
                            let predicate = Predicate {
                                node: PredicateNode::IsNonTerminalCall(CallableAndCallExpr {
                                    callable,
                                    call_expr,
                                    is_await,
                                }),
                                is_positive: true,
                            };

                            let predicate_id =
                                self.add_predicate(PredicateOrLiteral::Predicate(predicate));
                            let narrowing_constraint = self
                                .current_use_def_map_mut()
                                .narrowing_constraints
                                .add_atom(predicate_id);

                            let reachability_constraint = self
                                .current_reachability_constraints_mut()
                                .add_atom(predicate_id);
                            self.current_use_def_map_mut()
                                .record_non_terminal_call_constraints(
                                    reachability_constraint,
                                    narrowing_constraint,
                                );
                        }

                        // basedpython: the same call may be a call to an assertion guard
                        // (`def f(x) -> asserts x`), which narrows once it returns — that is,
                        // for the rest of this flow rather than inside a branch
                        if is_guard_call_candidate {
                            // record the call itself, which is what a checker sees; `expr`
                            // is the `await` for an awaited call
                            if let Some(call) = asserted_call(expr) {
                                self.basedpython_statement_calls
                                    .insert(ExpressionNodeKey::from(call));
                            }
                            self.record_narrowing_constraint(PredicateOrLiteral::Predicate(
                                Predicate {
                                    node: PredicateNode::AssertsCall(CallableAndCallExpr {
                                        callable,
                                        call_expr,
                                        is_await,
                                    }),
                                    is_positive: true,
                                },
                            ));
                        }
                    }
                }
            }
            _ => {
                walk_stmt(self, stmt);
            }
        }
    }
}

impl<'ast> Visitor<'ast> for SemanticIndexBuilder<'_, 'ast> {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        let is_loop = matches!(stmt, ast::Stmt::For(_) | ast::Stmt::While(_));
        if is_loop {
            self.loop_ranges.push(stmt.range());
        }
        self.push_statement(CurrentStatement::default());
        self.visit_stmt_impl(stmt);
        let current_statement = self.pop_statement();
        if is_loop {
            self.loop_ranges.pop();
        }

        if current_statement.lambda_expressions.is_empty()
            && current_statement.fluid_uses.is_empty()
        {
            return;
        }

        // Classify how each fluid-candidate use in this statement interacts with the
        // candidate's specialization. Constraints can only be read back from the
        // inference of simple (non-compound) statements, so constraint-bearing roles
        // inside compound statement headers are downgraded.
        let classified: Vec<(Definition<'_>, FluidUse<'_>)> = current_statement
            .fluid_uses
            .into_iter()
            .map(|(candidate_def, use_expression, range, loops)| {
                let (mut role, discarded_call_result) = classify_fluid_use(stmt, use_expression);
                if role.contributes_constraints() && !is_simple_statement(stmt) {
                    role = match role {
                        // A method call in a compound statement header (e.g. `if a.pop():`)
                        // cannot be read back for constraints; treat it as an opaque use.
                        FluidUseRole::MethodReceiver | FluidUseRole::SubscriptStore => {
                            FluidUseRole::Escape
                        }
                        FluidUseRole::TypeContextual => FluidUseRole::Escape,
                        role => role,
                    };
                }
                (
                    candidate_def,
                    FluidUse {
                        use_expression,
                        range,
                        role,
                        discarded_call_result,
                        statement_range: stmt.range(),
                        loops,
                        statement: None,
                    },
                )
            })
            .collect();

        let needs_standalone_statement = !current_statement.lambda_expressions.is_empty()
            || classified
                .iter()
                .any(|(_, fluid_use)| fluid_use.role.contributes_constraints());

        if !needs_standalone_statement {
            for (candidate_def, fluid_use) in classified {
                self.fluid_candidates_by_use
                    .insert(fluid_use.use_expression, candidate_def);
                self.fluid_uses_by_candidate
                    .entry(candidate_def)
                    .or_default()
                    .push(fluid_use);
            }
            return;
        }

        let standalone_statement = self.add_standalone_statement(stmt);

        // The body of a lambda expression needs access to the `Callable` type
        // context the lambda is being inferred with, and so any statement
        // containing a lambda must be inferable as a standalone statement
        // to avoid large scope-level cycles.
        self.enclosing_lambda_statements.extend(
            current_statement
                .lambda_expressions
                .into_iter()
                .map(|lambda| (lambda.into(), standalone_statement)),
        );

        // The inferred specialization of a fluid candidate depends on uses of
        // the candidate in its containing scope, and so each constraining use must be
        // part of a standalone inferable statement to avoid large scope-level cycles.
        for (candidate_def, mut fluid_use) in classified {
            if fluid_use.role.contributes_constraints() {
                fluid_use.statement = Some(standalone_statement);
            }

            self.fluid_candidates_by_use
                .insert(fluid_use.use_expression, candidate_def);
            self.fluid_uses_by_candidate
                .entry(candidate_def)
                .or_default()
                .push(fluid_use);
        }
    }

    fn visit_keyword(&mut self, keyword: &'ast ast::Keyword) {
        walk_keyword(self, keyword);

        if keyword.arg.is_some() {
            return;
        }

        // Record a use of all members of `x` for a splatted keyword argument `**x`.
        let current_scope = self.current_scope();
        let member_places = PlaceExpr::try_from_expr(&keyword.value)
            .and_then(|value_place_expr| {
                self.current_place_table()
                    .place_id((&value_place_expr).into())
            })
            .map(|value_place_id| {
                let place_table = &self.place_tables[current_scope];
                place_table
                    .associated_place_ids(value_place_id)
                    .iter()
                    .filter(move |key_member_id| {
                        let key_member_expr = place_table.member(**key_member_id).expression();

                        // Only include top-level keys.
                        let Some(key_parent) = key_member_expr.as_ref().parent() else {
                            return true;
                        };
                        match place_table.place(value_place_id) {
                            PlaceExprRef::Symbol(_) => false,
                            PlaceExprRef::Member(value_member) => {
                                key_parent == value_member.expression()
                            }
                        }
                    })
                    .map(|key_member_id| ScopedPlaceId::from(*key_member_id))
            });

        let use_id = self.ast_ids[current_scope].record_use(keyword);
        self.use_def_maps[current_scope]
            .record_multi_use(member_places.into_iter().flatten(), use_id);
    }

    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        // Generic AST walking evaluates child expressions as values. Short-circuit syntax
        // propagates condition context explicitly through `visit_expr_with_context`.
        self.visit_expr_with_context(expr, ExpressionContext::Value);
    }

    fn visit_parameters(&mut self, parameters: &'ast ast::Parameters) {
        // Intentionally avoid walking default expressions, as we handle them in the enclosing
        // scope.
        for parameter in parameters.iter().map(ast::AnyParameterRef::as_parameter) {
            self.visit_parameter(parameter);
        }
    }

    fn visit_parameter(&mut self, parameter: &'ast ast::Parameter) {
        // Only the annotation belongs to this scope. basedpython: a destructuring
        // parameter's pattern binds in the function's body scope, where
        // `declare_parameter` visits it
        if let Some(annotation) = &parameter.annotation {
            self.visit_annotation(annotation);
        }
    }

    fn visit_pattern(&mut self, pattern: &'ast ast::Pattern) {
        if let ast::Pattern::MatchOr(ast::PatternMatchOr { patterns, .. }) = pattern
            && let Some((last, alternatives)) = patterns.split_last()
            && (
                // Capture-free alternatives do not affect bindings and need no flow merge.
                patterns.iter().any(Self::pattern_has_bindings)
            )
        {
            // Start each alternative without earlier captures so repeated names do not shadow one
            // another. Complementary predicates preserve possible missing captures while all
            // alternatives together recover the incoming reachability.
            let mut successful_alternatives = None;
            for alternative in alternatives {
                let remaining_alternatives = self.flow_snapshot();
                let selected_alternative =
                    self.record_reachability_constraint(PredicateOrLiteral::Predicate(Predicate {
                        node: PredicateNode::OrPatternAlternative(self.current_scope_id()),
                        is_positive: true,
                    }));
                self.visit_pattern(alternative);
                if let Some(previous_alternatives) = successful_alternatives.take() {
                    self.flow_merge(previous_alternatives);
                }
                successful_alternatives = Some(self.flow_snapshot());
                self.flow_restore(remaining_alternatives);
                self.record_negated_reachability_constraint(selected_alternative);
            }

            self.visit_pattern(last);
            if let Some(successful_alternative) = successful_alternatives {
                self.flow_merge(successful_alternative);
            }
            return;
        }

        if let ast::Pattern::MatchStar(ast::PatternMatchStar {
            name: Some(name),
            range: _,
            node_index: _,
        }) = pattern
        {
            let symbol = self.add_symbol(name.id().clone());
            let state = self.current_match_case.as_ref().unwrap();
            self.add_definition(
                symbol.into(),
                MatchPatternDefinitionNodeRef {
                    pattern: state.pattern,
                    identifier: name,
                    predicate: state.predicate,
                    is_case_name: false,
                },
            );
        }

        walk_pattern(self, pattern);

        if let ast::Pattern::MatchAs(ast::PatternMatchAs {
            name: Some(name), ..
        })
        | ast::Pattern::MatchMapping(ast::PatternMatchMapping {
            rest: Some(name), ..
        }) = pattern
        {
            // A capture's own scope has to be recorded rather than left to the
            // interval map to infer. That map merges consecutive same-scope
            // entries into ranges, so a node with no entry of its own is only
            // answered for when some *recorded* expression in the same scope sits
            // on either side of it in node order. A `match` subject happens to sit
            // before its patterns and so covered them by accident; basedpython's
            // `let (a, b) := v` and `if let P := v` write the pattern *before* the
            // value, and a `let (m, n): T` parameter binder has no neighbouring
            // expression at all, so both fell outside every interval and every
            // service that starts by asking which scope a name is in — goto,
            // find-references, rename, highlight — answered nothing at the binder
            self.scopes_by_expression
                .record_expression(name, self.current_scope());
            let symbol = self.add_symbol(name.id().clone());
            let state = self.current_match_case.as_ref().unwrap();
            let (pattern_node, predicate, case_name) = (
                state.pattern,
                state.predicate,
                state.case_name(name).cloned(),
            );
            let node_ref = MatchPatternDefinitionNodeRef {
                pattern: pattern_node,
                identifier: name,
                predicate,
                is_case_name: case_name.is_some(),
            };
            if let Some(case_name) = case_name {
                // basedpython: the name binds only where it turned out not to be
                // an enum member of the subject, which is a question for type
                // checking — so the binding is recorded as the branch it is:
                //
                // ```py
                // if <the name is a capture>:
                //     A = <subject>
                // ```
                let capture = CaseNameCapturePredicate::new(self.db, case_name);
                let no_capture = self.flow_snapshot();
                let constraint = self.record_reachability_constraint(capture.into());
                self.add_definition(symbol.into(), node_ref);
                let captured = self.flow_snapshot();
                self.flow_restore(no_capture);
                self.record_negated_reachability_constraint(constraint);
                self.flow_merge(captured);
            } else {
                self.add_definition(symbol.into(), node_ref);
            }
        }
    }
}

impl SemanticSyntaxContext for SemanticIndexBuilder<'_, '_> {
    fn future_annotations_or_stub(&self) -> bool {
        self.has_future_annotations
    }

    fn lazy_import_context(&self) -> Option<LazyImportContext> {
        match self.scopes[self.current_scope()].kind() {
            // Possible, but invalid positions.
            ScopeKind::Function => return Some(LazyImportContext::Function),
            ScopeKind::Class => return Some(LazyImportContext::Class),
            // Valid position.
            ScopeKind::Module => {}
            // Impossible positions because lambdas and comprehensions can't contain statements.
            ScopeKind::Comprehension
            | ScopeKind::Lambda
            | ScopeKind::TypeAlias
            | ScopeKind::TypeParams => {}
        }

        if self.in_try_statement {
            return Some(LazyImportContext::TryExceptBlocks);
        }

        None
    }

    fn python_version(&self) -> PythonVersion {
        self.python_version
    }

    fn source(&self) -> &str {
        self.source_text().as_str()
    }

    // We handle the one syntax error that relies on this method (`LoadBeforeGlobalDeclaration`)
    // directly in `visit_stmt`, so this just returns a placeholder value.
    fn global(&self, _name: &str) -> Option<TextRange> {
        None
    }

    // We handle the one syntax error that relies on this method (`NonlocalWithoutBinding`) directly
    // in `TypeInferenceBuilder::infer_nonlocal_statement`, so this just returns `true`.
    fn has_nonlocal_binding(&self, _name: &str) -> bool {
        true
    }

    fn in_async_context(&self) -> bool {
        for scope_info in self.scope_stack.iter().rev() {
            let scope = &self.scopes[scope_info.file_scope_id];
            match scope.kind() {
                ScopeKind::Class | ScopeKind::Lambda => return false,
                ScopeKind::Function => {
                    return scope.node().expect_function().node(self.module).is_async;
                }
                ScopeKind::Comprehension
                | ScopeKind::Module
                | ScopeKind::TypeAlias
                | ScopeKind::TypeParams => {}
            }
        }
        false
    }

    fn in_await_allowed_context(&self) -> bool {
        for scope_info in self.scope_stack.iter().rev() {
            let scope = &self.scopes[scope_info.file_scope_id];
            match scope.kind() {
                ScopeKind::Class => return false,
                ScopeKind::Function | ScopeKind::Lambda => return true,
                ScopeKind::Comprehension
                    if matches!(scope.node(), NodeWithScopeKind::GeneratorExpression(_)) =>
                {
                    return true;
                }
                ScopeKind::Comprehension
                | ScopeKind::Module
                | ScopeKind::TypeAlias
                | ScopeKind::TypeParams => {}
            }
        }
        false
    }

    fn in_yield_allowed_context(&self) -> bool {
        for scope_info in self.scope_stack.iter().rev() {
            let scope = &self.scopes[scope_info.file_scope_id];
            match scope.kind() {
                ScopeKind::Class | ScopeKind::Comprehension => return false,
                ScopeKind::Function | ScopeKind::Lambda => return true,
                ScopeKind::Module | ScopeKind::TypeAlias | ScopeKind::TypeParams => {}
            }
        }
        false
    }

    fn in_sync_comprehension(&self) -> bool {
        for scope_info in self.scope_stack.iter().rev() {
            let scope = &self.scopes[scope_info.file_scope_id];
            let generators = match scope.node() {
                NodeWithScopeKind::ListComprehension(node) => &node.node(self.module).generators,
                NodeWithScopeKind::SetComprehension(node) => &node.node(self.module).generators,
                NodeWithScopeKind::DictComprehension(node) => &node.node(self.module).generators,
                _ => continue,
            };
            if generators
                .iter()
                .all(|comprehension| !comprehension.is_async)
            {
                return true;
            }
        }
        false
    }

    fn in_class_body_comprehension(&self) -> bool {
        for scope_info in self.scope_stack.iter().rev() {
            match self.scopes[scope_info.file_scope_id].kind() {
                ScopeKind::Comprehension => {}
                ScopeKind::Class => return true,
                ScopeKind::Module
                | ScopeKind::TypeParams
                | ScopeKind::Function
                | ScopeKind::Lambda
                | ScopeKind::TypeAlias => return false,
            }
        }
        false
    }

    fn in_module_scope(&self) -> bool {
        self.scope_stack.len() == 1
    }

    fn in_function_scope(&self) -> bool {
        let kind = self.scopes[self.current_scope()].kind();
        matches!(kind, ScopeKind::Function | ScopeKind::Lambda)
    }

    fn def_is_method(&self) -> bool {
        // a function definition is checked as the statement is visited, so a class
        // body being the current scope is what makes the `def` a method of it
        self.scopes[self.current_scope()].kind() == ScopeKind::Class
    }

    fn in_generator_context(&self) -> bool {
        for scope_info in &self.scope_stack {
            let scope = &self.scopes[scope_info.file_scope_id];
            if matches!(scope.node(), NodeWithScopeKind::GeneratorExpression(_)) {
                return true;
            }
        }
        false
    }

    fn in_notebook(&self) -> bool {
        self.source_text().is_notebook()
    }

    fn report_semantic_error(&self, error: SemanticSyntaxError) {
        // TODO(brent) The long-term fix for this is for `YieldOutsideFunction` not to apply to
        // `await` at all and only to emit the more specific `AwaitOutsideAsyncFunction` instead.
        // However, to preserve backwards compatibility with the corresponding Ruff rules, we
        // temporarily filter out the diagnostic for ty instead.
        if matches!(
            error.kind,
            SemanticSyntaxErrorKind::YieldOutsideFunction(YieldOutsideFunctionKind::Await)
        ) {
            return;
        }

        if self.db.should_check_file(self.file.file(self.db)) {
            self.semantic_syntax_errors.borrow_mut().push(error);
        }
    }

    fn in_loop_context(&self) -> bool {
        self.current_scope_info().current_loop.is_some()
    }

    fn is_bound_parameter(&self, name: &str) -> bool {
        self.scopes[self.current_scope()]
            .node()
            .as_function()
            .is_some_and(|func| func.node(self.module).parameters.includes(name))
    }

    fn is_basedpython(&self) -> bool {
        self.source_type.is_basedpython()
    }
}

/// A simple-name annotated assignment with an RHS whose declaration is already recorded.
/// Created only by `begin_annotated_assignment`; finishing it records the value binding.
#[derive(Copy, Clone, Debug, PartialEq)]
struct PendingAnnotatedAssignment<'db> {
    definition: Definition<'db>,
}

#[derive(Copy, Clone, Debug, PartialEq)]
enum CurrentAssignment<'ast, 'db> {
    Assign {
        node: &'ast ast::StmtAssign,
        unpack: Option<Unpack<'db>>,
        owner: BindingsOwner,
    },
    AnnAssign {
        node: &'ast ast::StmtAnnAssign,
        pending: Option<PendingAnnotatedAssignment<'db>>,
    },
    AugAssign(&'ast ast::StmtAugAssign),
    For {
        node: &'ast ast::StmtFor,
        unpack: Option<(UnpackPosition, Unpack<'db>)>,
    },
    Named(&'ast ast::ExprNamed),
    Comprehension {
        node: &'ast ast::Comprehension,
        first: bool,
        unpack: Option<(UnpackPosition, Unpack<'db>)>,
    },
    WithItem {
        item: &'ast ast::WithItem,
        is_async: bool,
        unpack: Option<(UnpackPosition, Unpack<'db>)>,
    },
}

impl CurrentAssignment<'_, '_> {
    fn unpack_position_mut(&mut self) -> Option<&mut UnpackPosition> {
        match self {
            Self::For { unpack, .. }
            | Self::WithItem { unpack, .. }
            | Self::Comprehension { unpack, .. } => unpack.as_mut().map(|(position, _)| position),
            Self::Assign { .. } | Self::AnnAssign { .. } | Self::AugAssign(_) | Self::Named(_) => {
                None
            }
        }
    }
}

#[derive(Default)]
struct CurrentStatement<'ast, 'db> {
    /// A list of lambda expressions contained in this statement.
    lambda_expressions: Vec<&'ast ast::ExprLambda>,
    /// A list of fluid candidate definitions whose uses are contained in this statement,
    /// together with the use expression, its range, and its enclosing loop ranges.
    fluid_uses: Vec<(
        Definition<'db>,
        ExpressionNodeKey,
        TextRange,
        Box<[TextRange]>,
    )>,
}

/// basedpython: whether `value` is a [trailing lambda block] standing as a
/// statement's value. Such a value may not be made a standalone expression: the
/// block's suite defines a function, and an expression region cannot own a
/// definition. The parser restricts the block to the assignment shapes whose
/// value is inferred with the target's own definition instead.
///
/// [trailing lambda block]: ast::ExprStatement::trailing_lambda
fn is_trailing_lambda_value(value: &ast::Expr) -> bool {
    value
        .as_statement_expr()
        .is_some_and(ast::ExprStatement::is_trailing_lambda)
}

/// Whether constraints learned at a fluid-candidate use in this statement can be read
/// back from the statement's standalone inference. Compound statements are excluded
/// because inferring them as a standalone unit would re-infer their entire body.
fn is_simple_statement(stmt: &ast::Stmt) -> bool {
    matches!(
        stmt,
        ast::Stmt::Expr(_)
            | ast::Stmt::Assign(_)
            | ast::Stmt::AnnAssign(_)
            | ast::Stmt::AugAssign(_)
            | ast::Stmt::Return(_)
    )
}

/// Classify how a use of a fluid specialization candidate interacts with the candidate's
/// specialization, based on the syntactic position of the use within its statement.
fn classify_fluid_use(stmt: &ast::Stmt, use_expression: ExpressionNodeKey) -> (FluidUseRole, bool) {
    // The role of the use when it is one of the statement's direct sub-expressions.
    let direct_role = |expr: &ast::Expr| match stmt {
        // A bare expression statement reads the value and discards it.
        ast::Stmt::Expr(_) => FluidUseRole::Read,
        // Return values and annotated assignments are inferred with the declared type
        // as bidirectional type context.
        ast::Stmt::Return(_) | ast::Stmt::AnnAssign(_) => FluidUseRole::TypeContextual,
        // Truthiness tests and iteration cannot constrain or leak the specialization.
        ast::Stmt::If(ast::StmtIf { test, .. })
        | ast::Stmt::While(ast::StmtWhile { test, .. })
        | ast::Stmt::Assert(ast::StmtAssert { test, .. })
            if ExpressionNodeKey::from(test.as_ref()) == ExpressionNodeKey::from(expr) =>
        {
            FluidUseRole::Read
        }
        ast::Stmt::For(ast::StmtFor { iter, .. })
            if ExpressionNodeKey::from(iter.as_ref()) == ExpressionNodeKey::from(expr) =>
        {
            FluidUseRole::Read
        }
        _ => FluidUseRole::Escape,
    };

    let mut classifier = FluidUseClassifier {
        use_expression,
        stack: Vec::new(),
        result: None,
    };
    classifier.visit_stmt_header(stmt);

    let (role, call_is_root) = classifier
        .result
        .map_or((FluidUseRole::Escape, false), |found| match found {
            FoundFluidUse::Direct(expr) => (direct_role(expr), false),
            FoundFluidUse::Nested { role, call_is_root } => (role, call_is_root),
        });

    // The target of an augmented subscript assignment (`a[k] += v`) both reads and
    // writes; its constraints are not recorded, so treat it as an opaque use.
    if role == FluidUseRole::SubscriptStore && stmt.is_aug_assign_stmt() {
        return (FluidUseRole::Escape, false);
    }

    // An expression statement discards its value: no observer of the call's result
    // survives the call.
    let discarded_call_result =
        role == FluidUseRole::TypeContextual && call_is_root && stmt.is_expr_stmt();

    (role, discarded_call_result)
}

enum FoundFluidUse<'ast> {
    /// The use is a direct sub-expression of the statement.
    Direct(&'ast ast::Expr),
    /// The use is nested within an expression; its role was derived from its parents.
    Nested {
        role: FluidUseRole,
        /// Whether the use is an argument of a call that is the statement's root
        /// expression — if the statement discards the call's result, no observer of
        /// the result survives the call.
        call_is_root: bool,
    },
}

struct FluidUseClassifier<'ast> {
    use_expression: ExpressionNodeKey,
    stack: Vec<&'ast ast::Expr>,
    result: Option<FoundFluidUse<'ast>>,
}

impl<'ast> FluidUseClassifier<'ast> {
    /// Visit the statement's direct sub-expressions, without descending into the bodies
    /// of compound statements: uses there are recorded against the nested statements.
    fn visit_stmt_header(&mut self, stmt: &'ast ast::Stmt) {
        match stmt {
            ast::Stmt::Expr(node) => self.visit_expr(&node.value),
            ast::Stmt::Return(node) => {
                if let Some(value) = &node.value {
                    self.visit_expr(value);
                }
            }
            ast::Stmt::Assign(node) => {
                for target in &node.targets {
                    self.visit_expr(target);
                }
                self.visit_expr(&node.value);
            }
            ast::Stmt::AnnAssign(node) => {
                self.visit_expr(&node.target);
                if let Some(value) = &node.value {
                    self.visit_expr(value);
                }
            }
            ast::Stmt::AugAssign(node) => {
                self.visit_expr(&node.target);
                self.visit_expr(&node.value);
            }
            ast::Stmt::If(node) => self.visit_expr(&node.test),
            ast::Stmt::Let(node) => self.visit_expr(&node.value),
            ast::Stmt::While(node) => self.visit_expr(&node.test),
            ast::Stmt::For(node) => {
                self.visit_expr(&node.target);
                self.visit_expr(&node.iter);
            }
            ast::Stmt::Assert(node) => {
                self.visit_expr(&node.test);
                if let Some(msg) = &node.msg {
                    self.visit_expr(msg);
                }
            }
            ast::Stmt::Delete(node) => {
                for target in &node.targets {
                    self.visit_expr(target);
                }
            }
            ast::Stmt::With(node) => {
                for item in &node.items {
                    self.visit_expr(&item.context_expr);
                    if let Some(optional_vars) = &item.optional_vars {
                        self.visit_expr(optional_vars);
                    }
                }
            }
            ast::Stmt::Match(node) => self.visit_expr(&node.subject),
            ast::Stmt::Raise(node) => {
                if let Some(exc) = &node.exc {
                    self.visit_expr(exc);
                }
                if let Some(cause) = &node.cause {
                    self.visit_expr(cause);
                }
            }
            _ => {}
        }
    }

    /// Derive the role of the use from its parent expressions.
    fn role_from_stack(&self, use_expr: &'ast ast::Expr) -> FoundFluidUse<'ast> {
        let Some((parent, rest)) = self.stack.split_last() else {
            return FoundFluidUse::Direct(use_expr);
        };

        let use_key = ExpressionNodeKey::from(use_expr);

        let role = match parent {
            // `a.m` — the role depends on whether the bound method is immediately called.
            ast::Expr::Attribute(attribute)
                if ExpressionNodeKey::from(attribute.value.as_ref()) == use_key =>
            {
                match rest.last() {
                    Some(ast::Expr::Call(call))
                        if ExpressionNodeKey::from(call.func.as_ref())
                            == ExpressionNodeKey::from(*parent) =>
                    {
                        FluidUseRole::MethodReceiver
                    }
                    // A bound method that escapes without being called could observe
                    // the specialization later.
                    _ => FluidUseRole::Escape,
                }
            }
            ast::Expr::Subscript(subscript)
                if ExpressionNodeKey::from(subscript.value.as_ref()) == use_key =>
            {
                match subscript.ctx {
                    ast::ExprContext::Load => FluidUseRole::Read,
                    ast::ExprContext::Store => FluidUseRole::SubscriptStore,
                    ast::ExprContext::Del | ast::ExprContext::Invalid => FluidUseRole::Escape,
                }
            }
            // A direct argument of a plain function call is inferred with the declared
            // parameter type as bidirectional type context. Arguments of bound-method
            // calls escape instead: the receiver may retain the value (e.g.
            // `other.append(a)` aliases `a` into `other`).
            ast::Expr::Call(call) if ExpressionNodeKey::from(call.func.as_ref()) != use_key => {
                let role = if call.func.is_name_expr() {
                    FluidUseRole::TypeContextual
                } else {
                    FluidUseRole::Escape
                };
                return FoundFluidUse::Nested {
                    role,
                    call_is_root: rest.is_empty(),
                };
            }
            _ => FluidUseRole::Escape,
        };

        FoundFluidUse::Nested {
            role,
            call_is_root: false,
        }
    }
}

impl<'ast> Visitor<'ast> for FluidUseClassifier<'ast> {
    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        if self.result.is_some() {
            return;
        }

        if ExpressionNodeKey::from(expr) == self.use_expression {
            self.result = Some(self.role_from_stack(expr));
            return;
        }

        self.stack.push(expr);
        walk_expr(self, expr);
        self.stack.pop();
    }
}

/// The places a pattern match against a subject can narrow: the subject's own
/// places with the bindings each was read from, and — for a list/tuple subject
/// display — its individual elements. See
/// [`SemanticIndexBuilder::match_subject_targets`].
type MatchSubjectTargets = (
    SmallVec<[(ScopedPlaceId, SmallVec<[ScopedDefinitionId; 2]>); 2]>,
    SmallVec<[(ScopedPlaceId, ScopedUseId, ExpressionNodeKey); 2]>,
);

#[derive(Debug, PartialEq)]
struct CurrentMatchCase<'ast, 'db> {
    /// The pattern that's part of the current match case.
    pattern: &'ast ast::Pattern,

    /// The predicate for the complete match case.
    predicate: PatternPredicate<'db>,

    /// basedpython: the bare `case A:` names the predicate offered to
    /// context-sensitive resolution, as collected by
    /// [`SemanticIndexBuilder::predicate_kind`].
    case_names: CaseNames<'ast, 'db>,
}

/// basedpython: the bare `case A:` names of one pattern, each paired with what
/// type checking needs to decide whether it is a capture at all.
type CaseNames<'ast, 'db> = Vec<(&'ast ast::Identifier, CaseNamePredicateKind<'db>)>;

impl<'ast, 'db> CurrentMatchCase<'ast, 'db> {
    fn new(
        pattern: &'ast ast::Pattern,
        predicate: PatternPredicate<'db>,
        case_names: CaseNames<'ast, 'db>,
    ) -> Self {
        Self {
            pattern,
            predicate,
            case_names,
        }
    }

    /// basedpython: what type checking needs to decide whether `identifier`
    /// captures, or `None` when it is an ordinary capture either way.
    fn case_name(&self, identifier: &ast::Identifier) -> Option<&CaseNamePredicateKind<'db>> {
        self.case_names
            .iter()
            .find(|(case_name, _)| std::ptr::eq(*case_name, identifier))
            .map(|(_, kind)| kind)
    }
}

/// basedpython: the state of a [statement expression](ast::ExprStatement) whose
/// wrapped statement is currently being visited.
#[derive(Debug)]
struct CurrentStatementExpression {
    /// The synthetic place holding the statement expression's value.
    place: ScopedPlaceId,

    /// Every expression whose evaluation produces the statement expression's
    /// value: the tail expression of each branch, plus the operand of each
    /// `break <value>` targeting the loop the statement expression wraps.
    ///
    /// The value is *possibly unbound* at the use exactly when some path through
    /// the statement reaches its end without passing one of these — which is
    /// what makes the statement expression non-exhaustive.
    values: FxHashSet<ExpressionNodeKey>,
}

enum Unpackable<'ast> {
    Assign(&'ast ast::StmtAssign),
    For(&'ast ast::StmtFor),
    WithItem {
        item: &'ast ast::WithItem,
        is_async: bool,
    },
    Comprehension {
        first: bool,
        node: &'ast ast::Comprehension,
    },
}

impl<'ast> Unpackable<'ast> {
    const fn kind(&self) -> UnpackKind {
        match self {
            Unpackable::Assign(_) => UnpackKind::Assign,
            Unpackable::For(ast::StmtFor { is_async, .. }) => UnpackKind::Iterable {
                mode: EvaluationMode::from_is_async(*is_async),
            },
            Unpackable::Comprehension {
                node: ast::Comprehension { is_async, .. },
                ..
            } => UnpackKind::Iterable {
                mode: EvaluationMode::from_is_async(*is_async),
            },
            Unpackable::WithItem { is_async, .. } => UnpackKind::ContextManager {
                mode: EvaluationMode::from_is_async(*is_async),
            },
        }
    }

    fn as_current_assignment<'db>(
        &self,
        unpack: Option<Unpack<'db>>,
    ) -> CurrentAssignment<'ast, 'db> {
        let positioned = unpack.map(|unpack| (UnpackPosition::First, unpack));
        match self {
            Unpackable::Assign(stmt) => CurrentAssignment::Assign {
                node: stmt,
                unpack,
                owner: BindingsOwner::Statement,
            },
            Unpackable::For(stmt) => CurrentAssignment::For {
                node: stmt,
                unpack: positioned,
            },
            Unpackable::WithItem { item, is_async } => CurrentAssignment::WithItem {
                item,
                is_async: *is_async,
                unpack: positioned,
            },
            Unpackable::Comprehension { node, first } => CurrentAssignment::Comprehension {
                node,
                first: *first,
                unpack: positioned,
            },
        }
    }
}

/// Returns the single argument to `__all__.extend()`, if it is a call to `__all__.extend()`
/// where it looks like the argument might be a `submodule.__all__` expression.
/// Else, returns `None`.
fn dunder_all_extend_argument(value: &ast::Expr) -> Option<&ast::Expr> {
    let ast::ExprCall {
        func,
        arguments:
            ast::Arguments {
                args,
                keywords,
                range: _,
                node_index: _,
            },
        ..
    } = value.as_call_expr()?;

    let ast::ExprAttribute { value, attr, .. } = func.as_attribute_expr()?;

    let ast::ExprName { id, .. } = value.as_name_expr()?;

    if id != "__all__" {
        return None;
    }

    if attr != "extend" {
        return None;
    }

    if !keywords.is_empty() {
        return None;
    }

    let [single_argument] = &**args else {
        return None;
    };

    let ast::ExprAttribute { value, attr, .. } = single_argument.as_attribute_expr()?;

    (attr == "__all__").then_some(value)
}

/// basedpython: the places this file's narrowing guards name.
#[derive(Debug, Default)]
struct GuardTargets {
    /// Guards whose root is not a parameter of the annotated function, as a root name and
    /// the attribute segments below it.
    scope_places: Vec<(Name, Box<[Name]>)>,
    /// The attribute segments of guards rooted at a parameter, which apply below whatever
    /// a call passes for that parameter.
    member_chains: Vec<Box<[Name]>>,
    /// The same, for a guard recovered from a body rather than written in an annotation, keyed
    /// by the name of the `def` it was recovered from.
    ///
    /// A written guard is rare enough that pairing every chain with every call costs nothing. A
    /// recovered one is not: every unannotated `def` that returns a test on a member of a
    /// parameter contributes a chain, and a file with many of both would pay their product at
    /// every narrowing predicate. The name a call writes is the one syntactic thing that can
    /// narrow that down before the callee is resolvable, so a chain only reaches a call that
    /// writes the name it was recovered from. A call that reaches its callee some other way —
    /// through a variable, an alias — registers nothing and so narrows nothing.
    recovered_member_chains: FxHashMap<Name, Vec<Box<[Name]>>>,
}

impl GuardTargets {
    fn is_empty(&self) -> bool {
        self.scope_places.is_empty()
            && self.member_chains.is_empty()
            && self.recovered_member_chains.is_empty()
    }
}

/// basedpython: collects the places this file's narrowing return annotations name.
struct GuardTargetCollector<'a> {
    targets: &'a mut GuardTargets,
}

impl<'ast> Visitor<'ast> for GuardTargetCollector<'_> {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        if let ast::Stmt::FunctionDef(function) = stmt {
            if let Some(guards) = return_guards(function) {
                for guard in guards {
                    let (name, members) = guard.place_parts();
                    let members: Box<[Name]> = members.into_iter().cloned().collect();
                    if function
                        .parameters
                        .iter()
                        .any(|parameter| parameter.name().id == *name)
                    {
                        if !members.is_empty() {
                            self.targets.member_chains.push(members);
                        }
                    } else {
                        self.targets.scope_places.push((name.clone(), members));
                    }
                }
            } else if function.returns.is_none() && !function.is_asserts_return {
                // a `def` that wrote no return type has its guards recovered from what it
                // returns, so the chains those returns name below a parameter are targets too
                let mut chains = Vec::new();
                walk_body(
                    &mut ReturnedExpressionCollector {
                        parameters: &function.parameters,
                        chains: &mut chains,
                    },
                    &function.body,
                );
                if !chains.is_empty() {
                    let recovered = self
                        .targets
                        .recovered_member_chains
                        .entry(function.name.id.clone())
                        .or_default();
                    recovered.extend(chains);
                    recovered.sort_unstable();
                    recovered.dedup();
                }
            }
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, _expr: &'ast ast::Expr) {
        // a guard is declared on a statement, and recovered from the `return`s of one
    }
}

/// basedpython: collects the attribute chains a body's `return`s name below a parameter.
///
/// These are the places a recovered guard can narrow — see
/// `ty_python_semantic::types::inferred_narrowing`. Only what a `return` names counts: an
/// attribute the body merely touches is not something a caller is told anything about, and every
/// chain collected here is paired with every call root at every narrowing predicate in the file.
struct ReturnedExpressionCollector<'a, 'ast> {
    parameters: &'ast ast::Parameters,
    chains: &'a mut Vec<Box<[Name]>>,
}

impl<'ast> Visitor<'ast> for ReturnedExpressionCollector<'_, 'ast> {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        match stmt {
            // a nested `def` or `class` returns for itself, not for the function around it
            ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_) => {}
            ast::Stmt::Return(ast::StmtReturn {
                value: Some(value), ..
            }) => {
                let mut chains = MemberChainCollector {
                    parameters: self.parameters,
                    chains: self.chains,
                };
                chains.visit_expr(value);
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, _expr: &'ast ast::Expr) {
        // a statement other than a `return` is walked for the `return`s inside it alone
    }
}

/// basedpython: collects the attribute chains an expression names below a parameter.
struct MemberChainCollector<'a, 'ast> {
    parameters: &'ast ast::Parameters,
    chains: &'a mut Vec<Box<[Name]>>,
}

impl<'ast> Visitor<'ast> for MemberChainCollector<'_, 'ast> {
    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        if expr.is_attribute_expr()
            && let Some(path) = UnqualifiedName::from_expr(expr)
            && let [root, members @ ..] = path.segments()
            && self
                .parameters
                .iter()
                .any(|parameter| parameter.name().id == *root)
        {
            self.chains
                .push(members.iter().copied().map(Name::new).collect());
        }
        walk_expr(self, expr);
    }
}

/// Collects every call expression in a predicate, including the predicate itself.
struct CallCollector<'ast> {
    calls: Vec<&'ast ast::ExprCall>,
}

impl<'ast> Visitor<'ast> for CallCollector<'ast> {
    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        if let ast::Expr::Call(call) = expr {
            self.calls.push(call);
        }
        walk_expr(self, expr);
    }
}

/// The name a call writes for its callee, whether that is a bare name or the last segment of an
/// attribute access.
fn called_name(call: &ast::ExprCall) -> Option<&Name> {
    match call.func.as_ref() {
        ast::Expr::Name(name) => Some(&name.id),
        ast::Expr::Attribute(attribute) => Some(attribute.attr.id()),
        _ => None,
    }
}

/// The expressions a call binds its leading parameters to: its receiver, when the callee is
/// an attribute access, and its plain arguments.
fn call_roots(call: &ast::ExprCall) -> impl Iterator<Item = &ast::Expr> {
    let receiver = match call.func.as_ref() {
        ast::Expr::Attribute(attribute) => Some(&*attribute.value),
        _ => None,
    };
    receiver.into_iter().chain(
        call.arguments
            .args
            .iter()
            .chain(call.arguments.keywords.iter().map(|keyword| &keyword.value)),
    )
}

/// The call a statement-level call expression makes, looking through `await`.
fn asserted_call(expr: &ast::Expr) -> Option<&ast::ExprCall> {
    match expr {
        ast::Expr::Await(await_expr) => await_expr.value.as_call_expr(),
        expr => expr.as_call_expr(),
    }
}

/// Returns `true` for syntactically direct `range(...)` calls.
///
/// This avoids adding reachability predicates for every `for` loop target to the TDD graph. We only
/// emit the predicate for syntactically direct `range(...)` calls; type checking later verifies that
/// the callee resolves to the built-in `range` and determines whether the range is statically
/// non-empty.
fn is_direct_range_call(expr: &ast::Expr) -> bool {
    expr.expression_value()
        .as_call_expr()
        .and_then(|call| call.func.as_name_expr())
        .is_some_and(|name| name.id == "range")
}

/// Builds an interval-map that matches expressions (by their node index) to their enclosing scopes.
///
/// The interval map is built in a two-step process because the expression ids are assigned in source order,
/// but we visit the expressions in semantic order. Few expressions are registered out of order.
///
/// 1. build a point vector that maps node indices to their corresponding file scopes.
/// 2. Sort the expressions by their starting id. Then condense the point vector into an interval map
///    by collapsing adjacent node indices with the same scope
///    into a single interval.
struct ExpressionsScopeMapBuilder {
    expression_and_scope: Vec<(NodeIndex, FileScopeId)>,
}

impl ExpressionsScopeMapBuilder {
    fn new() -> Self {
        Self {
            expression_and_scope: vec![],
        }
    }

    fn record_expression(&mut self, expression: &impl HasTrackedScope, scope: FileScopeId) {
        self.expression_and_scope
            .push((expression.node_index().load(), scope));
    }

    fn build(mut self) -> ExpressionsScopeMap {
        self.expression_and_scope
            .sort_unstable_by_key(|(index, _)| *index);

        let mut iter = self.expression_and_scope.into_iter();
        let Some(first) = iter.next() else {
            return ExpressionsScopeMap::default();
        };

        let mut interval_map = Vec::new();

        let mut current_scope = first.1;
        let mut range = first.0..=first.0;

        for (index, scope) in iter {
            if scope == current_scope {
                range = *range.start()..=index;
                continue;
            }

            interval_map.push((range, current_scope));

            current_scope = scope;
            range = index..=index;
        }

        interval_map.push((range, current_scope));

        ExpressionsScopeMap(interval_map.into_boxed_slice())
    }
}

/// Returns the static truthiness of a literal iterable.
///
/// Returns [`Truthiness::Unknown`] for other expressions and when starred elements or dictionary
/// unpacking make the literal's emptiness ambiguous.
fn literal_iterable_truthiness(expr: &ast::Expr) -> Truthiness {
    match expr {
        ast::Expr::Tuple(_)
        | ast::Expr::List(_)
        | ast::Expr::Set(_)
        | ast::Expr::Dict(_)
        | ast::Expr::StringLiteral(_)
        | ast::Expr::BytesLiteral(_) => Truthiness::from_expr(expr, |_| false),
        _ => Truthiness::Unknown,
    }
}

/// Returns if the expression is a `TYPE_CHECKING` expression.
fn is_if_type_checking(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::Name(ast::ExprName { id, .. }) => id == "TYPE_CHECKING",
        ast::Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
            attr == "TYPE_CHECKING" && is_dotted_name(value)
        }
        _ => false,
    }
}

/// Returns if the expression is a `not TYPE_CHECKING` expression.
fn is_if_not_type_checking(expr: &ast::Expr) -> bool {
    matches!(
        expr,
        ast::Expr::UnaryOp(ast::ExprUnaryOp {
            op: ast::UnaryOp::Not,
            operand,
            ..
        }) if is_if_type_checking(operand)
    )
}

/// Whether an expression can create a "fluid" specialization when bound to a name: a
/// generic instance whose inferred specialization may be refined by later uses of the
/// binding. Collection literals and constructor calls qualify; calls with subscripted
/// callees (e.g. `A[int](...)`) are excluded because their specialization is explicit.
///
/// This is a purely syntactic over-approximation: whether the assigned value actually
/// is a generic instance with an inferred specialization is determined during type
/// inference.
fn is_fluid_specialization_candidate(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::List(_) | ast::Expr::Set(_) | ast::Expr::Dict(_) => true,
        ast::Expr::Call(call) => matches!(
            call.func.as_ref(),
            ast::Expr::Name(_) | ast::Expr::Attribute(_)
        ),
        _ => false,
    }
}
