//! basedpython: a reflected dunder that python asks before the left operand's own dunder
//!
//! when the right operand's class is a subclass of the left operand's, python asks the right
//! operand first: for a binary operator where the subclass answers the reflected dunder
//! differently, and for a comparison whatever it answers it with
//!
//! ```python
//! class A:
//!     def __add__(self, other: object) -> int: ...
//!
//! class B(A):
//!     def __radd__(self, other: object) -> str: ...
//!
//! def f(a1: A, a2: A):
//!     a1 + a2  # `int` by `A.__add__`, but `B.__radd__` answers it first when `a2` is a `B`
//! ```
//!
//! code typed against `A` can't see that coming, so a class may answer a reflected dunder
//! differently from an ancestor only with what that ancestor's own dunder answers: `B.__radd__`
//! has to return an `int`, or hand the operand back with `NotImplemented`, after which python
//! asks `A.__add__`

use ruff_python_ast as ast;
use ruff_python_ast::helpers::TypeModifier;
use ruff_text_size::{Ranged, TextRange};

use super::call::CallArguments;
use super::class::{ClassLiteral, ClassType, StaticClassLiteral};
use super::class_base::ClassBase;
use super::context::InferContext;
use super::diagnostic::INVALID_REFLECTED_DUNDER;
use super::operator_domain::{DunderStep, tests_its_operand};
use super::restricted::RestrictedType;
use super::{Type, TypeContext};
use crate::{Db, ProgramEnvironment};

/// the operators whose right operand python may ask first, each as its dunder, the reflected
/// dunder python asks of the right operand, and how the operator is written. `__eq__` and `__ne__`
/// are their own reflections, and an override of them is checked as an override
const REFLECTED_OPERATORS: [(&str, &str, &str); 17] = [
    ("__add__", "__radd__", "+"),
    ("__sub__", "__rsub__", "-"),
    ("__mul__", "__rmul__", "*"),
    ("__matmul__", "__rmatmul__", "@"),
    ("__truediv__", "__rtruediv__", "/"),
    ("__floordiv__", "__rfloordiv__", "//"),
    ("__mod__", "__rmod__", "%"),
    ("__pow__", "__rpow__", "**"),
    ("__lshift__", "__rlshift__", "<<"),
    ("__rshift__", "__rrshift__", ">>"),
    ("__and__", "__rand__", "&"),
    ("__xor__", "__rxor__", "^"),
    ("__or__", "__ror__", "|"),
    ("__lt__", "__gt__", "<"),
    ("__le__", "__ge__", "<="),
    ("__gt__", "__lt__", ">"),
    ("__ge__", "__le__", ">="),
];

/// report each reflected dunder of `class` that python asks before an ancestor's own dunder, and
/// that answers with something that ancestor's dunder doesn't
///
/// the class may have the reflected dunder from a base rather than its own body: in
/// `class C(B, A)`, `B.__radd__` answers `A() + C()` before `A.__add__`, though `B` alone has no
/// `A` to conform to. where the class that defines the reflected dunder already has the ancestor,
/// that class is where it is reported, so its subclasses aren't
pub(crate) fn check_reflected_dunders<'db>(
    context: &InferContext<'db, '_>,
    class: StaticClassLiteral<'db>,
    class_node: &ast::StmtClassDef,
) {
    let db = context.db();
    let env = context.program_environment();
    // the check belongs with the other operator checks: a `.by` class always has it, and a `.py`
    // class where its file's operator dunders are checked. elsewhere a `.py` class is read as ty
    // reads it, which asks nothing of a reflected dunder
    let file = context.file();
    if !context.is_lint_enabled(&INVALID_REFLECTED_DUNDER)
        || !(file.source_type(db).is_basedpython()
            || db.analysis_settings(file).checked_operator_operands)
    {
        return;
    }
    // the class as its own body sees it, so that a type parameter stays itself: `B[T]`, whose
    // ancestors are `A[T]`, rather than `B[Unknown]`, whose `Unknown` answers anything
    let class_type = class.identity_specialization(db);
    let instance = Type::instance(db, env, class_type);
    let mro: Vec<ClassType<'db>> = class_type
        .iter_mro(db)
        .filter_map(|base| match base {
            ClassBase::Class(ancestor) => Some(ancestor),
            _ => None,
        })
        .collect();

    for (dunder, reflected_dunder, symbol) in REFLECTED_OPERATORS {
        let reflected = Type::from(class_type)
            .member(db, env, reflected_dunder)
            .place;
        if reflected.is_undefined() {
            continue;
        }
        // python asks a comparison's reflected dunder first for any subclass, even one that
        // inherits it. a binary operator's, only where the subclass answers it differently
        let is_comparison = matches!(dunder, "__lt__" | "__le__" | "__gt__" | "__ge__");
        let answers_alike = |ancestor: ClassType<'db>, name: &str| {
            Type::from(ancestor)
                .member(db, env, name)
                .place
                .is_equal_ignoring_provenance(Type::from(class_type).member(db, env, name).place)
        };
        // the furthest class this one has the reflected dunder from, without a class in between
        // answering it differently
        let owner = mro
            .iter()
            .take_while(|ancestor| answers_alike(**ancestor, reflected_dunder))
            .last()
            .copied()
            .unwrap_or(class_type);
        let owner_mro: Vec<ClassLiteral<'db>> = owner
            .iter_mro(db)
            .filter_map(|base| match base {
                ClassBase::Class(ancestor) => Some(ancestor.class_literal(db)),
                _ => None,
            })
            .collect();
        for (index, ancestor) in mro.iter().enumerate() {
            if index == 0 {
                // a comparison asks a subclass's reflected dunder first even where the subclass
                // inherits it, so a class whose own two comparisons disagree is wrong as soon as
                // anything subclasses it
                // every comparison is another's reflection, so its own reflected dunder covers
                // a disagreement either way round
                if !is_comparison || class_type.is_final(db) || owner != class_type {
                    continue;
                }
            } else {
                if !is_comparison && answers_alike(*ancestor, reflected_dunder) {
                    continue;
                }
                // the class that defines the reflected dunder answers for this ancestor itself
                if owner != class_type && owner_mro.contains(&ancestor.class_literal(db)) {
                    continue;
                }
            }
            let Some(wrong) = conformance(db, env, *ancestor, instance, dunder, reflected_dunder)
            else {
                continue;
            };
            let range = if owner == class_type {
                own_definition(class_node, reflected_dunder)
                    .unwrap_or_else(|| class.header_range(db))
            } else {
                class.header_range(db)
            };
            if let Some(builder) = context.report_lint(&INVALID_REFLECTED_DUNDER, range) {
                let class_name = class.name(db);
                let ancestor_name = ancestor.name(db);
                // `A() + B()`, or for a class asked about its own subclasses, `A() < x`
                let (operation, subclass) = if index == 0 {
                    (
                        format!("{ancestor_name}() {symbol} x"),
                        ", for `x` a subclass,",
                    )
                } else {
                    (format!("{ancestor_name}() {symbol} {class_name}()"), "")
                };
                let mut diagnostic = match wrong {
                    Wrong::Answers { found, expected } => builder.into_diagnostic(format_args!(
                        "`{class_name}.{reflected_dunder}` answers `{operation}`{subclass} with \
                         `{}`, where `{ancestor_name}.{dunder}` answers `{}`",
                        found.display(db, env),
                        expected.display(db, env),
                    )),
                    Wrong::NotAccepted => builder.into_diagnostic(format_args!(
                        "`{class_name}.{reflected_dunder}` runs with an `{ancestor_name}` it \
                         doesn't accept, for `{operation}`{subclass}",
                    )),
                };
                if index == 0 {
                    diagnostic.info(format_args!(
                        "python asks a subclass's `{reflected_dunder}` before `{dunder}` even \
                         when it inherits it, so for a `{class_name}` it has to answer as \
                         `{class_name}.{dunder}` does, unless `{class_name}` is `@final`",
                    ));
                } else {
                    diagnostic.info(format_args!(
                        "python asks a subclass's `{reflected_dunder}` before \
                         `{ancestor_name}.{dunder}`, so for an `{ancestor_name}` it has to answer \
                         as `{ancestor_name}.{dunder}` does, or not take an `{ancestor_name}`",
                    ));
                }
            }
            // one ancestor is enough to show the reflected dunder is wrong
            break;
        }
    }
}

/// how a reflected dunder fails an ancestor's dunder
enum Wrong<'db> {
    /// it answers with `found`, which the ancestor's dunder doesn't
    Answers {
        found: Type<'db>,
        expected: Type<'db>,
    },
    /// it takes the ancestor but doesn't accept it, and runs with it anyway
    NotAccepted,
}

/// how `instance`'s `reflected_dunder` fails `ancestor`'s `dunder`, asked first with an operand
/// whose class is exactly `ancestor`, if it does
fn conformance<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ancestor: ClassType<'db>,
    instance: Type<'db>,
    dunder: &str,
    reflected_dunder: &str,
) -> Option<Wrong<'db>> {
    // the left operand is exactly the ancestor: python asks the right operand first only when
    // its class is a subclass of the left operand's
    let left = RestrictedType::from_type_expression(
        db,
        env,
        TypeModifier::Final,
        Type::instance(db, env, ancestor),
    );
    let expected = left
        .try_call_dunder(
            db,
            env,
            dunder,
            CallArguments::positional([instance]),
            TypeContext::default(),
        )
        .ok()?
        .return_type(db, env)
        .without_not_implemented(db);
    let step = DunderStep::of(db, env, left, |operand| {
        instance
            .try_call_dunder(
                db,
                env,
                reflected_dunder,
                CallArguments::positional([operand]),
                TypeContext::default(),
            )
            .map(|bindings| (bindings.return_type(db, env), false))
    });
    match step {
        DunderStep::Rejects => None,
        DunderStep::Accepts { answer, .. } => {
            let found = answer.without_not_implemented(db);
            (!found.is_assignable_to(db, env, expected))
                .then_some(Wrong::Answers { found, expected })
        }
        // only a plain method is held to handing back an operand it doesn't take: a comparison
        // a dataclass writes, say, hands back an operand of another class, whatever its type says
        DunderStep::NotAccepted(_) => {
            tests_operand(db, env, instance, reflected_dunder).then_some(Wrong::NotAccepted)
        }
    }
}

/// whether `instance`'s `name` is a plain method that hands back an operand outside its domain
fn tests_operand<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    instance: Type<'db>,
    name: &str,
) -> bool {
    match instance
        .member(db, env, name)
        .place
        .ignore_possibly_undefined()
    {
        Some(Type::BoundMethod(bound)) => bound
            .function(db)
            .is_some_and(|function| *tests_its_operand(db, function.literal(db).last_definition)),
        _ => false,
    }
}

/// where the class body itself defines `name`
fn own_definition(class_node: &ast::StmtClassDef, name: &str) -> Option<TextRange> {
    class_node
        .body
        .iter()
        .find_map(|statement| match statement {
            ast::Stmt::FunctionDef(function) if function.name.as_str() == name => {
                Some(function.name.range())
            }
            _ => None,
        })
}
