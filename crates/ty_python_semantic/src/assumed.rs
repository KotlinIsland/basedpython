//! Turning what a debugger observed into a type the checker can narrow with.
//!
//! [`ty_python_core::assumptions`] records what was read off a running program, in a vocabulary of
//! observations rather than of types. This is where those become [`Type`]s, and it is deliberately
//! the only place that knows how to cross between the two.
//!
//! ## An observation that cannot be expressed produces no seed
//!
//! Every conversion here is partial, and that is the design rather than a gap. A `bytes` value too
//! wide for a literal, a class in a module that does not resolve, an integer larger than `i64` —
//! each of them answers `None`, and the analysis carries on knowing one thing less.
//!
//! The alternative would be inventing a type that is *nearly* what was observed. A seeded analysis
//! that is slightly wrong is worse than one that is silent, because the whole point of seeding is
//! that the answer is grounded in what actually happened. `Truthiness::Ambiguous` already exists
//! for "not known", and it is the honest destination for anything this cannot express.
//!
//! ## What is deliberately not converted
//!
//! [`Observed::HasLength`] and [`Observed::IsTruthy`] have no independent type to become. A length
//! is a property of a value, not a set of values, and the truthiness of an object of unknown type
//! is not expressible as a type at all. They ride along with the observations that *are*
//! convertible — a literal already carries both — and on their own they are recorded and unused.

use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::{line_index, source_text};
use ruff_python_ast::{self as ast, name::Name};
use ruff_source_file::OneIndexed;
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::FxHashMap;
use ty_module_resolver::{ModuleName, resolve_module_confident};
use ty_python_core::assumptions::{ClassName, Observed};
use ty_python_core::scope::ScopeId;
use ty_python_core::{
    BindingWithConstraintsIterator, DefinitionState, PlaceExpr, PlaceExprRef, PlaceTable,
    ProgramFile, ScopedPlaceId, semantic_index,
};

use crate::Db;
use crate::place::{Place, imported_symbol};
use crate::types::Type;
use crate::types::context::ProgramEnvironment;

/// The type an observation pins a name to, when it pins one at all.
pub(crate) fn seeded_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    observed: &Observed,
) -> Option<Type<'db>> {
    match observed {
        Observed::IsNone => Some(Type::none(db, env)),

        Observed::IsBool(value) => Some(Type::bool_literal(*value)),

        // An `int` has no width in Python and a literal type does. A value that does not fit is
        // still known to be an `int`, which narrows a `str | int` union even though it cannot
        // narrow a comparison against a literal
        Observed::IsInt(text) => Some(text.parse::<i64>().map_or_else(
            |_| crate::types::KnownClass::Int.to_instance(db, env),
            Type::int_literal,
        )),

        Observed::IsStr(text) => Some(Type::string_literal(db, text.as_str())),

        Observed::IsBytes(bytes) => Some(Type::bytes_literal(db, bytes)),

        Observed::IsExactly(class) => instance_of(db, env, class),

        // The class is what a `match` or an `is` comparison against a member narrows against, and
        // resolving the member itself would mean reading the enum's own definition — which the
        // checker does anyway once it knows the class
        Observed::IsEnumMember { class, .. } => instance_of(db, env, class),

        // No type of their own — see the module docs
        Observed::HasLength(_) | Observed::IsTruthy(_) => None,
    }
}

/// The instance type of a class named by module and qualname.
///
/// `None` when the module does not resolve, when it has no such name, or when the name is nested
/// inside another class. A nested class is reachable only by reading the outer class's members,
/// which is work the checker does for itself once it has the outer type — and guessing at it here
/// would be this module inventing a class that may not exist.
fn instance_of<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    class: &ClassName,
) -> Option<Type<'db>> {
    if class.qualname.contains('.') {
        return None;
    }

    let module = ModuleName::new(&class.module)?;
    let resolved = resolve_module_confident(db, env.resolver_environment(db), &module)?;
    let file = ProgramFile::new(db, resolved.file(db)?, env.program(db));

    // What the debugger saw is the *generated* python's name, and basedpython renames a
    // `private` declaration on the way out. So a `_Helper` with no such name in the source is
    // looked up as the `Helper` it came from — see [`private_renames`]
    let source_name = private_renames(db, file)
        .get(&class.qualname)
        .map_or(class.qualname.as_str(), Name::as_str);

    let Place::Defined(defined) = imported_symbol(db, env, Some(file), source_name, None).place
    else {
        return None;
    };

    // The approximation rather than the exact projection: a seed is a *source* of a type — the
    // thing narrowing starts from — and never the target of a subtype check, which is the case
    // the exactness is there to protect
    defined.ty.to_instance_approximation(db, env)
}

/// Whether a name is one this program has an observation for.
///
/// Cheap enough to ask on every name load, which is where it is asked: a program with no
/// assumptions — every program that is not a debugger's — answers `false` without looking at
/// anything.
pub(crate) fn observation_for<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    name: &Name,
) -> Option<&'db Observed> {
    env.program(db).assumptions(db)?.observed(db, name)
}

/// Every seed that survives, keyed by the place it pins.
///
/// Computed once per seeded program rather than per name load, and keyed by `(scope, place)` so
/// that consulting it costs no allocation and no string comparison on the hot path. A program with
/// no assumptions — which is every program that is not a debugger's — produces an empty map and
/// never walks anything.
///
/// ## The two refusals
///
/// A seed describes the state at one line. It survives to a use below that line only when nothing
/// can have changed what the name holds in between, and there are exactly two ways it can have:
///
/// **A binding at or below the stop line.** The program's own assignment wins over an observation
/// taken before it. The use-def map works out which binding reaches a use, so a seed that survived
/// this check cannot be shadowing one that did not.
///
/// **A binding inside a loop that encloses the stop line.** This is the one that looks like it
/// should come from the use-def map and does not:
///
/// ```py
/// for item in items:    # binds `item`, above the stop line
///     ...               # ← stopped here
///     if item == 5: ...
/// ```
///
/// `bindings_at_use` reports that the loop's binding reaches the condition, which is true in every
/// iteration — the map does not distinguish one iteration from another, and neither does
/// `ReachableLoopBinding`, which is about which loop-back bindings are *live*. So the check is
/// syntactic: any binding inside a loop that contains the stop line loses its seed, whatever the
/// dataflow says. Coarser than the truth and never wrong about it.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn seeds<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
) -> FxHashMap<ScopedPlaceId, Type<'db>> {
    let mut seeds = FxHashMap::default();

    let file = scope.program_file(db);
    let Some(assumptions) = file.program(db).assumptions(db) else {
        return seeds;
    };
    let observations = assumptions.observations(db);
    if observations.is_empty() {
        return seeds;
    }

    let source_file = file.file(db);
    let source = source_text(db, source_file);
    let Some(line) = OneIndexed::new(*assumptions.line(db) as usize) else {
        return seeds;
    };
    let stop = line_index(db, source_file).line_start(line, &source);

    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let enclosing_loops = loops_containing(parsed.suite(), stop);

    let env = ProgramEnvironment::from_scope(scope);
    let index = semantic_index(db, file);
    let file_scope = scope.file_scope_id(db);
    let table = index.place_table(file_scope);
    let use_def = index.use_def_map(file_scope);

    for observation in observations {
        let Some(place_id) = place_id_of(table, &observation.name) else {
            continue;
        };
        let bindings = use_def.reachable_bindings(place_id);
        if !bindings_survive(db, bindings, &parsed, stop, &enclosing_loops) {
            continue;
        }
        let Some(ty) = seeded_type(db, &env, &observation.observed) else {
            continue;
        };
        seeds.insert(place_id, ty);
    }

    seeds
}

/// The place a name or dotted path refers to in one scope's table.
fn place_id_of(table: &PlaceTable, name: &Name) -> Option<ScopedPlaceId> {
    let mut segments = name.split('.');
    let root = Name::new(segments.next()?);
    let members: Vec<Name> = segments.map(Name::new).collect();

    if members.is_empty() {
        return table.symbol_id(root.as_str()).map(Into::into);
    }
    let member = PlaceExpr::from_symbol_with_members(&root, &members)?;
    table.place_id(&member)
}

/// Whether every binding of a place leaves the observation standing.
fn bindings_survive<'db>(
    db: &'db dyn Db,
    bindings: BindingWithConstraintsIterator<'_, 'db>,
    parsed: &ParsedModuleRef,
    stop: TextSize,
    enclosing_loops: &[TextRange],
) -> bool {
    bindings.into_iter().all(|binding| {
        let DefinitionState::Defined(definition) = binding.binding else {
            return true;
        };
        let range = definition.full_range(db, parsed).range();
        range.end() <= stop
            && !enclosing_loops
                .iter()
                .any(|loop_| loop_.contains_range(range))
    })
}

/// The ranges of the loops that contain `stop`, innermost last.
///
/// Walked rather than queried because the question is about syntax: whether the program can arrive
/// at the stop line again with a different value bound.
fn loops_containing(suite: &[ast::Stmt], stop: TextSize) -> Vec<TextRange> {
    let mut found = Vec::new();
    collect_loops(suite, stop, &mut found);
    found
}

fn collect_loops(suite: &[ast::Stmt], stop: TextSize, found: &mut Vec<TextRange>) {
    for statement in suite {
        if !statement.range().contains(stop) {
            continue;
        }
        match statement {
            ast::Stmt::For(node) => {
                found.push(node.range());
                collect_loops(&node.body, stop, found);
                collect_loops(&node.orelse, stop, found);
            }
            ast::Stmt::While(node) => {
                found.push(node.range());
                collect_loops(&node.body, stop, found);
                collect_loops(&node.orelse, stop, found);
            }
            ast::Stmt::If(node) => {
                collect_loops(&node.body, stop, found);
                for clause in &node.elif_else_clauses {
                    collect_loops(&clause.body, stop, found);
                }
            }
            ast::Stmt::With(node) => collect_loops(&node.body, stop, found),
            ast::Stmt::Try(node) => {
                collect_loops(&node.body, stop, found);
                for handler in &node.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_loops(&handler.body, stop, found);
                }
                collect_loops(&node.orelse, stop, found);
                collect_loops(&node.finalbody, stop, found);
            }
            ast::Stmt::FunctionDef(node) => collect_loops(&node.body, stop, found),
            ast::Stmt::ClassDef(node) => collect_loops(&node.body, stop, found),
            ast::Stmt::Match(node) => {
                for case in &node.cases {
                    collect_loops(&case.body, stop, found);
                }
            }
            _ => {}
        }
    }
}

/// The type a debugger observed this place holding in this scope, if it observed one.
///
/// The lookup the name-load path makes. It is a hash lookup into a map that is empty for every
/// program a checker, a formatter or an editor's ordinary diagnostics run under.
pub(crate) fn seeded_place<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    place_expr: PlaceExprRef,
) -> Option<Type<'db>> {
    if scope.program(db).assumptions(db).is_none() {
        return None;
    }
    let seeds = seeds(db, scope);
    if seeds.is_empty() {
        return None;
    }
    let index = semantic_index(db, scope.program_file(db));
    let place_id = index
        .place_table(scope.file_scope_id(db))
        .place_id(place_expr)?;
    seeds.get(&place_id).copied()
}

/// Whether a use is below the line the program is stopped on.
///
/// A use above it already ran, and ran before the observation was taken — so an observation
/// applied there would be describing the wrong moment. The stop line itself counts as above:
/// the statement on it has not finished.
pub(crate) fn is_below_stop_line<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    range: TextRange,
) -> bool {
    let Some(assumptions) = scope.program(db).assumptions(db) else {
        return false;
    };
    let source_file = scope.file(db);
    let source = source_text(db, source_file);
    let Some(line) = OneIndexed::new(*assumptions.line(db) as usize) else {
        return false;
    };
    range.start() >= line_index(db, source_file).line_start(line, &source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::{TestDb, TestDbBuilder};
    use ruff_db::files::system_path_to_file;
    use ty_python_core::assumptions::{Assumptions, Observation};
    use ty_python_core::{TestProgramDb as _, global_scope};

    /// A module whose branch is only decidable from what the program was holding.
    ///
    /// `limit` is a parameter of nothing — it is a module-level name bound once, above the stop
    /// line — so a checker reading the source alone can say only that it is an `int`. The
    /// debugger saw a `5`.
    const SOURCE: &str = "\
limit = compute()
if limit > 100:
    over = 1
";

    fn db_with(source: &str) -> TestDb {
        TestDbBuilder::new()
            .with_file("/src/stopped.py", source)
            .build()
            .expect("valid TestDb setup")
    }

    /// The seeds of the module scope, under one set of observations taken at `line`.
    fn seeds_at(db: &TestDb, line: u32, observations: Vec<Observation>) -> usize {
        let file = system_path_to_file(db, "/src/stopped.py").expect("the fixture was written");
        let assumptions = Assumptions::new(db, line, observations.into_boxed_slice());
        let seeded = db.program().seeded(db, assumptions);
        let scope = global_scope(db, seeded.program_file(db, file));
        seeds(db, scope).len()
    }

    fn observing(name: &str, observed: Observed) -> Observation {
        Observation {
            name: Name::new(name),
            observed,
        }
    }

    #[test]
    fn an_observation_taken_above_a_use_is_seeded() {
        let db = db_with(SOURCE);
        assert_eq!(
            seeds_at(
                &db,
                2,
                vec![observing("limit", Observed::IsInt("5".to_string()))],
            ),
            1,
            "`limit` is bound on line 1, the stop is on line 2, and nothing rebinds it"
        );
    }

    #[test]
    fn an_observation_a_later_binding_would_overwrite_is_refused() {
        // the stop is *above* the binding, so what the debugger saw is not what the condition
        // will read — the program's own assignment happens in between
        let db = db_with(SOURCE);
        assert_eq!(
            seeds_at(
                &db,
                1,
                vec![observing("limit", Observed::IsInt("5".to_string()))],
            ),
            0,
            "a binding at or below the stop line wins over an observation taken before it"
        );
    }

    #[test]
    fn a_name_bound_by_a_loop_around_the_stop_line_is_refused() {
        // `item` is bound above the stop line and rebound by the back edge, so an observation of
        // it is true for this iteration and false for the next. the use-def map cannot see the
        // difference; this refusal is what stands in for that
        let db = db_with(
            "\
for item in [1, 2, 3]:
    here = item
    if item > 2:
        big = 1
",
        );
        assert_eq!(
            seeds_at(
                &db,
                2,
                vec![observing("item", Observed::IsInt("1".to_string()))],
            ),
            0,
            "a loop enclosing the stop line can rebind the name before the condition runs again"
        );
    }

    #[test]
    fn a_program_with_no_assumptions_seeds_nothing_and_walks_nothing() {
        let db = db_with(SOURCE);
        let file = system_path_to_file(&db, "/src/stopped.py").expect("the fixture was written");
        let scope = global_scope(&db, db.program().program_file(&db, file));
        assert!(
            seeds(&db, scope).is_empty(),
            "every program that is not a debugger's carries no assumptions at all"
        );
    }

    #[test]
    fn an_observation_no_type_can_express_seeds_nothing() {
        let db = db_with(SOURCE);
        assert_eq!(
            seeds_at(&db, 2, vec![observing("limit", Observed::HasLength(3))]),
            0,
            "a length is a property of a value rather than a set of them, so there is no type \
             for it to become and no seed to make"
        );
    }
}

/// Module-level names basedpython renames on the way out, generated name first.
///
/// A `private class Helper` is `_Helper` in the emitted python, so that is the name a debugger
/// reports for an instance of it — and it is not a name the `.by` source has. Without this, a fact
/// about a private class resolves to nothing and the analysis is one seed short.
///
/// Computed from the source rather than read from `_by_sourcemap.py`. The map is a runtime artefact
/// that lives as long as one `by run`, and this question is about a file the server already has
/// open: `private` is a decorator in the AST, and what it renames to is a rule rather than a
/// lookup. So there is nothing to emit, nothing to keep in step, and it works for a file that has
/// never been run.
///
/// Only module level. A `private` member of a class is `__name`, which python then mangles per
/// class — a different rule, for names that are attributes rather than the qualnames a fact
/// carries.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn private_renames<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> FxHashMap<String, Name> {
    let mut renamed = FxHashMap::default();
    let parsed = parsed_module(db, file.python_file(db)).load(db);

    for statement in parsed.suite() {
        let (name, decorators) = match statement {
            ast::Stmt::ClassDef(node) => (&node.name, &node.decorator_list),
            ast::Stmt::FunctionDef(node) => (&node.name, &node.decorator_list),
            _ => continue,
        };
        if decorators.iter().any(is_private) {
            renamed.insert(format!("_{name}"), Name::new(name.as_str()));
        }
    }
    renamed
}

/// Whether a decorator is basedpython's `private` modifier.
///
/// The modifier is written as a bare word before the declaration and parses as a decorator whose
/// expression is the name `private`. A call — `@private(...)` — is somebody's own decorator that
/// happens to share the name, and is left alone.
fn is_private(decorator: &ast::Decorator) -> bool {
    decorator
        .expression
        .as_name_expr()
        .is_some_and(|name| name.id.as_str() == "private")
}
