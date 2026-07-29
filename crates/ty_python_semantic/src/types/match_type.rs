//! basedpython: match types — a type alias whose value is chosen by pattern matching
//!
//! ```by
//! type NDTuple[T, *Shape: int] = match *Shape:
//!     case ():
//!         T
//!     case (Dim, *Rest):
//!         (NDTuple[T, *Rest],) * Dim
//! ```
//!
//! The alias is applied like any other — `NDTuple[int, 2, 3]` — and its value is the body
//! of the first `case` whose pattern matches the subject. A pattern's captures (`Dim`,
//! `*Rest`) are *type variables* scoped to that case, so the body is inferred exactly once,
//! symbolically, and each application only has to substitute them. That is what makes the
//! recursion work: `(NDTuple[T, *Rest],) * Dim` is inferred as a deferred operation over the
//! typevars, and substituting `Dim = 2`, `Rest = (3,)` re-folds it into a real tuple type.
//!
//! An application whose subject is not yet known — `NDTuple[T, *Shape]` inside
//! `class Array[T, *Shape]` — matches nothing and stays symbolic. It reduces later, when the
//! enclosing class is specialized and the alias's own specialization becomes concrete.
//!
//! Matching answers three ways, not two (see [`PatternMatch`]). Collapsing "does not match"
//! and "cannot be decided" into one `false` is what makes a match type quietly pick the
//! *wrong* case: a subject of unknown length, or a malformed pattern, would fall through to
//! whichever later case happens to be irrefutable.

use ruff_python_ast as ast;

use ruff_db::parsed::parsed_module;

use crate::Db;
use crate::types::deferred::{DeferredOperation, DeferredType};
use crate::types::generics::{ApplySpecialization, GenericContext};
use crate::types::tuple::{Tuple, TupleType};
use crate::types::type_alias::PEP695TypeAliasType;
use crate::types::typevar::{BindingContext, BoundTypeVarInstance};
use crate::types::visitor::any_over_type;
use crate::types::{
    ApplyTypeMappingVisitor, KnownInstanceType, Type, TypeAliasType, TypeContext, TypeMapping,
    bind_typevar, binding_type, definition_expression_type,
};
use ty_python_core::definition::Definition;
use ty_python_core::semantic_index;

/// basedpython: the type an application of `alias` denotes, if `alias` is a match type.
///
/// The application is a [`DeferredOperation::MatchType`]: the arguments are its operands, so
/// a specialization substitutes them and the match is re-run, exactly as a deferred `Dim + 1`
/// re-folds. Fully known arguments are evaluated on the spot and never become symbolic.
///
/// Returns `None` for an ordinary type alias.
pub(crate) fn match_type_application<'db>(
    db: &'db dyn Db,
    alias: TypeAliasType<'db>,
) -> Option<Type<'db>> {
    let pep695 = alias.as_pep_695_type_alias()?;
    if !pep695.is_match_type(db) {
        return None;
    }
    let generic_context = pep695.generic_context(db)?;
    let specialization = pep695
        .specialization(db)
        .unwrap_or_else(|| generic_context.default_specialization(db, None));

    let mut operands = Vec::with_capacity(specialization.types(db).len() + 1);
    operands.push(Type::KnownInstance(KnownInstanceType::TypeAliasType(
        alias.unspecialized(db),
    )));
    operands.extend_from_slice(specialization.types(db));

    Some(DeferredType::build(
        db,
        &DeferredOperation::MatchType,
        operands.into_boxed_slice(),
    ))
}

/// How large a match type's subject may get before evaluation gives up.
///
/// A well-founded match type shrinks its subject, so it only ever gets *smaller* as the
/// recursion proceeds. An ill-founded one — `case (A, *R): Grow[A, *R, A]` — grows it
/// without bound and would otherwise recurse until the stack ran out.
///
/// The budget is deliberately a property of the subject alone, so whether a given
/// application reduces never depends on how it was reached. A limit on recursion *depth*
/// would have made that dependent on the call stack, and this evaluation is memoized: the
/// same application must not answer differently for two use sites. Self-referential
/// recursion at an unchanging argument (`case X: Loop[X]`) is a genuine query cycle, which
/// Salsa's cycle handling catches instead.
const MAX_SUBJECT_NODES: u32 = 48;

/// The result of evaluating a match type's application.
#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) enum MatchTypeOutcome<'db> {
    /// A case matched; this is the alias's value.
    Matched(Type<'db>),
    /// Which case applies is not yet known — the arguments still mention a type parameter,
    /// the subject's shape is not pinned down, or a case is malformed. The application stays
    /// symbolic and is retried once it is specialized.
    Unresolved,
    /// The subject is fully known and no case matched.
    NoCaseMatched,
    /// The subject grew past [`MAX_SUBJECT_NODES`], so the match type does not reduce.
    TooLarge,
}

/// Evaluates the `case` blocks of `alias` against its subject, with the alias's own
/// specialization applied.
///
/// Returns `None` when `alias` is not a match type.
pub(crate) fn evaluate_match_type<'db>(
    db: &'db dyn Db,
    alias: PEP695TypeAliasType<'db>,
) -> Option<&'db MatchTypeOutcome<'db>> {
    if !alias.is_match_type(db) {
        return None;
    }
    Some(evaluate_match_type_cached(db, alias))
}

/// Evaluates a match type, memoized per application.
///
/// Memoizing matters for more than speed: `NDTuple[int, 2, 3]` expands into two copies of
/// `NDTuple[int, 3]`, and each enclosing dimension multiplies that again, so an unmemoized
/// evaluation is exponential in the shape.
///
/// A cyclic definition (`type Loop[T] = match T: case X: Loop[X]`) re-enters this query at
/// the same key; the cycle's initial value leaves it unresolved rather than diverging.
#[salsa::tracked(
    returns(ref),
    cycle_initial = |_, _, _| MatchTypeOutcome::Unresolved,
    heap_size = ruff_memory_usage::heap_size
)]
fn evaluate_match_type_cached<'db>(
    db: &'db dyn Db,
    alias: PEP695TypeAliasType<'db>,
) -> MatchTypeOutcome<'db> {
    let scope = alias.rhs_scope(db);
    let file = scope.file(db);
    let module = parsed_module(db, file).load(db);
    let node = scope.node(db).expect_type_alias().node(&module);

    let definition = alias.definition(db);
    let subject = alias.apply_own_specialization(
        db,
        subject_type(db, definition, &node.value).unwrap_or_else(Type::unknown),
    );

    // a subject that still mentions a type parameter cannot pick a case: `()` and
    // `(Dim, *Rest)` are both still possible
    if subject.has_typevar(db) {
        return MatchTypeOutcome::Unresolved;
    }
    if exceeds_budget(db, subject) {
        return MatchTypeOutcome::TooLarge;
    }

    for case in &node.cases {
        let mut bindings = Bindings::default();
        match match_pattern(db, file, subject, &case.pattern, &mut bindings) {
            PatternMatch::NoMatch => continue,
            // an undecidable pattern stops the whole match: falling through to the next case
            // would let it answer a question this one could still have claimed
            PatternMatch::Undecidable => return MatchTypeOutcome::Unresolved,
            PatternMatch::Matched => {}
        }
        let Some(body) = case_body(case) else {
            return MatchTypeOutcome::Unresolved;
        };
        let body =
            alias.apply_own_specialization(db, definition_expression_type(db, definition, body));
        let value = bindings.apply(db, body);
        // a body that names a capture the pattern did not bind — an or-pattern whose
        // alternatives bind different names, say — would otherwise leak that capture's type
        // variable into the alias's value, where it means nothing. the malformed pattern is
        // reported where the alias is written; here it simply has no value
        if mentions_capture_of(db, value, definition) {
            return MatchTypeOutcome::Unresolved;
        }
        return MatchTypeOutcome::Matched(value);
    }

    MatchTypeOutcome::NoCaseMatched
}

/// Whether `ty` is larger than [`MAX_SUBJECT_NODES`] type nodes.
///
/// The walk short-circuits as soon as the budget runs out, so this costs at most the budget
/// however large `ty` actually is.
fn exceeds_budget<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    let remaining = std::cell::Cell::new(MAX_SUBJECT_NODES);
    any_over_type(db, ty, false, |_| match remaining.get() {
        0 => true,
        budget => {
            remaining.set(budget - 1);
            false
        }
    })
}

/// Whether `ty` still mentions a type variable bound by `alias_definition`.
///
/// The alias's own type parameters are substituted before this is asked, so anything left is
/// a `case` capture that escaped its pattern.
fn mentions_capture_of<'db>(
    db: &'db dyn Db,
    ty: Type<'db>,
    alias_definition: Definition<'db>,
) -> bool {
    any_over_type(db, ty, false, |ty| match ty {
        Type::TypeVar(bound_typevar) => {
            bound_typevar.binding_context(db) == BindingContext::Definition(alias_definition)
        }
        _ => false,
    })
}

/// The single type expression a `case` block's body consists of.
///
/// The parser rejects any other body, so `None` here only ever means the case was written
/// wrongly and has already been reported.
fn case_body(case: &ast::MatchCase) -> Option<&ast::Expr> {
    match case.body.as_slice() {
        [ast::Stmt::Expr(expression)] => Some(&expression.value),
        _ => None,
    }
}

/// The type a match type's subject expression denotes, as a sequence.
///
/// `match *Shape:` matches over the elements of the pack, so the subject is the tuple the
/// pack spreads into. Any other subject is matched as itself.
fn subject_type<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    subject: &ast::Expr,
) -> Option<Type<'db>> {
    let ast::Expr::Starred(starred) = subject else {
        return Some(definition_expression_type(db, definition, subject));
    };
    let unpacked = definition_expression_type(db, definition, &starred.value);
    // an unpacked pack is already a tuple once specialized; before that it is the pack's own
    // typevar, which spreads into a variable-length tuple
    if unpacked.exact_tuple_instance_spec(db).is_some() {
        return Some(unpacked);
    }
    let bound_typevar = unpacked.as_typevar()?;
    Some(Type::tuple(Some(TupleType::unpacked_typevartuple(
        db,
        bound_typevar,
    ))))
}

/// The outcome of matching one pattern against one subject type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PatternMatch {
    /// The pattern matches, and its captures have been recorded.
    Matched,
    /// The pattern definitely does not match; the next case may still.
    NoMatch,
    /// Whether the pattern matches cannot be decided. Either the subject's shape is not
    /// pinned down (a variable-length tuple, or a gradual type), or the pattern is one a
    /// type-level match has no meaning for. No later case may be tried, because this one
    /// could still have been the answer.
    Undecidable,
}

/// The types a case's captures were matched to, in the order they were bound.
#[derive(Default)]
struct Bindings<'db> {
    captures: Vec<(BoundTypeVarInstance<'db>, Type<'db>)>,
}

impl<'db> Bindings<'db> {
    /// Records what a capture matched, or reports that the same name was captured twice.
    ///
    /// Python rejects `case (A, A)` outright ("multiple assignments to name"); ruff's parser
    /// does not, so a duplicate reaches here. Answering it would mean picking one of the two
    /// bindings arbitrarily, so it is undecidable instead.
    ///
    /// The comparison is by *name*: each occurrence of `A` is its own definition, and so its
    /// own type variable, which is precisely why the body cannot say which one it meant.
    fn push(
        &mut self,
        db: &'db dyn Db,
        typevar: BoundTypeVarInstance<'db>,
        ty: Type<'db>,
    ) -> PatternMatch {
        if self
            .captures
            .iter()
            .any(|(existing, _)| existing.name(db) == typevar.name(db))
        {
            return PatternMatch::Undecidable;
        }
        self.captures.push((typevar, ty));
        PatternMatch::Matched
    }

    fn checkpoint(&self) -> usize {
        self.captures.len()
    }

    /// Discards the captures recorded since `checkpoint`, for an alternative that turned out
    /// not to match after binding part of itself.
    fn rollback(&mut self, checkpoint: usize) {
        self.captures.truncate(checkpoint);
    }

    /// Substitutes the captures into a case body's type.
    fn apply(&self, db: &'db dyn Db, body: Type<'db>) -> Type<'db> {
        if self.captures.is_empty() {
            return body;
        }
        let generic_context = GenericContext::from_typevar_instances(
            db,
            self.captures.iter().map(|(typevar, _)| *typevar),
        );
        let specialization = generic_context.specialize(
            db,
            self.captures
                .iter()
                .map(|(_, ty)| *ty)
                .collect::<Vec<_>>()
                .as_slice(),
        );
        body.apply_type_mapping_impl(
            db,
            &TypeMapping::ApplySpecialization(ApplySpecialization::TypeAlias(specialization)),
            TypeContext::default(),
            &ApplyTypeMappingVisitor::default(),
        )
    }
}

/// Matches `subject` against one `case` pattern, recording what its captures bind to.
///
/// `bindings` is only meaningful when the result is [`PatternMatch::Matched`].
fn match_pattern<'db>(
    db: &'db dyn Db,
    file: ruff_db::files::File,
    subject: Type<'db>,
    pattern: &ast::Pattern,
    bindings: &mut Bindings<'db>,
) -> PatternMatch {
    match pattern {
        // `case _:` and `case Name:` — the latter captures the whole subject
        ast::Pattern::MatchAs(ast::PatternMatchAs {
            pattern: inner,
            name,
            ..
        }) => {
            if let Some(inner) = inner.as_deref() {
                match match_pattern(db, file, subject, inner, bindings) {
                    PatternMatch::Matched => {}
                    other => return other,
                }
            }
            let Some(name) = name else {
                return PatternMatch::Matched;
            };
            let Some(typevar) = capture_typevar(db, file, name) else {
                return PatternMatch::Undecidable;
            };
            bindings.push(db, typevar, subject)
        }

        ast::Pattern::MatchOr(ast::PatternMatchOr { patterns, .. }) => {
            let mut undecidable = false;
            for pattern in patterns {
                let checkpoint = bindings.checkpoint();
                match match_pattern(db, file, subject, pattern, bindings) {
                    PatternMatch::Matched => return PatternMatch::Matched,
                    PatternMatch::Undecidable => undecidable = true,
                    PatternMatch::NoMatch => {}
                }
                // an alternative that bound part of itself before failing must not leave
                // those captures behind for the alternative that does match
                bindings.rollback(checkpoint);
            }
            if undecidable {
                PatternMatch::Undecidable
            } else {
                PatternMatch::NoMatch
            }
        }

        ast::Pattern::MatchSequence(ast::PatternMatchSequence { patterns, .. }) => {
            match_sequence(db, file, subject, patterns, bindings)
        }

        // `case 2:` — a literal in a shape. `Literal[2]` is what the subject element is, so
        // the comparison is against the literal type the pattern's expression denotes
        ast::Pattern::MatchValue(ast::PatternMatchValue { value, .. }) => {
            let Some(expected) = literal_pattern_type(db, value) else {
                // not a literal type at all — reported where the alias is written
                return PatternMatch::Undecidable;
            };
            decide(subject, subject == expected)
        }

        ast::Pattern::MatchSingleton(ast::PatternMatchSingleton { value, .. }) => {
            let expected = match value {
                ast::Singleton::None => Type::none(db),
                ast::Singleton::True => Type::bool_literal(true),
                ast::Singleton::False => Type::bool_literal(false),
            };
            decide(subject, subject == expected)
        }

        // a class or mapping pattern destructures a *value*; there is nothing at the type
        // level for it to take apart. a bare `*Rest` outside a sequence has no sequence to
        // consume either. all three are rejected where the alias is written, and none of
        // them can answer here
        ast::Pattern::MatchClass(_)
        | ast::Pattern::MatchMapping(_)
        | ast::Pattern::MatchStar(_) => PatternMatch::Undecidable,

        // basedpython: a conjunction matches when every conjunct does, and binds
        // what all of them bind. A conjunct that fails leaves nothing behind
        ast::Pattern::MatchAnd(ast::PatternMatchAnd { patterns, .. }) => {
            let checkpoint = bindings.checkpoint();
            let mut undecidable = false;
            for pattern in patterns {
                match match_pattern(db, file, subject, pattern, bindings) {
                    PatternMatch::Matched => {}
                    PatternMatch::Undecidable => undecidable = true,
                    PatternMatch::NoMatch => {
                        bindings.rollback(checkpoint);
                        return PatternMatch::NoMatch;
                    }
                }
            }
            if undecidable {
                bindings.rollback(checkpoint);
                PatternMatch::Undecidable
            } else {
                PatternMatch::Matched
            }
        }
    }
}

/// Turns a definite comparison into a [`PatternMatch`], unless the subject is gradual.
///
/// `Unknown` stands for a type nobody worked out, so it neither matches a literal pattern nor
/// definitely fails to — treating it as a miss would let a later case answer for it.
fn decide(subject: Type<'_>, matches: bool) -> PatternMatch {
    if matches {
        PatternMatch::Matched
    } else if subject.is_dynamic() {
        PatternMatch::Undecidable
    } else {
        PatternMatch::NoMatch
    }
}

/// Matches a sequence pattern — `()`, `(A, B)`, `(A, *Rest)` — against a tuple subject.
fn match_sequence<'db>(
    db: &'db dyn Db,
    file: ruff_db::files::File,
    subject: Type<'db>,
    patterns: &[ast::Pattern],
    bindings: &mut Bindings<'db>,
) -> PatternMatch {
    let Some(spec) = subject.exact_tuple_instance_spec(db) else {
        // a gradual subject could be a tuple of any shape; anything else definitely is not a
        // tuple at all
        return decide(subject, false);
    };
    // a variable-length subject has no definite length, so no sequence pattern can be decided
    // against it — `()` and `(A, *Rest)` are both still possible
    let Tuple::Fixed(fixed) = spec.as_ref() else {
        return PatternMatch::Undecidable;
    };
    let elements = fixed.all_elements();

    let Some(star_index) = patterns.iter().position(ast::Pattern::is_match_star) else {
        if elements.len() != patterns.len() {
            return PatternMatch::NoMatch;
        }
        return match_all(db, file, patterns, elements, bindings);
    };

    let suffix_len = patterns.len() - star_index - 1;
    if elements.len() < star_index + suffix_len {
        return PatternMatch::NoMatch;
    }
    let (prefix, rest) = elements.split_at(star_index);
    let (starred, suffix) = rest.split_at(rest.len() - suffix_len);

    match match_all(db, file, &patterns[..star_index], prefix, bindings) {
        PatternMatch::Matched => {}
        other => return other,
    }
    match match_all(db, file, &patterns[star_index + 1..], suffix, bindings) {
        PatternMatch::Matched => {}
        other => return other,
    }

    let ast::Pattern::MatchStar(ast::PatternMatchStar { name, .. }) = &patterns[star_index] else {
        return PatternMatch::Undecidable;
    };
    let Some(name) = name else {
        return PatternMatch::Matched;
    };
    let Some(typevar) = capture_typevar(db, file, name) else {
        return PatternMatch::Undecidable;
    };
    bindings.push(
        db,
        typevar,
        Type::heterogeneous_tuple(db, starred.iter().copied()),
    )
}

/// Matches each pattern against the element at the same position, stopping at the first one
/// that does not match or cannot be decided.
fn match_all<'db>(
    db: &'db dyn Db,
    file: ruff_db::files::File,
    patterns: &[ast::Pattern],
    elements: &[Type<'db>],
    bindings: &mut Bindings<'db>,
) -> PatternMatch {
    for (pattern, element) in std::iter::zip(patterns, elements) {
        match match_pattern(db, file, *element, pattern, bindings) {
            PatternMatch::Matched => {}
            other => return other,
        }
    }
    PatternMatch::Matched
}

/// The literal type a value pattern denotes, e.g. `Literal[2]` for `case 2:`.
pub(crate) fn literal_pattern_type<'db>(db: &'db dyn Db, value: &ast::Expr) -> Option<Type<'db>> {
    match value {
        ast::Expr::NumberLiteral(ast::ExprNumberLiteral {
            value: ast::Number::Int(int),
            ..
        }) => int.as_i64().map(Type::int_literal),
        ast::Expr::StringLiteral(string) => Some(Type::string_literal(db, string.value.to_str())),
        ast::Expr::BytesLiteral(bytes) => Some(Type::bytes_literal(
            db,
            &bytes.value.bytes().collect::<Vec<_>>(),
        )),
        ast::Expr::UnaryOp(ast::ExprUnaryOp {
            op: ast::UnaryOp::USub,
            operand,
            ..
        }) => match operand.as_ref() {
            ast::Expr::NumberLiteral(ast::ExprNumberLiteral {
                value: ast::Number::Int(int),
                ..
            }) => int.as_i64().map(|value| Type::int_literal(-value)),
            _ => None,
        },
        _ => None,
    }
}

/// The type variable a capture name introduces.
///
/// This is the same variable the case body was inferred against — the alias's own scope is
/// what binds it — so substituting it here is what turns the symbolic body into a type.
fn capture_typevar<'db>(
    db: &'db dyn Db,
    file: ruff_db::files::File,
    name: &ast::Identifier,
) -> Option<BoundTypeVarInstance<'db>> {
    let index = semantic_index(db, file);
    let definition = index.try_definition(name)?;
    let Type::KnownInstance(KnownInstanceType::TypeVar(typevar)) = binding_type(db, definition)
    else {
        return None;
    };
    bind_typevar(db, index, definition.file_scope(db), None, typevar)
}
