//! basedpython: a call whose result is thrown away.
//!
//! A call on a line of its own keeps only what the call did on the way to its
//! answer, and discards the answer. For most functions that is a mistake —
//! `text.strip()` on its own line looks like it strips `text`, and strips
//! nothing — so basedpython reports it, in the same spirit as kotlin's
//! return-value checker.
//!
//! Some results genuinely are optional: `list.pop()` called to shorten a list,
//! a fluent method returning `self`, a cache primed for its side effect. Those
//! declarations say so with `@ignorable_return_value`, and a member of a class
//! that says so says otherwise with `@must_use_return_value`.

use ruff_python_ast as ast;
use ty_python_core::semantic_index;

use crate::Db;
use crate::types::class::{ClassLiteral, ClassType, StaticClassLiteral};
use crate::types::diagnostic::UNUSED_RETURN_VALUE;
use crate::types::function::{FunctionDecorators, FunctionType, KnownFunction};
use crate::types::infer::{TypeInferenceBuilder, nearest_enclosing_class};
use crate::types::{MemberLookupPolicy, ProgramEnvironment, Type};

/// The call an expression statement discards the result of, if it is one.
///
/// `await f()` is one call, not two: the coroutine `f()` builds is what the
/// `await` consumes, and the value that reaches the end of the statement — the
/// one being discarded — is what the awaited coroutine returned.
fn discarded_call(value: &ast::Expr) -> Option<&ast::ExprCall> {
    match value {
        ast::Expr::Call(call) => Some(call),
        ast::Expr::Await(await_expression) => await_expression.value.as_call_expr(),
        _ => None,
    }
}

/// Whether a call to `function` may have its result thrown away.
///
/// Tracked because answering it for a function defined in a class body reads
/// that module's semantic index, and this is asked once per discarded call.
#[salsa::tracked]
fn function_result_is_ignorable<'db>(db: &'db dyn Db, function: FunctionType<'db>) -> bool {
    if function.has_known_decorator(db, FunctionDecorators::MUST_USE_RETURN_VALUE) {
        return false;
    }
    if function.has_known_decorator(db, FunctionDecorators::IGNORABLE_RETURN_VALUE) {
        return true;
    }
    // a method inherits the marker from the class body it is defined in, which
    // is how one `@ignorable_return_value` covers every method of a fluent
    // builder
    let overload = function.literal(db).last_definition;
    let index = semantic_index(db, overload.program_file(db));
    let Some(class) = nearest_enclosing_class(db, index, overload.body_scope(db)) else {
        return false;
    };
    if class_result_is_ignorable(db, class) {
        return true;
    }
    overridden_result_is_ignorable(db, class, function.name(db))
}

/// Whether the member `name` overrides answers that its result may be thrown
/// away.
///
/// A caller holding the base class was allowed to drop what the base declared,
/// and an override cannot take that back — `os._Environ.setdefault` is the
/// `MutableMapping.setdefault` a caller wrote against. An override that really
/// does produce a result worth keeping says so with `@must_use_return_value`,
/// which is read before this.
fn overridden_result_is_ignorable<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> bool {
    let literal = ClassLiteral::Static(class);
    let mut ancestors = literal.iter_mro(db);
    // the class itself declares the override; what it overrides is above it
    ancestors.next();

    let env = ProgramEnvironment::from_scope(class.body_scope(db));
    let overridden =
        literal.class_member_from_mro(db, &env, name, MemberLookupPolicy::default(), ancestors);
    match overridden.place.ignore_possibly_undefined() {
        Some(Type::FunctionLiteral(overridden)) => *function_result_is_ignorable(db, overridden),
        _ => false,
    }
}

/// Whether constructing `class`, or calling a method it defines, may have its
/// result thrown away.
fn class_result_is_ignorable<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> bool {
    class
        .known_function_decorators(db)
        .any(|decorator| decorator == KnownFunction::IgnorableReturnValue)
}

/// What the thing being called is called, where it has a name worth printing.
fn callee_name<'db>(db: &'db dyn Db, callee: Type<'db>) -> Option<&'db str> {
    match callee {
        Type::FunctionLiteral(function) => Some(function.name(db)),
        Type::BoundMethod(bound) => Some(bound.function(db).name(db)),
        Type::ClassLiteral(class) => Some(class.name(db)),
        Type::GenericAlias(alias) => Some(ClassType::Generic(alias).class_literal(db).name(db)),
        _ => None,
    }
}

/// Whether the declaration behind `callee` allows a call to it to be discarded.
///
/// A callable with no declaration behind it — a `Callable[[], int]` parameter,
/// an instance with a `__call__` — has nothing that could carry the marker, so
/// it keeps the default.
fn callee_result_is_ignorable<'db>(db: &'db dyn Db, callee: Type<'db>) -> bool {
    match callee {
        Type::FunctionLiteral(function) => *function_result_is_ignorable(db, function),
        Type::BoundMethod(bound) => *function_result_is_ignorable(db, bound.function(db)),
        Type::ClassLiteral(class) => class
            .as_static()
            .is_some_and(|class| class_result_is_ignorable(db, class)),
        Type::GenericAlias(alias) => ClassType::Generic(alias)
            .class_literal(db)
            .as_static()
            .is_some_and(|class| class_result_is_ignorable(db, class)),
        _ => false,
    }
}

impl<'db> TypeInferenceBuilder<'db, '_> {
    /// Report an expression statement that drops the result of a call.
    ///
    /// `result` is the type of the whole statement expression, which for
    /// `await f()` is what the coroutine resolved to rather than the coroutine.
    /// An unawaited coroutine never reaches here: a missing `await` is
    /// `unused-awaitable`'s to report.
    pub(super) fn check_unused_return_value(&mut self, value: &ast::Expr, result: Type<'db>) {
        if !self.context.is_lint_enabled(&UNUSED_RETURN_VALUE) {
            return;
        }
        // `None` is the answer of a function that has no answer, so discarding it discards
        // nothing, and `Never` is the answer of a call that never comes back. A gradual result
        // is not known to be anything at all, so nothing is known to have been discarded —
        // which holds just as much for a gradual result with a `None` in it, the shape a call
        // on an untyped receiver answers with (`Path.mkdir` reached through an unannotated
        // path is `Any | None`). The most specific fully static form the result could take
        // covers all of those at once: when even that carries no value, there is nothing to
        // report.
        let discarded = result.bottom_materialization(self.db(), self.program_environment());
        if discarded.is_none(self.db()) || discarded.is_never() {
            return;
        }
        let Some(call) = discarded_call(value) else {
            return;
        };

        let db = self.db();
        let callee = self.expression_type(&call.func);
        if callee_result_is_ignorable(db, callee) {
            return;
        }

        let Some(builder) = self.context.report_lint(&UNUSED_RETURN_VALUE, value) else {
            return;
        };
        let mut diagnostic = builder.into_diagnostic("The result of this call is unused");
        let env = self.program_environment();
        match callee_name(db, callee) {
            Some(name) => {
                diagnostic.info(format_args!(
                    "`{name}` returns `{}`",
                    result.display(db, env)
                ));
                diagnostic.help(format_args!(
                    "Decorate `{name}` with `@ignorable_return_value` if discarding its result is expected"
                ));
            }
            None => {
                diagnostic.info(format_args!(
                    "the call returns `{}`",
                    result.display(db, env)
                ));
            }
        }
    }
}
