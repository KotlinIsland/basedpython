//! _Predicates_ are Python expressions whose runtime values can affect type inference.
//!
//! We currently use predicates in two places:
//!
//! - [_Narrowing constraints_][crate::narrowing_constraints] constrain the type of
//!   a binding that is visible at a particular use.
//! - [_Reachability constraints_][crate::reachability_constraints] determine the
//!   static reachability of a binding, and the reachability of a statement or expression.

use crate::Program;
use ruff_db::PythonFile;
use ruff_db::files::File;
use ruff_index::{FrozenIndexVec, Idx, IndexVec};
use ruff_python_ast::{Singleton, name::Name};

use crate::ProgramFile;
use crate::ast_ids::ExpressionNodeKey;
use crate::db::Db;
use crate::definition::Definition;
use crate::expression::Expression;
use crate::global_scope;
use crate::reachability_constraints::ScopedReachabilityConstraintId;
use crate::scope::{FileScopeId, ScopeId};
use crate::symbol::ScopedSymbolId;

// A scoped identifier for each `Predicate` in a scope.
#[derive(Clone, Debug, Copy, PartialOrd, Ord, PartialEq, Eq, Hash, get_size2::GetSize)]
pub struct ScopedPredicateId(u32);

impl ScopedPredicateId {
    /// A special ID that is used for an "always true" predicate.
    pub(crate) const ALWAYS_TRUE: ScopedPredicateId = ScopedPredicateId(0xffff_ffff);

    /// A special ID that is used for an "always false" predicate.
    pub(crate) const ALWAYS_FALSE: ScopedPredicateId = ScopedPredicateId(0xffff_fffe);

    const SMALLEST_TERMINAL: ScopedPredicateId = Self::ALWAYS_FALSE;

    fn is_terminal(self) -> bool {
        self >= Self::SMALLEST_TERMINAL
    }
}

impl Idx for ScopedPredicateId {
    #[inline]
    fn new(value: usize) -> Self {
        assert!(value <= (Self::SMALLEST_TERMINAL.0 as usize));
        #[expect(clippy::cast_possible_truncation)]
        Self(value as u32)
    }

    #[inline]
    fn index(self) -> usize {
        debug_assert!(!self.is_terminal());
        self.0 as usize
    }
}

// A collection of predicates for a given scope.
pub type Predicates<'db> = FrozenIndexVec<ScopedPredicateId, Predicate<'db>>;

#[derive(Debug, Default)]
pub(crate) struct PredicatesBuilder<'db> {
    predicates: IndexVec<ScopedPredicateId, Predicate<'db>>,
}

impl<'db> PredicatesBuilder<'db> {
    /// Adds a predicate. Note that we do not deduplicate predicates. If you add a `Predicate`
    /// more than once, you will get distinct `ScopedPredicateId`s for each one. (This lets you
    /// model predicates that might evaluate to different values at different points of execution.)
    pub(crate) fn add_predicate(&mut self, predicate: Predicate<'db>) -> ScopedPredicateId {
        self.predicates.push(predicate)
    }

    pub(crate) fn build(self) -> Predicates<'db> {
        self.predicates.into()
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub struct Predicate<'db> {
    pub node: PredicateNode<'db>,
    pub is_positive: bool,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, get_size2::GetSize)]
pub(crate) enum PredicateOrLiteral<'db> {
    Literal(bool),
    Predicate(Predicate<'db>),
}

impl PredicateOrLiteral<'_> {
    pub(crate) fn negated(self) -> Self {
        match self {
            PredicateOrLiteral::Literal(value) => PredicateOrLiteral::Literal(!value),
            PredicateOrLiteral::Predicate(Predicate { node, is_positive }) => {
                PredicateOrLiteral::Predicate(Predicate {
                    node,
                    is_positive: !is_positive,
                })
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub struct CallableAndCallExpr<'db> {
    pub callable: Expression<'db>,
    pub call_expr: Expression<'db>,
    /// Whether the call is wrapped in an `await` expression. If `true`, `call_expr` refers to the
    /// `await` expression rather than the call itself. This is used to detect terminal `await`s of
    /// async functions that return `Never`.
    pub is_await: bool,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub enum PredicateNode<'db> {
    /// The truthiness of an expression's resulting value.
    Expression(Expression<'db>),
    /// A boolean operation, `not`, or conditional expression evaluated directly as a condition.
    ///
    /// In `if x and False`, the truthy branch is unreachable. But after `y = x and False`,
    /// `if y` may be truthy: it can call `x.__bool__` a second time and get a different result.
    Condition(Expression<'db>),
    /// A chained comparison evaluated directly as a condition. Its inferred truthiness is
    /// available without walking the expression again.
    ChainedComparisonCondition(Expression<'db>),
    /// Whether a context manager's exit return type allows an exception to be suppressed.
    ///
    /// Resolved during type inference because the context manager's type is unavailable during
    /// semantic indexing.
    ContextManagerSuppresses {
        expression: Expression<'db>,
        is_async: bool,
    },
    /// Whether semantic evaluation rules out every normal entry into a `finally` suite.
    ///
    /// The continuation is captured before constructing this predicate, so its constraint cannot
    /// depend on the predicate itself. Deferring evaluation preserves terminal cleanup paths when
    /// a context manager's suppression behavior is unavailable during semantic indexing.
    FinallyNormalPathImpossible {
        scope: ScopeId<'db>,
        continuation: ScopedReachabilityConstraintId,
    },
    /// These predicates are recorded for statements with call expressions. As part of
    /// reachability constraints, they are used to determine whether control flow can
    /// continue past this statement or not.
    ///
    /// The predicate evaluates to
    /// [`crate::Truthiness::AlwaysTrue`] in the common case where a call
    /// is inferred as returning an inhabited type: in these situations, we will
    /// infer control flow as flowing through the call expression without
    /// terminating. If it can be statically guaranteed that a call always
    /// returns `Never`/`NoReturn`, however, the predicate evaluates to
    /// [`crate::Truthiness::AlwaysFalse`], signaling that control flow
    /// ends as a result of the call: these call expressions are terminal.
    ///
    /// These predicates never evaluate to
    /// [`crate::Truthiness::Ambiguous`], even if the return type of the
    /// call is `Unknown`/`Any`, because that would result in too many false
    /// positives.
    IsNonTerminalCall(CallableAndCallExpr<'db>),
    /// basedpython: a statement-level call that may be a call to an assertion guard
    /// (`def f(x) -> asserts x`).
    ///
    /// The guard names a place the call narrows once it returns, which is the whole of the
    /// code that follows the call statement — so unlike an ordinary call, this predicate is
    /// recorded as a narrowing constraint on the statement's own flow. Whether the callee is
    /// an assertion guard at all is resolved during type checking; a call to anything else
    /// narrows nothing.
    AssertsCall(CallableAndCallExpr<'db>),
    /// Whether an iterable is statically known to yield at least one item.
    ///
    /// Currently, this predicate is only emitted for direct `range(...)` calls. It is resolved
    /// semantically during type checking, so calls to a shadowed `range` remain ambiguous.
    IsNonEmptyIterable(Expression<'db>),
    Pattern(PatternPredicate<'db>),
    /// Whether control flow takes one branch of an OR pattern instead of its remaining
    /// alternatives. The selected branch is unknown, but recording a predicate and its negation
    /// preserves the fact that exactly one branch is taken.
    OrPatternAlternative(ScopeId<'db>),
    SubjectElementPattern(SubjectElementPatternPredicate<'db>),
    StarImportPlaceholder(StarImportPlaceholderPredicate<'db>),
    /// basedpython: whether a bare `case A:` binds `A` at all.
    ///
    /// The name is a capture only when it is *not* an enum member of the
    /// subject's type — see [`CaseNamePredicateKind`] — so the capture's binding
    /// is recorded under this predicate and disappears where the name turned out
    /// to be a value pattern. Evaluates to [`crate::Truthiness::AlwaysTrue`] or
    /// [`crate::Truthiness::AlwaysFalse`], never to
    /// [`crate::Truthiness::Ambiguous`]: a name either resolves or it does not.
    CaseNameCapture(CaseNameCapturePredicate<'db>),
}

/// basedpython: one bare `case A:` name — `case A | B:` records one for each.
#[salsa::tracked(debug, heap_size=ruff_memory_usage::heap_size)]
pub struct CaseNameCapturePredicate<'db> {
    #[returns(ref)]
    pub kind: CaseNamePredicateKind<'db>,
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for CaseNameCapturePredicate<'_> {}

impl<'db> From<CaseNameCapturePredicate<'db>> for PredicateOrLiteral<'db> {
    fn from(predicate: CaseNameCapturePredicate<'db>) -> Self {
        PredicateOrLiteral::Predicate(Predicate {
            node: PredicateNode::CaseNameCapture(predicate),
            is_positive: true,
        })
    }
}

/// A pattern predicate applied to one expression in a sequence-display subject.
///
/// The full pattern determines the predicate's truth value, while `target` selects the subject
/// occurrence whose aligned pattern constraint should be applied to a binding.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub struct SubjectElementPatternPredicate<'db> {
    pub pattern: PatternPredicate<'db>,
    pub target: ExpressionNodeKey,
}

/// Structural details for sequence patterns that affect narrowing and reachability.
#[derive(Debug, Clone, Hash, PartialEq, get_size2::GetSize, salsa::SalsaValue)]
pub struct SequencePatternPredicateKind<'db> {
    pub patterns: Box<[PatternPredicateKind<'db>]>,
}

impl<'db> SequencePatternPredicateKind<'db> {
    /// Return `true` for `case [*rest]`, the only sequence pattern with no
    /// length or element constraints.
    pub fn is_irrefutable(&self) -> bool {
        matches!(self.patterns.as_ref(), [PatternPredicateKind::Star(_)])
    }

    /// Return the patterns before and after the starred element.
    pub fn split_around_star(
        &self,
    ) -> Option<(&[PatternPredicateKind<'db>], &[PatternPredicateKind<'db>])> {
        let star_index = self
            .patterns
            .iter()
            .position(|pattern| matches!(pattern, PatternPredicateKind::Star(_)))?;
        let (prefix, star_and_suffix) = self.patterns.split_at(star_index);
        Some((prefix, &star_and_suffix[1..]))
    }
}

/// Structural details for a class pattern.
#[derive(Debug, Clone, Hash, PartialEq, get_size2::GetSize, salsa::SalsaValue)]
pub struct ClassPatternPredicateKind<'db> {
    pub class: Expression<'db>,
    /// The positional subpatterns, in source order, with any starred wildcard
    /// left out — it matches nothing itself, it only moves the subpatterns
    /// written after it to the end of `__match_args__`.
    pub positional: Box<[PatternPredicateKind<'db>]>,
    /// basedpython `case A(x, *_, y)`: how many of `positional` were written
    /// after the starred wildcard, and so name the *last* entries of
    /// `__match_args__` rather than the first. `0` for every python class
    /// pattern, and for a `*_` written last — `case A(x, *_)` accepts exactly
    /// what `case A(x)` does
    pub positional_from_end: usize,
    pub keywords: Box<[ClassPatternKeywordPredicateKind<'db>]>,
}

impl ClassPatternPredicateKind<'_> {
    pub fn is_empty(&self) -> bool {
        self.positional.is_empty() && self.keywords.is_empty()
    }
}

#[derive(Debug, Clone, Hash, PartialEq, get_size2::GetSize, salsa::SalsaValue)]
pub struct ClassPatternKeywordPredicateKind<'db> {
    pub attr: Name,
    pub pattern: PatternPredicateKind<'db>,
}

/// Structural details for a mapping pattern.
#[derive(Debug, Clone, Hash, PartialEq, get_size2::GetSize, salsa::SalsaValue)]
pub struct MappingPatternPredicateKind<'db> {
    pub entries: Box<[MappingPatternEntryPredicateKind<'db>]>,
    pub rest: Option<Name>,
}

impl MappingPatternPredicateKind<'_> {
    pub fn is_irrefutable(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Clone, Hash, PartialEq, get_size2::GetSize, salsa::SalsaValue)]
pub struct MappingPatternEntryPredicateKind<'db> {
    pub key: Expression<'db>,
    pub pattern: PatternPredicateKind<'db>,
}

/// basedpython: a bare `case A:` matched directly against the subject.
///
/// Such a name is a capture in python, but in basedpython it is first offered to
/// ty's context-sensitive resolution: an unambiguous enum member of the subject's
/// type makes it a *value* pattern, and anything else leaves it the capture
/// python spells. Which one it is depends on the subject's type, so this is the
/// one pattern whose shape is not settled until type checking.
///
/// The scope is carried here because the resolution rules are the same as the
/// rest of context-sensitive resolution's: the name must be claimed by no
/// lexical scope, and the enum must be nameable in this one.
#[derive(Debug, Clone, Hash, PartialEq, get_size2::GetSize, salsa::SalsaValue)]
pub struct CaseNamePredicateKind<'db> {
    pub name: Name,
    pub scope: ScopeId<'db>,

    /// What the case matches, which is what the name is resolved against.
    ///
    /// Held here rather than taken from whatever subject type reaches this node,
    /// because that one has already been narrowed by the preceding cases: by the
    /// last case of an exhaustive `match` nothing of the enum is left, and a name
    /// that stopped resolving there would silently turn back into a capture. What
    /// a name means cannot depend on which case it is written in.
    pub subject: PatternSubject<'db>,
}

/// Pattern structure used for type narrowing, static reachability, and inferring the types of
/// names bound by a successful match.
#[derive(Debug, Clone, Hash, PartialEq, get_size2::GetSize, salsa::SalsaValue)]
pub enum PatternPredicateKind<'db> {
    Singleton(Singleton),
    Value(Expression<'db>),
    Or(Box<[PatternPredicateKind<'db>]>),
    /// basedpython `P and Q`: matches only what every one of its patterns matches
    And(Box<[PatternPredicateKind<'db>]>),
    Class(ClassPatternPredicateKind<'db>),
    Mapping(MappingPatternPredicateKind<'db>),
    Sequence(SequencePatternPredicateKind<'db>),
    As(Option<Box<PatternPredicateKind<'db>>>, Option<Name>),
    Star(Option<Name>),
    /// basedpython `case A:`
    CaseName(CaseNamePredicateKind<'db>),
}

#[salsa::tracked(debug, heap_size=ruff_memory_usage::heap_size)]
pub struct PatternPredicate<'db> {
    #[returns(copy)]
    pub program_file: ProgramFile<'db>,

    #[returns(copy)]
    pub file_scope: FileScopeId,

    #[returns(copy)]
    pub subject: PatternSubject<'db>,

    #[returns(ref)]
    pub kind: PatternPredicateKind<'db>,

    #[returns(copy)]
    pub guard: Option<Expression<'db>>,

    /// A reference to the pattern of the previous match case
    #[returns(as_deref)]
    pub previous_predicate: Option<Box<PatternPredicate<'db>>>,
}

/// The value a pattern is matched against.
///
/// A `match` case, an `if let` clause and a `let` statement all match an
/// expression the source wrote. basedpython's other destructuring positions —
/// a `for` target, a `with` item, a parameter — have no such expression: the
/// value arrives already bound to a [synthetic binder](ruff_python_ast::destructure_binder_name),
/// and the binder's definition is what says what its type is.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub enum PatternSubject<'db> {
    Expression(Expression<'db>),
    Binder(Definition<'db>),
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for PatternPredicate<'_> {}

impl<'db> PatternPredicate<'db> {
    pub fn file(self, db: &'db dyn Db) -> File {
        self.program_file(db).file(db)
    }

    pub fn python_file(self, db: &'db dyn Db) -> PythonFile<'db> {
        self.program_file(db).python_file(db)
    }

    pub fn scope(self, db: &'db dyn Db) -> ScopeId<'db> {
        self.file_scope(db).to_scope_id(db, self.program_file(db))
    }

    pub fn program(self, db: &'db dyn Db) -> Program<'db> {
        self.scope(db).program(db)
    }
}

/// A "placeholder predicate" that is used to model the fact that the boundness of a (possible)
/// definition or declaration caused by a `*` import cannot be fully determined until type-
/// inference time. This is essentially the same as a standard reachability constraint, so we reuse
/// the [`Predicate`] infrastructure to model it.
///
/// To illustrate, say we have a module `exporter.py` like so:
///
/// ```py
/// if <condition>:
///     class A: ...
/// ```
///
/// and we have a module `importer.py` like so:
///
/// ```py
/// A = 1
///
/// from exporter import *
/// ```
///
/// Since we cannot know whether or not <condition> is true at semantic-index time, we record
/// a definition for `A` in `importer.py` as a result of the `from exporter import *` statement,
/// but place a predicate on it to record the fact that we don't yet know whether this definition
/// will be visible from all control-flow paths or not. Essentially, we model `importer.py` as
/// something similar to this:
///
/// ```py
/// A = 1
///
/// if <star_import_placeholder_predicate>:
///     from a import A
/// ```
///
/// At type-check time, the placeholder predicate for the `A` definition is evaluated by attempting
/// to resolve the `A` symbol in `exporter.py`'s global namespace:
/// - If it resolves to a definitely bound symbol, then the predicate resolves to [`Truthiness::AlwaysTrue`]
/// - If it resolves to an unbound symbol, then the predicate resolves to [`Truthiness::AlwaysFalse`]
/// - If it resolves to a possibly bound symbol, then the predicate resolves to [`Truthiness::Ambiguous`]
///
/// [Truthiness]: [crate::types::Truthiness]
#[salsa::tracked(debug, heap_size=ruff_memory_usage::heap_size)]
pub struct StarImportPlaceholderPredicate<'db> {
    #[returns(copy)]
    pub importing_file: ProgramFile<'db>,

    /// Each symbol imported by a `*` import has a separate predicate associated with it:
    /// this field identifies which symbol that is.
    ///
    /// Note that a [`ScopedPlaceId`] is only meaningful if you also know the scope
    /// it is relative to. For this specific struct, however, there's no need to store a
    /// separate field to hold the ID of the scope. `StarImportPredicate`s are only created
    /// for valid `*`-import definitions, and valid `*`-import definitions can only ever
    /// exist in the global scope; thus, we know that the `symbol_id` here will be relative
    /// to the global scope of the importing file.
    #[returns(copy)]
    pub symbol_id: ScopedSymbolId,

    #[returns(copy)]
    pub referenced_file: ProgramFile<'db>,
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for StarImportPlaceholderPredicate<'_> {}

impl<'db> StarImportPlaceholderPredicate<'db> {
    pub fn scope(self, db: &'db dyn Db) -> ScopeId<'db> {
        // See doc-comment above [`StarImportPlaceholderPredicate::symbol_id`]:
        // valid `*`-import definitions can only take place in the global scope.
        global_scope(db, self.importing_file(db))
    }
}

impl<'db> From<StarImportPlaceholderPredicate<'db>> for PredicateOrLiteral<'db> {
    fn from(predicate: StarImportPlaceholderPredicate<'db>) -> Self {
        PredicateOrLiteral::Predicate(Predicate {
            node: PredicateNode::StarImportPlaceholder(predicate),
            is_positive: true,
        })
    }
}
