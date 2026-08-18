//! turning what a debugger observed into a type the checker can narrow with
//!
//! [`ty_python_core::assumptions`] records what was read off a running program, in a vocabulary of
//! observations rather than of types. this is where those become [`Type`]s, and it is deliberately
//! the only place that knows how to cross between the two
//!
//! ## an observation that cannot be expressed produces no seed
//!
//! every conversion here is partial, and that is the design rather than a gap. a class in a module
//! that does not resolve, an enum member the class does not have, a name the stopped scope does not
//! bind — each of them answers `None`, and the analysis carries on knowing one thing less
//!
//! the alternative would be inventing a type that is *nearly* what was observed. a seeded analysis
//! that is slightly wrong is worse than one that is silent, because the whole point of seeding is
//! that the answer is grounded in what actually happened. `Truthiness::Ambiguous` already exists
//! for "not known", and it is the honest destination for anything this cannot express

use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::{line_index, source_text};
use ruff_python_ast::{self as ast, name::Name};
use ruff_source_file::OneIndexed;
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::FxHashMap;
use ty_module_resolver::{ModuleName, resolve_module_confident};
use ty_python_core::assumptions::{ClassName, Observed};
use ty_python_core::scope::{FileScopeId, NodeWithScopeRef, ScopeId};
use ty_python_core::{
    BindingWithConstraintsIterator, DefinitionState, PlaceExpr, PlaceExprRef, PlaceTable,
    ProgramFile, ScopedPlaceId, semantic_index,
};

use crate::Db;
use crate::place::{Place, imported_symbol};
use crate::types::context::ProgramEnvironment;
use crate::types::{EnumLiteralType, Type};

/// the type an observation pins a name to, when it pins one at all
pub(crate) fn seeded_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    observed: &Observed,
) -> Option<Type<'db>> {
    match observed {
        Observed::IsNone => Some(Type::none(db, env)),

        Observed::IsBool(value) => Some(Type::bool_literal(*value)),

        // an `int` has no width in python and a literal type does. a value that does not fit is
        // still known to be an `int`, which narrows a `str | int` union even though it cannot
        // narrow a comparison against a literal
        Observed::IsInt(text) => Some(text.parse::<i64>().map_or_else(
            |_| crate::types::KnownClass::Int.to_instance(db, env),
            Type::int_literal,
        )),

        // the float as it was read, every value of it — `inf`, `-inf`, `nan` and `-0.0` included.
        //
        // a literal for those is a *true* statement about the value, and the reading is what gets
        // displayed beside the code, so filtering them here would replace a fact with `float` for
        // no gain. what they are dangerous for is comparison folding, which is a rule about types
        // rather than a statement about a value: `nan` is not equal to itself and `-0.0` *is* equal
        // to `0.0`, so an arm that decided `==` from literal identity would answer both the wrong
        // way. `by` folds `Int`, `Bool`, `String` and `Bytes` and not `Float`, so nothing decides
        // them today — and a seed is neither where that would be decided nor the only way one
        // arrives: basedpython already writes `float.nan` and `±float.inf` as literal types. the
        // warning belongs where such an arm would be written, and is in `types::infer::comparisons`
        //
        // text that will not parse falls back to the class, which is the trade `IsInt` makes for an
        // integer too wide to hold: still narrows a `str | float`, still says less than nothing did
        Observed::IsFloat(text) => Some(text.parse::<f64>().map_or_else(
            |_| crate::types::KnownClass::Float.to_instance(db, env),
            Type::float_literal,
        )),

        Observed::IsStr(text) => Some(Type::string_literal(db, text.as_str())),

        Observed::IsBytes(bytes) => Some(Type::bytes_literal(db, bytes)),

        Observed::IsExactly(class) => {
            resolved_class(db, env, class)?.to_instance_approximation(db, env)
        }

        Observed::IsEnumMember { class, member } => enum_member(db, env, class, member),
    }
}

/// the type of the class object named by module and qualname
///
/// `None` when the module does not resolve, when it has no such name, or when the name is nested
/// inside another class. a nested class is reachable only by reading the outer class's members,
/// which is work the checker does for itself once it has the outer type — and guessing at it here
/// would be this module inventing a class that may not exist
fn resolved_class<'db>(
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

    let Place::Defined(defined) = imported_symbol(db, env, Some(file), &class.qualname, None).place
    else {
        // what the debugger saw is the *generated* python's name, and basedpython renames a
        // `private` declaration on the way out. so a `_Helper` the source does not have is looked
        // up as the `Helper` it came from — see [`private_renames`]
        let source_name = private_renames(db, file).get(&class.qualname)?;
        let Place::Defined(defined) =
            imported_symbol(db, env, Some(file), source_name.as_str(), None).place
        else {
            return None;
        };
        return Some(defined.ty);
    };

    Some(defined.ty)
}

/// the singleton type of one member of one enum
///
/// the class on its own is not what the observation said, and it is not enough to decide anything:
/// narrowing `c is Color.RED` against an instance of `Color` is ambiguous, which is the same answer
/// as having observed nothing. the member is the whole value of the observation, so a member the
/// class does not actually have produces no seed rather than a bare class
fn enum_member<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    class: &ClassName,
    member: &Name,
) -> Option<Type<'db>> {
    let enum_class = resolved_class(db, env, class)?
        .as_class_literal()?
        .into_enum_class(db)?;
    if !enum_class.member_names(db).any(|name| name == member) {
        return None;
    }
    Some(Type::enum_literal(EnumLiteralType::new(
        db,
        enum_class,
        member.clone(),
    )))
}

/// every seed that survives, keyed by the place it pins
///
/// computed once per seeded program rather than per name load, and keyed by `(scope, place)` so
/// that consulting it costs no allocation and no string comparison on the hot path. a program with
/// no assumptions — which is every program that is not a debugger's — produces an empty map and
/// never walks anything
///
/// ## the three refusals
///
/// **a scope that is not the one the program is stopped in.** an observation is what one frame
/// held, and a frame is one scope. seeding any other scope would be applying a reading of `limit`
/// in this function to an unrelated `limit` in that one, so only the innermost scope containing
/// the stop line is seeded at all — see [`stopped_scope`]
///
/// **a name the stopped scope does not itself bind.** a free variable read out of a frame is a
/// global or a closure cell, and what happens to it between the stop and a use below is decided
/// somewhere this scope cannot see. the two checks below both work by looking at the bindings in
/// *this* scope, so a name with none of them would pass them vacuously — which is exactly how a
/// confident wrong answer would get out
///
/// **a binding that can have run since.** a seed describes the state at one line, so it survives to
/// a use below that line only when nothing can have changed what the name holds in between, and
/// there are two ways it can have:
///
/// a binding at or below the stop line is the program's own assignment, and wins over an
/// observation taken before it. the use-def map works out which binding reaches a use, so a seed
/// that survived this check cannot be shadowing one that did not
///
/// a binding inside a loop that encloses the stop line is the one that looks like it should come
/// from the use-def map and does not:
///
/// ```py
/// for item in items:    # binds `item`, above the stop line
///     ...               # ← stopped here
///     if item == 5: ...
/// ```
///
/// `bindings_at_use` reports that the loop's binding reaches the condition, which is true in every
/// iteration — the map does not distinguish one iteration from another, and neither does
/// `ReachableLoopBinding`, which is about which loop-back bindings are *live*. so the check is
/// syntactic: any binding inside a loop that contains the stop line loses its seed, whatever the
/// dataflow says. coarser than the truth and never wrong about it
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

    // every file this one imports is resolved under the same seeded program, so this is what keeps
    // the stop line from being read against a file the debugger was never stopped in
    let source_file = file.file(db);
    if source_file != assumptions.file(db) {
        return seeds;
    }

    let Some(line) = OneIndexed::new(assumptions.line(db) as usize) else {
        return seeds;
    };
    let stop = stop_offset(db, source_file, line);

    let file_scope = scope.file_scope_id(db);
    if *stopped_scope(db, file, stop) != Some(file_scope) {
        return seeds;
    }

    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let enclosing_loops = loops_containing(parsed.suite(), stop);

    let env = ProgramEnvironment::from_scope(scope);
    let index = semantic_index(db, file);
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

    seeds.shrink_to_fit();
    seeds
}

/// where in the source a program stopped on `line` actually is
///
/// the first character of the line that is not indentation, rather than the line's first byte. a
/// debugger reports a line, and everything downstream of that wants an offset — which scope the
/// stop is in, which bindings are behind it, which code is below it.
///
/// the line's first byte was the obvious offset and it was wrong, in a way that only showed on one
/// shape of source. every statement's range starts at its first token, so the indentation in front
/// of it belongs to no statement at all — and `body_contains` asks whether the stop falls between
/// the first statement's start and the last one's end. a stop on the *first* statement of a
/// function body therefore landed just before that body, `stopped_scope` answered with the
/// enclosing scope instead, and every seed was refused for being about another frame:
///
/// ```py
/// def price(qty: int, member: bool):
///     discount = 0.0    # ← stopped here: line_start is in the indent, before `discount`
///     if qty >= 10: ...  # nothing decided. one line further down, everything decided
/// ```
///
/// widening the *body* to include its first line's indentation was the alternative. it loses on a
/// compound statement written on one line — `def f(): return 1`, where the body's first statement
/// shares the header's line, and a stop there would then be read as inside the body rather than on
/// the header. narrowing the stop instead leaves that distinction exactly where it was
///
/// a blank or all-whitespace line has no such character, and answers with the line start. nothing
/// is written there for the answer to be wrong about
pub fn stop_offset(db: &dyn Db, file: ruff_db::files::File, line: OneIndexed) -> TextSize {
    let source = source_text(db, file);
    let start = line_index(db, file).line_start(line, &source);
    let indent = source[usize::from(start)..]
        .find(|character: char| !matches!(character, ' ' | '\t' | '\x0c'))
        .unwrap_or(0);
    start + TextSize::try_from(indent).unwrap_or_default()
}

/// the innermost scope the stop line falls inside
///
/// walked over the syntax rather than asked of the scope tree because the question is "which frame
/// is this", and a frame is a function body or the module body. the scopes that have no statements
/// to stop on — a lambda, a comprehension, a type-parameter list — are not candidates, so a stop
/// inside one of them answers with the function or module that contains it
///
/// `stop` is a [`stop_offset`], not a line start: this walk compares it against statement ranges,
/// which begin at a statement's first token
#[salsa::tracked(heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn stopped_scope<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    stop: TextSize,
) -> Option<FileScopeId> {
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let index = semantic_index(db, file);

    let mut innermost = Some(FileScopeId::global());
    let mut suite: &[ast::Stmt] = parsed.suite();

    'descend: loop {
        for statement in suite {
            let (scope, body) = match statement {
                ast::Stmt::FunctionDef(node) if body_contains(&node.body, stop) => {
                    (NodeWithScopeRef::Function(node), &node.body)
                }
                ast::Stmt::ClassDef(node) if body_contains(&node.body, stop) => {
                    (NodeWithScopeRef::Class(node), &node.body)
                }
                other if other.range().contains(stop) => {
                    // not a scope of its own, but the statement the stop is in may hold one — an
                    // `if` or a `for` around a `def`, say. no suite of it contains the stop when
                    // the stop is on the statement's own header line, and a header belongs to the
                    // scope the statement is written in
                    let Some(inner) = nested_suites(other, stop) else {
                        return innermost;
                    };
                    suite = inner;
                    continue 'descend;
                }
                _ => continue,
            };
            innermost = index.try_node_scope(scope);
            suite = body;
            continue 'descend;
        }
        return innermost;
    }
}

/// whether the stop falls between the first and last statement of a body
fn body_contains(body: &[ast::Stmt], stop: TextSize) -> bool {
    let (Some(first), Some(last)) = (body.first(), body.last()) else {
        return false;
    };
    TextRange::new(first.range().start(), last.range().end()).contains(stop)
}

/// the block of a compound statement that the stop is inside, if any
///
/// only the statement's own suites — a `def` nested in one of them is found by the caller's next
/// pass, which is what keeps the descent one level at a time
fn nested_suites(statement: &ast::Stmt, stop: TextSize) -> Option<&[ast::Stmt]> {
    let suites: Vec<&[ast::Stmt]> = match statement {
        ast::Stmt::If(node) => std::iter::once(&node.body[..])
            .chain(node.elif_else_clauses.iter().map(|clause| &clause.body[..]))
            .collect(),
        ast::Stmt::For(node) => vec![&node.body, &node.orelse],
        ast::Stmt::While(node) => vec![&node.body, &node.orelse],
        ast::Stmt::With(node) => vec![&node.body],
        ast::Stmt::Try(node) => std::iter::once(&node.body[..])
            .chain(node.handlers.iter().map(|handler| {
                let ast::ExceptHandler::ExceptHandler(handler) = handler;
                &handler.body[..]
            }))
            .chain([&node.orelse[..], &node.finalbody[..]])
            .collect(),
        ast::Stmt::Match(node) => node.cases.iter().map(|case| &case.body[..]).collect(),
        _ => return None,
    };

    suites.into_iter().find(|suite| body_contains(suite, stop))
}

/// the place a name or dotted path refers to in one scope's table
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

/// whether this scope binds the place, and every binding of it leaves the observation standing
///
/// the "binds it at all" half is not a formality. a name this scope only reads is a global or a
/// closure cell, and the loop below would then run over nothing and say yes to everything
fn bindings_survive<'db>(
    db: &'db dyn Db,
    bindings: BindingWithConstraintsIterator<'_, 'db>,
    parsed: &ParsedModuleRef,
    stop: TextSize,
    enclosing_loops: &[TextRange],
) -> bool {
    let mut bound_here = false;

    for binding in bindings {
        let DefinitionState::Defined(definition) = binding.binding else {
            continue;
        };
        bound_here = true;

        let range = definition.full_range(db, parsed).range();
        let survives = range.end() <= stop
            && !enclosing_loops
                .iter()
                .any(|loop_| loop_.contains_range(range));
        if !survives {
            return false;
        }
    }

    bound_here
}

/// the ranges of the loops that contain `stop`, innermost last
///
/// walked rather than queried because the question is about syntax: whether the program can arrive
/// at the stop line again with a different value bound
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

/// the type a debugger observed this place holding in this scope, if it observed one
///
/// the lookup the name-load path makes. it is a hash lookup into a map that is empty for every
/// program a checker, a formatter or an editor's ordinary diagnostics run under
pub(crate) fn seeded_place<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    place_expr: PlaceExprRef,
) -> Option<Type<'db>> {
    scope.program(db).assumptions(db)?;
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

/// whether a use is at or below the line the program is stopped on
///
/// a use above it already ran, and ran before the observation was taken — so an observation applied
/// there would be describing the wrong moment. the stop line itself is included, and for the same
/// reason its bindings are excluded by [`bindings_survive`]: nothing on that line has run yet, so a
/// name read there still holds what the debugger saw
pub(crate) fn is_at_or_below_stop_line<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    range: TextRange,
) -> bool {
    let Some(assumptions) = scope.program(db).assumptions(db) else {
        return false;
    };
    let source_file = scope.file(db);
    if source_file != assumptions.file(db) {
        return false;
    }
    let Some(line) = OneIndexed::new(assumptions.line(db) as usize) else {
        return false;
    };
    range.start() >= stop_offset(db, source_file, line)
}

/// module-level names basedpython renames on the way out, generated name first
///
/// a `private class Helper` is `_Helper` in the emitted python, so that is the name a debugger
/// reports for an instance of it — and it is not a name the `.by` source has. without this, a fact
/// about a private class resolves to nothing and the analysis is one seed short
///
/// consulted only after a lookup of the literal name has already failed, so a file that really does
/// declare a `_Helper` alongside a `private Helper` resolves to the one the debugger named
///
/// computed from the source rather than read from `_by_sourcemap.py`. the map is a runtime artefact
/// that lives as long as one `by run`, and this question is about a file the server already has
/// open: `private` is a decorator in the AST, and what it renames to is a rule rather than a
/// lookup. so there is nothing to emit, nothing to keep in step, and it works for a file that has
/// never been run
///
/// only module level. a `private` member of a class is `__name`, which python then mangles per
/// class — a different rule, for names that are attributes rather than the qualnames a fact
/// carries
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

    renamed.shrink_to_fit();
    renamed
}

/// whether a decorator is basedpython's `private` modifier
///
/// the modifier is written as a bare word before the declaration and parses as a decorator whose
/// expression is the name `private`. a call — `@private(...)` — is somebody's own decorator that
/// happens to share the name, and is left alone
fn is_private(decorator: &ast::Decorator) -> bool {
    decorator
        .expression
        .as_name_expr()
        .is_some_and(|name| name.id.as_str() == "private")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::{TestDb, TestDbBuilder};
    use ruff_db::files::system_path_to_file;
    use ty_python_core::assumptions::{Assumptions, Observation};
    use ty_python_core::{TestProgramDb as _, global_scope};

    /// a module whose branch is only decidable from what the program was holding
    ///
    /// `limit` is a parameter of nothing — it is a module-level name bound once, above the stop
    /// line — so a checker reading the source alone can say only that it is an `int`. the
    /// debugger saw a `5`
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

    /// the seeds of the module scope, under one set of observations taken at `line`
    fn seeds_at(db: &TestDb, line: u32, observations: Vec<Observation>) -> usize {
        let file = system_path_to_file(db, "/src/stopped.py").expect("the fixture was written");
        let assumptions = Assumptions::new(db, file, line, observations.into_boxed_slice());
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
    fn a_name_the_stopped_scope_only_reads_is_refused() {
        // the module scope is where the stop is, but `limit` is bound in the function below it and
        // nowhere here. there is no binding in this scope for the checks above to look at, so
        // without this refusal the name would pass them by having nothing to fail
        let db = db_with(
            "\
x = 1
def f():
    limit = compute()
    if limit > 100:
        over = 1
",
        );
        assert_eq!(
            seeds_at(
                &db,
                1,
                vec![observing("limit", Observed::IsInt("5".to_string()))],
            ),
            0,
            "a name the stopped scope does not bind is a free variable, not an observation"
        );
    }

    #[test]
    fn only_the_scope_the_program_is_stopped_in_is_seeded() {
        // the stop is inside `f`, so `f`'s `limit` is the one the observation is about. the
        // module's own `limit` is a different name that happens to be spelled the same
        let db = db_with(
            "\
limit = compute()
def f():
    limit = compute()
    if limit > 100:
        over = 1
",
        );
        assert_eq!(
            seeds_at(
                &db,
                4,
                vec![observing("limit", Observed::IsInt("5".to_string()))],
            ),
            0,
            "the module scope is not the scope the program is stopped in"
        );
    }

    #[test]
    fn a_file_the_program_is_not_stopped_in_is_seeded_nothing() {
        // every module this one imports is read under the same seeded program, so a line number
        // alone would be matched against files the debugger was never in. this fixture is the
        // shape that makes that concrete: both files bind `limit` at the same line, and only one
        // of them is the one the observation came from
        let db = TestDbBuilder::new()
            .with_file("/src/stopped.py", SOURCE)
            .with_file("/src/other.py", SOURCE)
            .build()
            .expect("valid TestDb setup");

        let stopped = system_path_to_file(&db, "/src/stopped.py").expect("the fixture was written");
        let other = system_path_to_file(&db, "/src/other.py").expect("the fixture was written");

        let assumptions = Assumptions::new(
            &db,
            stopped,
            2,
            vec![observing("limit", Observed::IsInt("5".to_string()))].into_boxed_slice(),
        );
        let seeded = db.program().seeded(&db, assumptions);

        assert_eq!(
            seeds(&db, global_scope(&db, seeded.program_file(&db, stopped))).len(),
            1,
            "the file the observation was taken in still gets its seed"
        );
        assert!(
            seeds(&db, global_scope(&db, seeded.program_file(&db, other))).is_empty(),
            "a line number was read against a file the debugger was never stopped in"
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
            seeds_at(
                &db,
                2,
                vec![observing(
                    "limit",
                    Observed::IsExactly(ClassName {
                        module: "no_such_module".to_string(),
                        qualname: "Whatever".to_string(),
                    })
                )],
            ),
            0,
            "a class in a module that does not resolve is a reading this cannot express, and \
             inventing a type that is nearly it would be worse than staying quiet"
        );
    }

    /// which scope a stop on each line of an indented body is read as being in
    fn scope_stopped_in(db: &TestDb, line: u32) -> Option<FileScopeId> {
        let file = system_path_to_file(db, "/src/stopped.py").expect("the fixture was written");
        let stop = stop_offset(
            db,
            file,
            OneIndexed::new(line as usize).expect("a one-based line"),
        );
        *stopped_scope(db, db.program().program_file(db, file), stop)
    }

    #[test]
    fn a_stop_on_the_first_statement_of_a_body_is_read_as_inside_that_body() {
        // the offset a stop is taken at used to be the first byte of the line, which is in the
        // indentation — and a body's extent is measured from its first statement's first *token*.
        // so a stop on line 2 fell just outside `f`, the scope came back as the module, and every
        // seed was refused for being about another frame. a stop on line 3 was fine, which is what
        // made it look like a fault in the analysis rather than in the offset
        let db = db_with(
            "\
def f():
    limit = compute()
    if limit > 100:
        over = 1
",
        );
        let inside = scope_stopped_in(&db, 3);
        assert_ne!(
            inside,
            Some(FileScopeId::global()),
            "the fixture is wrong: line 3 was supposed to be inside `f`"
        );
        assert_eq!(
            scope_stopped_in(&db, 2),
            inside,
            "a stop on the first statement of `f` is in `f`, the same as one on the second"
        );
    }

    #[test]
    fn a_stop_on_a_function_header_is_read_as_outside_it() {
        // the other edge of the same offset. a `def` line is written in the scope that contains
        // the function, not in the function — nothing of the body has been entered yet, and a
        // frame for it does not exist. narrowing the stop to the line's first token rather than
        // widening the body to its first line is what keeps this true
        let db = db_with(
            "\
def f():
    limit = compute()
",
        );
        assert_eq!(
            scope_stopped_in(&db, 1),
            Some(FileScopeId::global()),
            "a stop on `def f():` is in the module, not in `f`"
        );
    }
}
