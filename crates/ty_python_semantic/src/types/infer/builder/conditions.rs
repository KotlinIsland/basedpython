use std::collections::VecDeque;

use ruff_python_ast as ast;
use ruff_text_size::TextRange;
use ty_python_core::Truthiness;

use crate::place::Place;
use crate::types::ProgramEnvironment;
use crate::types::{
    ClassLiteral, IntersectionBuilder, KnownClass, Type,
    diagnostic::{OVERLAPPING_CONDITION, REDUNDANT_BOOLEAN_COMPARISON},
    infer::TypeInferenceBuilder,
};
use crate::{AnalysisSettings, Db};

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

/// Strip the `not`s off a condition, returning the expression whose truthiness is really tested
/// and the half of it the condition selects.
///
/// A walrus is stripped too: `while (chunk := read()):` tests what `read()` returned.
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
    /// or a comprehension guard — for a branch shared by members the condition cannot tell apart.
    ///
    /// A boolean operator is not one condition but one per operand: each operand's truthiness is
    /// tested on its own at runtime, and the operator's *value* — the union of the operands — is
    /// not a value anything is tested for. `if count > 0 or leftovers:` asks two questions, and
    /// reading it as one would claim it conflates a `bool` with a `dict`.
    pub(super) fn check_condition(&mut self, test: &ast::Expr) {
        if !self.context.is_lint_enabled(&OVERLAPPING_CONDITION) {
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
        // a condition whose outcome is fixed is `redundant-condition`'s to report; one that is not
        // asking anything cannot be conflating anything
        if self
            .expression_type(root)
            .bool(self.db(), env)
            .is_ambiguous()
        {
            self.check_overlapping_condition(test, root, polarity);
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
    /// belongs to the other branch, as `False` meets `""` in `if not name:` over a
    /// `str | Literal[False]`. Two arms that are
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
        // `None` meeting another falsy value is `truthiness-test-of-none-union`'s to report
        if arm.is_none(db) {
            return;
        }
        let Some(class) = arm_class(db, env, arm) else {
            return;
        };
        if is_exempt(db, class, &settings.overlapping_condition_exempt_types) {
            return;
        }
        let truthiness = arm_truthiness(db, env, arm, settings);
        let whole = !truthiness.is_ambiguous();
        let part = match truthiness {
            Truthiness::AlwaysTrue if polarity == ConditionPolarity::Falsy => return,
            Truthiness::AlwaysFalse if polarity == ConditionPolarity::Truthy => return,
            // a member with no outcome reaches neither branch
            Truthiness::Uninhabited => return,
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
fn is_exempt<'db>(db: &'db dyn Db, class: ClassLiteral<'db>, exempt: &[Box<str>]) -> bool {
    if exempt.is_empty() {
        return false;
    }
    let matches = |name: &str| exempt.iter().any(|entry| &**entry == name);
    let qualified = class.qualified_name(db).to_string();
    matches(&qualified) || qualified.strip_prefix("builtins.").is_some_and(matches)
}
