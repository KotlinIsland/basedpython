//! Extract Variable and Introduce Constant: bind a selected expression to a
//! new name, and read the name where the expression was.
//!
//! A variable is bound directly above the statement the expression is part of,
//! so the expression is now evaluated before the rest of that statement. That is
//! only the same program when the expression was evaluated exactly once whenever
//! the statement ran, and nothing with an effect was evaluated before it — an
//! expression in a loop's condition, a conditional branch, a lambda, or after a
//! call is refused.
//!
//! A constant is bound at the top level of the module, before the definition
//! that uses it, so it is evaluated once at import — whether or not the
//! expression it was made from would have been evaluated at all. That is only
//! the same program for an expression that is made of literals, does not build
//! anything mutable, and cannot raise; see [`is_constant`].

use ruff_diagnostics::Edit;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, TraversalSignal, walk_node};
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, Stmt, UnaryOp};
use ruff_python_trivia::PythonWhitespace;
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_semantic::types::ide_support::folds_to_a_literal;

use super::evaluation::{NotOnce, Placement, placement_in_statement};
use super::hazards::{contains_named_expression, context_dependent_construct};
use super::names::{fresh_name, names_in_file, suggest_for_expression};
use super::text::{leading_comments_start, statement_ancestors, suite_of};
use super::{Plan, RefactorContext, Refusal};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Target {
    Variable,
    Constant,
}

pub(super) fn plan(
    context: &RefactorContext<'_>,
    range: TextRange,
    target: Target,
) -> Result<Plan, Refusal> {
    let Some(selection) = selected_expression(context, range) else {
        return Err(Refusal::NotApplicable);
    };
    let expr = selection.expr;
    if target == Target::Constant && !is_constant(context, expr) {
        return Err(Refusal::NotApplicable);
    }
    let module = context.parsed.suite();
    let ancestors = statement_ancestors(module, expr.range());
    let Some(&statement) = ancestors.last() else {
        return Err(Refusal::NotApplicable);
    };
    // the statement already is the expression on its own
    if matches!(statement, Stmt::Expr(stmt) if stmt.value.range() == expr.range()) {
        return Err(Refusal::NotApplicable);
    }

    let names = names_in_file(context);
    let version = context.file.python_version(context.db);
    let base = suggest_for_expression(expr, version.minor);
    let (name, title) = match target {
        Target::Variable => {
            let name = fresh_name(&base, &names);
            let title = format!("Extract variable `{name}`");
            (name, title)
        }
        Target::Constant => {
            let name = fresh_name(&base.to_uppercase(), &names);
            let title = format!("Introduce constant `{name}`");
            (name, title)
        }
    };
    let refuse = |reason: String| Refusal::refused(title.clone(), reason);

    if let Some(reason) = context_dependent_construct(context, AnyNodeRef::from(expr)) {
        return Err(refuse(format!("the expression can't be moved: {reason}")));
    }
    if contains_named_expression(expr) {
        return Err(refuse("the expression binds a name with `:=`".to_string()));
    }

    let placement = placement_in_statement(statement, AnyNodeRef::from(expr));
    let expression_text = expression_source(context, expr);
    let line_ending = context.line_ending();

    let insertion = match target {
        Target::Variable => {
            match placement {
                Placement::Once {
                    effects_before: false,
                } => {}
                Placement::Once {
                    effects_before: true,
                } => {
                    return Err(refuse(
                        "something with effects is evaluated before the expression, which would now run after it"
                            .to_string(),
                    ));
                }
                Placement::NotOnce(reason) => {
                    return Err(refuse(format!(
                        "the expression can't be evaluated before its statement: {}",
                        reason.describe()
                    )));
                }
            }
            if ancestors
                .iter()
                .rev()
                .skip(1)
                .find(|stmt| matches!(stmt, Stmt::FunctionDef(_) | Stmt::ClassDef(_)))
                .is_some_and(|stmt| stmt.is_class_def_stmt())
            {
                return Err(refuse(
                    "a variable bound in a class body would become an attribute of the class"
                        .to_string(),
                ));
            }
            if suite_of(module, statement).is_none()
                || ruff_python_trivia::indentation_at_offset(statement.start(), context.source())
                    .is_none()
            {
                return Err(refuse(
                    "the statement shares its line with the one it belongs to".to_string(),
                ));
            }
            let indentation = context.indentation(statement);
            Edit::insertion(
                format!("{indentation}{name} = {expression_text}{line_ending}"),
                context.source().line_start(statement.start()),
            )
        }
        Target::Constant => {
            if let Placement::NotOnce(reason @ (NotOnce::Annotation | NotOnce::Opaque)) = placement
            {
                return Err(refuse(format!(
                    "the expression can't be moved to the module: {}",
                    reason.describe()
                )));
            }
            let top_level = ancestors[0];
            let at = leading_comments_start(context.source(), top_level.start());
            let separation = if matches!(top_level, Stmt::FunctionDef(_) | Stmt::ClassDef(_)) {
                format!("{line_ending}{line_ending}")
            } else {
                String::new()
            };
            Edit::insertion(
                format!("{name} = {expression_text}{line_ending}{separation}"),
                at,
            )
        }
    };

    Ok(Plan {
        title,
        edits: vec![insertion, Edit::range_replacement(name, selection.replaced)],
    })
}

struct Selection<'a> {
    expr: &'a Expr,
    /// The range the name replaces: the expression, with the parentheses the
    /// selection included.
    replaced: TextRange,
}

/// The expression the selection covers exactly, ignoring surrounding
/// whitespace and allowing parentheses around it.
fn selected_expression<'a>(
    context: &'a RefactorContext<'_>,
    range: TextRange,
) -> Option<Selection<'a>> {
    struct Finder<'a, 'c, 'db> {
        context: &'c RefactorContext<'db>,
        range: TextRange,
        parents: Vec<AnyNodeRef<'a>>,
        found: Option<Selection<'a>>,
    }
    impl<'a> SourceOrderVisitor<'a> for Finder<'a, '_, '_> {
        fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
            // `leave_node` runs for skipped nodes too, so every node is pushed
            self.parents.push(node);
            // a node that is not around the selection can still be the expression
            // it selects, when the selection includes the expression's parentheses
            if self.found.is_some()
                || !(node.range().contains_range(self.range)
                    || self.range.contains_range(node.range()))
            {
                return TraversalSignal::Skip;
            }
            TraversalSignal::Traverse
        }

        fn leave_node(&mut self, _node: AnyNodeRef<'a>) {
            self.parents.pop();
        }

        fn visit_expr(&mut self, expr: &'a Expr) {
            if self.found.is_some() {
                return;
            }
            if let Some(parent) = self.parents.last().copied()
                && (expr.range() == self.range
                    || self.context.parenthesized_range(expr, parent) == self.range)
            {
                self.found = Some(Selection {
                    expr,
                    replaced: self.range,
                });
                return;
            }
            ruff_python_ast::visitor::source_order::walk_expr(self, expr);
        }
    }

    let source = context.source();
    let selected = source.get(range.to_std_range())?;
    let leading = selected.len() - selected.trim_whitespace_start().len();
    let trimmed = selected.trim_whitespace();
    if trimmed.is_empty() {
        return None;
    }
    let start = range.start() + TextSize::try_from(leading).ok()?;
    let trimmed = TextRange::at(start, TextSize::try_from(trimmed.len()).ok()?);

    let mut finder = Finder {
        context,
        range: trimmed,
        parents: Vec::new(),
        found: None,
    };
    walk_node(&mut finder, context.parsed.syntax().into());
    let selection = finder.found?;
    is_extractable(selection.expr).then_some(selection)
}

/// Whether `expr` is a value that can be bound to a name: it is read rather
/// than assigned, and it means something outside its position.
fn is_extractable(expr: &Expr) -> bool {
    match expr {
        Expr::Name(ast::ExprName { ctx, .. })
        | Expr::Attribute(ast::ExprAttribute { ctx, .. })
        | Expr::Subscript(ast::ExprSubscript { ctx, .. })
        | Expr::List(ast::ExprList { ctx, .. })
        | Expr::Tuple(ast::ExprTuple { ctx, .. }) => ctx.is_load(),
        Expr::Starred(_)
        | Expr::Slice(_)
        | Expr::IpyEscapeCommand(_)
        | Expr::CallableType(_)
        | Expr::ProtocolType(_)
        | Expr::ProtocolMethod(_)
        | Expr::Statement(_) => false,
        _ => true,
    }
}

/// The source of `expr` as the value of an assignment, parenthesized where its
/// text would not parse there on its own.
fn expression_source(context: &RefactorContext<'_>, expr: &Expr) -> String {
    let text = context.expression_text(expr);
    let bracketed = matches!(
        expr,
        Expr::List(_)
            | Expr::ListComp(_)
            | Expr::Dict(_)
            | Expr::DictComp(_)
            | Expr::Set(_)
            | Expr::SetComp(_)
    ) || matches!(expr, Expr::Tuple(tuple) if tuple.parenthesized);
    let unparenthesized_generator =
        matches!(expr, Expr::Generator(generator) if !generator.parenthesized);
    if unparenthesized_generator || (!bracketed && context.is_multiline(expr.range())) {
        format!("({text})")
    } else {
        text.to_string()
    }
}

/// Whether `expr` is made only of literals and evaluates to an immutable value without raising.
///
/// Made only of literals so that the constant, read at the top of the module, reads nothing that
/// might not be bound there yet.
///
/// Without raising because a constant is evaluated as the module loads, and the expression it is
/// made from need not have been evaluated at all where it was written — `1 / 0` in a branch that
/// never runs, or an expression under an `assert` that python skips with `-O`. Hoisting one of
/// those would turn a program that runs into one that fails to import.
///
/// Whether an operation over literals raises is not something to read off its spelling, so each
/// one is asked of the checker instead: ty folds it to the literal it produces exactly when it
/// has carried it out, which makes a folded type the proof that evaluating it yields a value.
/// `60 * 60 * 24` folds and is hoisted; `1 / 0`, `1 % 0`, `-"a"` and `1 + "a"` do not and are not.
fn is_constant(context: &RefactorContext<'_>, expr: &Expr) -> bool {
    match expr {
        Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::NumberLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::EllipsisLiteral(_) => true,
        Expr::UnaryOp(unary) => {
            matches!(
                unary.op,
                UnaryOp::USub | UnaryOp::UAdd | UnaryOp::Invert | UnaryOp::Not
            ) && is_constant(context, &unary.operand)
                && folds_to_a_literal(&context.model, expr)
        }
        Expr::BinOp(bin_op) => {
            !matches!(bin_op.op, ast::Operator::Coalesce | ast::Operator::Result)
                && is_constant(context, &bin_op.left)
                && is_constant(context, &bin_op.right)
                && folds_to_a_literal(&context.model, expr)
        }
        // a tuple display of constants builds a tuple, which cannot fail
        Expr::Tuple(tuple) => {
            tuple.ctx.is_load()
                && !tuple.is_anon_named_tuple
                && !tuple.is_anon_named_tuple_value
                && tuple.callable_shape.is_none()
                && !tuple.is_parameter_shape
                && tuple
                    .elts
                    .iter()
                    .all(|element| is_constant(context, element))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use crate::refactor::RefactorKind;
    use crate::refactor::test_support::RefactorTest;

    fn extract(source: &str) -> String {
        RefactorTest::python(source).apply(RefactorKind::ExtractVariable)
    }

    fn constant(source: &str) -> String {
        RefactorTest::python(source).apply(RefactorKind::IntroduceConstant)
    }

    #[test]
    fn extracts_above_the_statement() {
        assert_snapshot!(extract(
            "
            def f(items):
                print(<START>len(items) + 1<END>)
            ",
        ), @"
        Extract variable `value`
        ---

        def f(items):
            value = len(items) + 1
            print(value)
        ");
    }

    #[test]
    fn name_comes_from_the_call() {
        assert_snapshot!(extract(
            "
            def f(user):
                return <START>user.get_name()<END>.upper()
            ",
        ), @"
        Extract variable `name`
        ---

        def f(user):
            name = user.get_name()
            return name.upper()
        ");
    }

    #[test]
    fn name_avoids_every_name_in_the_file() {
        assert_snapshot!(extract(
            "
            value = 1

            def f(a, b):
                return <START>a * b<END> + value
            ",
        ), @"
        Extract variable `value_1`
        ---

        value = 1

        def f(a, b):
            value_1 = a * b
            return value_1 + value
        ");
    }

    #[test]
    fn selection_may_include_parentheses() {
        assert_snapshot!(extract(
            "
            def f(a, b):
                return <START>(a + b)<END> * 2
            ",
        ), @"
        Extract variable `value`
        ---

        def f(a, b):
            value = a + b
            return value * 2
        ");
    }

    #[test]
    fn loop_condition_is_refused() {
        assert_snapshot!(extract(
            "
            def f(queue):
                while <START>queue.pending()<END>:
                    queue.pop()
            ",
        ), @"refused: Extract variable `pending` (the expression can't be evaluated before its statement: it is evaluated again on every iteration of the loop)");
    }

    #[test]
    fn conditional_branch_is_refused() {
        assert_snapshot!(extract(
            "
            def f(a, b):
                return <START>b.value<END> if a else None
            ",
        ), @"refused: Extract variable `value` (the expression can't be evaluated before its statement: it is only evaluated when a condition holds)");
    }

    #[test]
    fn lambda_body_is_refused() {
        assert_snapshot!(extract(
            "
            def f(items):
                return sorted(items, key=lambda item: <START>item.name<END>)
            ",
        ), @"refused: Extract variable `name` (the expression can't be evaluated before its statement: it is evaluated later, when the enclosing function is called)");
    }

    #[test]
    fn after_an_effect_is_refused() {
        assert_snapshot!(extract(
            "
            def f(a):
                print(log(), <START>a.value<END>)

            def log(): ...
            ",
        ), @"refused: Extract variable `value` (something with effects is evaluated before the expression, which would now run after it)");
    }

    #[test]
    fn after_a_name_is_allowed() {
        assert_snapshot!(extract(
            "
            def f(a, b):
                print(a, <START>b.value<END>)
            ",
        ), @"
        Extract variable `value`
        ---

        def f(a, b):
            value = b.value
            print(a, value)
        ");
    }

    #[test]
    fn elif_condition_is_refused() {
        assert_snapshot!(extract(
            "
            def f(a):
                if a.x:
                    pass
                elif <START>a.y<END>:
                    pass
            ",
        ), @"refused: Extract variable `y` (the expression can't be evaluated before its statement: it is only evaluated when a condition holds)");
    }

    #[test]
    fn class_body_is_refused() {
        assert_snapshot!(extract(
            "
            class A:
                x = <START>compute()<END>

            def compute(): ...
            ",
        ), @"refused: Extract variable `compute_1` (a variable bound in a class body would become an attribute of the class)");
    }

    #[test]
    fn partial_expression_is_not_offered() {
        assert_snapshot!(extract(
            "
            def f(a, b, c):
                return a + <START>b + c<END>
            ",
        ), @"not offered");
    }

    #[test]
    fn assignment_target_is_not_offered() {
        assert_snapshot!(extract(
            "
            def f(a):
                <START>a.x<END> = 1
            ",
        ), @"not offered");
    }

    #[test]
    fn multiline_expression_is_parenthesized() {
        assert_snapshot!(extract(
            "
            def f(a, b):
                return g(<START>a +
                         b<END>)

            def g(x): ...
            ",
        ), @"
        Extract variable `value`
        ---

        def f(a, b):
            value = (a +
                     b)
            return g(value)

        def g(x): ...
        ");
    }

    #[test]
    fn constant_goes_above_the_definition() {
        assert_snapshot!(constant(
            "
            import os

            # waits a while
            def wait():
                sleep(<START>60 * 60<END>)

            def sleep(seconds): ...
            ",
        ), @"
        Introduce constant `VALUE`
        ---

        import os

        VALUE = 60 * 60


        # waits a while
        def wait():
            sleep(VALUE)

        def sleep(seconds): ...
        ");
    }

    #[test]
    fn constant_is_not_offered_for_a_name() {
        assert_snapshot!(constant(
            "
            def f(a):
                return <START>a + 1<END>
            ",
        ), @"not offered");
    }

    #[test]
    fn constant_is_not_offered_for_a_mutable_display() {
        assert_snapshot!(constant(
            "
            def f():
                return <START>[1, 2]<END>
            ",
        ), @"not offered");
    }

    /// A constant is evaluated as the module loads. This expression is evaluated only when the
    /// branch it is in is taken, so hoisting it would turn a program that runs into one that
    /// raises `ZeroDivisionError` on import.
    #[test]
    fn constant_is_not_offered_for_an_expression_that_raises() {
        assert_snapshot!(constant(
            "
            def f(c):
                return c if c else <START>1 / 0<END>
            ",
        ), @"not offered");
    }

    /// The same, for an expression python does not evaluate at all under `-O`.
    #[test]
    fn constant_is_not_offered_from_an_assert_when_it_raises() {
        assert_snapshot!(constant(
            "
            def f(c):
                assert c == <START>1 % 0<END>
            ",
        ), @"not offered");
    }

    /// An operation over literals that does not type-check raises `TypeError` where it is
    /// written, so it is no more hoistable than one that divides by zero.
    #[test]
    fn constant_is_not_offered_for_an_operation_between_literals_of_different_types() {
        assert_snapshot!(constant(
            "
            def f(c):
                return c if c else <START>1 + \"a\"<END>
            ",
        ), @"not offered");
    }

    /// Nor is a unary operator the operand does not support.
    #[test]
    fn constant_is_not_offered_for_an_unsupported_unary_operator() {
        assert_snapshot!(constant(
            "
            def f(c):
                return c if c else <START>-\"a\"<END>
            ",
        ), @"not offered");
    }

    /// ty folds through a `Final` name — `K + 1` is `Literal[4]` here — so the folded type alone
    /// would say this is hoistable. It is not: the constant would be written above `K`, reading a
    /// name that is not bound yet. The structural half of the rule is what refuses it.
    #[test]
    fn constant_is_not_offered_for_arithmetic_over_a_final_name() {
        assert_snapshot!(constant(
            "
            from typing import Final

            K: Final = 3

            def f(c):
                return c if c else <START>K + 1<END>
            ",
        ), @"not offered");
    }

    /// An arithmetic expression the checker has carried out is still hoisted: having folded it
    /// to the value it produces, ty has shown that evaluating it does not raise.
    #[test]
    fn constant_is_offered_for_arithmetic_the_checker_folds() {
        assert_snapshot!(constant(
            "
            def f(c):
                return c if c else <START>60 * 60 * 24<END>
            ",
        ), @"
        Introduce constant `VALUE`
        ---

        VALUE = 60 * 60 * 24


        def f(c):
            return c if c else VALUE
        ");
    }

    #[test]
    fn augmented_assignment_value() {
        assert_snapshot!(extract(
            "
            def f(items, tax):
                total = 0
                for item in items:
                    total += <START>item * tax<END>
                return total
            ",
        ), @"
        Extract variable `value`
        ---

        def f(items, tax):
            total = 0
            for item in items:
                value = item * tax
                total += value
            return total
        ");
    }

    #[test]
    fn optional_chain_link_is_refused() {
        assert_snapshot!(
            RefactorTest::basedpython(
                "
                class User:
                    name: str

                def f(user: User | None):
                    return <START>user?.name<END>.upper()
                ",
            )
            .apply(RefactorKind::ExtractVariable),
            @"refused: Extract variable `name_1` (the expression can't be evaluated before its statement: it is a link of an optional chain, which is evaluated as a whole)"
        );
    }
}
