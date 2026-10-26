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
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, TraversalSignal, walk_node};
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, Stmt, UnaryOp};
use ruff_python_trivia::PythonWhitespace;
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_semantic::types::ide_support::{
    BuiltinScalar, builtin_scalar_kinds, folds_to_a_literal,
};

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
    plan_selection(context, &selection, target, Title::Plain)
}

/// What extracting does at a caret rather than a selection: each expression of the statement
/// around `offset`, innermost first, with the range a client selects to extract it and the
/// rewrite — the expressions `by/expressionTypes` answers at the same caret, less a bare name
/// and those that cannot be bound to a name. When none of them can be extracted, the innermost one's refusal is
/// the answer, so an editor asking about a caret is told why rather than nothing.
pub(super) fn around_caret(
    context: &RefactorContext<'_>,
    offset: TextSize,
    target: Target,
) -> Vec<(TextRange, Result<Plan, Refusal>)> {
    let covering = covering_node(context.parsed.syntax().into(), TextRange::empty(offset));
    let mut ranges: Vec<TextRange> = Vec::new();
    for node in covering.ancestors() {
        if node.is_statement() {
            break;
        }
        if node.as_expr_ref().is_some() && ranges.last() != Some(&node.range()) {
            ranges.push(node.range());
        }
    }

    let mut available = Vec::new();
    let mut first_refusal = None;
    for range in ranges {
        let Some(selection) = selected_expression(context, range) else {
            continue;
        };
        // a name is already a name
        if matches!(selection.extracted, Extracted::Expr(Expr::Name(_))) {
            continue;
        }
        match plan_selection(context, &selection, target, Title::Naming) {
            Ok(plan) => available.push((range, Ok(plan))),
            Err(Refusal::NotApplicable) => {}
            Err(refusal) => {
                if first_refusal.is_none() {
                    first_refusal = Some((range, Err(refusal)));
                }
            }
        }
    }
    if available.is_empty() {
        available.extend(first_refusal);
    }
    available
}

/// Whether a title names the expression it extracts, which it must when several are offered at
/// once.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Title {
    Plain,
    Naming,
}

fn plan_selection(
    context: &RefactorContext<'_>,
    selection: &Selection<'_>,
    target: Target,
    title_kind: Title,
) -> Result<Plan, Refusal> {
    if target == Target::Constant
        && !selection
            .operands()
            .iter()
            .all(|operand| is_constant(context, operand))
    {
        return Err(Refusal::NotApplicable);
    }
    let module = context.parsed.suite();
    let ancestors = statement_ancestors(module, selection.replaced);
    let Some(&statement) = ancestors.last() else {
        return Err(Refusal::NotApplicable);
    };
    // the statement already is the expression on its own
    if let Extracted::Expr(expr) = selection.extracted
        && matches!(statement, Stmt::Expr(stmt) if stmt.value.range() == expr.range())
    {
        return Err(Refusal::NotApplicable);
    }

    let names = names_in_file(context);
    let version = context.file.python_version(context.db);
    let base = selection.suggested_name(version.minor);
    let name = match target {
        Target::Variable => fresh_name(&base, &names),
        Target::Constant => fresh_name(&base.to_uppercase(), &names),
    };
    let mut title = match target {
        Target::Variable => format!("Extract variable `{name}`"),
        Target::Constant => format!("Introduce constant `{name}`"),
    };
    if title_kind == Title::Naming {
        title = format!(
            "{title} from `{}`",
            abbreviated(&context.source()[selection.replaced])
        );
    }
    let refuse = |reason: String| Refusal::refused(title.clone(), reason);

    for operand in selection.operands() {
        if let Some(reason) = context_dependent_construct(context, AnyNodeRef::from(operand)) {
            return Err(refuse(format!("the expression can't be moved: {reason}")));
        }
        if contains_named_expression(operand) {
            return Err(refuse("the expression binds a name with `:=`".to_string()));
        }
    }

    // a run of operands is evaluated where its first operand was, so it is that operand's place
    // in the statement that decides whether the run can be evaluated before the statement
    let placement = placement_in_statement(
        &context.model,
        statement,
        AnyNodeRef::from(selection.first()),
    );
    let expression_text = expression_source(context, selection);
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

/// `text` on one line and at most a few dozen characters, for a title to name it by.
fn abbreviated(text: &str) -> String {
    const MOST: usize = 40;
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MOST {
        return one_line;
    }
    let mut shortened: String = one_line.chars().take(MOST - 1).collect();
    shortened.push('…');
    shortened
}

/// What a selection extracts.
enum Extracted<'a> {
    /// An expression of the tree.
    Expr(&'a Expr),
    /// Consecutive operands of a chain of one associative operator, `b + c` in `a + b + c`,
    /// which python groups as `(a + b) + c` and so has no node of its own.
    Run { operands: Vec<&'a Expr> },
}

struct Selection<'a> {
    extracted: Extracted<'a>,
    /// The range the name replaces: the expression, with the parentheses the
    /// selection included.
    replaced: TextRange,
}

impl<'a> Selection<'a> {
    /// The expressions the selection is made of, evaluated in this order.
    fn operands(&self) -> Vec<&'a Expr> {
        match &self.extracted {
            Extracted::Expr(expr) => vec![*expr],
            Extracted::Run { operands, .. } => operands.clone(),
        }
    }

    /// The first expression of the selection to be evaluated.
    fn first(&self) -> &'a Expr {
        match &self.extracted {
            Extracted::Expr(expr) => expr,
            Extracted::Run { operands, .. } => operands[0],
        }
    }

    /// A name that says what the selection evaluates to.
    fn suggested_name(&self, python_version_minor: u8) -> String {
        match &self.extracted {
            Extracted::Expr(expr) => suggest_for_expression(expr, python_version_minor),
            Extracted::Run { .. } => "value".to_string(),
        }
    }
}

/// The expression the selection covers exactly, ignoring surrounding
/// whitespace and allowing parentheses around it — or, when the selection
/// covers consecutive operands of an associative chain, those operands.
fn selected_expression<'a>(
    context: &'a RefactorContext<'_>,
    range: TextRange,
) -> Option<Selection<'a>> {
    struct Finder<'a, 'c, 'db> {
        context: &'c RefactorContext<'db>,
        range: TextRange,
        parents: Vec<AnyNodeRef<'a>>,
        found: Option<&'a Expr>,
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
                self.found = Some(expr);
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
    if let Some(expr) = finder.found {
        return is_extractable(expr).then_some(Selection {
            extracted: Extracted::Expr(expr),
            replaced: trimmed,
        });
    }
    selected_run(context, trimmed)
}

/// Consecutive operands of an associative chain that `range` covers exactly, from the first
/// one's opening parenthesis, if it has one, to the last one's closing parenthesis.
///
/// `a + b + c` is `(a + b) + c`, so `b + c` is no expression of it. Binding `b + c` to a name
/// and reading `a + value` instead evaluates `a + (b + c)`, which is the same value exactly when
/// the operator is associative over the operands' types — which the checker is asked, not the
/// spelling: `+` over integers or over strings is, `+` over floats rounds differently grouped
/// the other way, and an operator a class defines is anything it likes.
fn selected_run<'a>(context: &'a RefactorContext<'_>, range: TextRange) -> Option<Selection<'a>> {
    let covering = covering_node(context.parsed.syntax().into(), range);
    let chain = covering.ancestors().find_map(|node| match node {
        AnyNodeRef::ExprBinOp(bin_op) => Some(bin_op),
        _ => None,
    })?;
    // the operands of the chain, each with the range it has in it, parentheses included
    let mut operands: Vec<(&Expr, TextRange)> = Vec::new();
    let mut node = chain;
    loop {
        let parent = AnyNodeRef::from(node);
        operands.push((
            &node.right,
            context.parenthesized_range(&node.right, parent),
        ));
        let left_range = context.parenthesized_range(&node.left, parent);
        match &*node.left {
            Expr::BinOp(left) if left.op == chain.op && left_range == left.range() => node = left,
            left => {
                operands.push((left, left_range));
                break;
            }
        }
    }
    operands.reverse();

    let first = operands
        .iter()
        .position(|(_, operand_range)| operand_range.start() == range.start())?;
    let last = operands
        .iter()
        .position(|(_, operand_range)| operand_range.end() == range.end())?;
    // a run starting at the first operand is an expression of the tree, found already, and a
    // single operand is not a run
    if first == 0 || last <= first {
        return None;
    }
    let all: Vec<&Expr> = operands.iter().map(|(operand, _)| *operand).collect();
    if !is_associative(context, chain.op, &all[..=last]) {
        return None;
    }
    Some(Selection {
        extracted: Extracted::Run {
            operands: all[first..=last].to_vec(),
        },
        replaced: range,
    })
}

/// Whether `op` is associative over the values `operands` evaluate to, as the checker reads
/// them: integers under `+`, `*` and the bitwise operators, and strings or byte strings — all of
/// one kind — under `+`.
fn is_associative(context: &RefactorContext<'_>, op: ast::Operator, operands: &[&Expr]) -> bool {
    let Some(kinds) = operands
        .iter()
        .map(|operand| builtin_scalar_kinds(&context.model, operand))
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    let integers = || {
        kinds
            .iter()
            .flatten()
            .all(|kind| matches!(kind, BuiltinScalar::Int | BuiltinScalar::Bool))
    };
    let all_one = |wanted: BuiltinScalar| kinds.iter().all(|kinds| kinds.as_slice() == [wanted]);
    match op {
        ast::Operator::Add => {
            integers() || all_one(BuiltinScalar::Str) || all_one(BuiltinScalar::Bytes)
        }
        ast::Operator::Mult
        | ast::Operator::BitAnd
        | ast::Operator::BitOr
        | ast::Operator::BitXor => integers(),
        _ => false,
    }
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

/// The source of the selection as the value of an assignment, parenthesized where its text
/// would not parse there on its own.
fn expression_source(context: &RefactorContext<'_>, selection: &Selection<'_>) -> String {
    let Extracted::Expr(expr) = selection.extracted else {
        let text = &context.source()[selection.replaced];
        return if context.is_multiline(selection.replaced) {
            format!("({text})")
        } else {
            text.to_string()
        };
    };
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

    /// `a + b + c` is `(a + b) + c`; extracting `b + c` evaluates `a + (b + c)`, which is
    /// the same value because adding integers is associative.
    #[test]
    fn operands_of_an_associative_chain() {
        assert_snapshot!(extract(
            "
            def f(a: int, b: int, c: int, d: int):
                return a + <START>b + c<END> + d
            ",
        ), @"
        Extract variable `value`
        ---

        def f(a: int, b: int, c: int, d: int):
            value = b + c
            return a + value + d
        ");
    }

    #[test]
    fn operands_of_a_string_concatenation() {
        assert_snapshot!(extract(
            "
            def f(name: str):
                return 'hello ' + <START>name + '!'<END>
            ",
        ), @"
        Extract variable `value`
        ---

        def f(name: str):
            value = name + '!'
            return 'hello ' + value
        ");
    }

    /// floats round, so `(a + b) + c` and `a + (b + c)` can differ
    #[test]
    fn operands_of_a_float_chain_are_not_offered() {
        assert_snapshot!(extract(
            "
            def f(a: float, b: float, c: float):
                return a + <START>b + c<END>
            ",
        ), @"not offered");
    }

    #[test]
    fn operands_under_another_operator_are_not_offered() {
        assert_snapshot!(extract(
            "
            def f(a: int, b: int, c: int):
                return a * <START>b + c<END>
            ",
        ), @"not offered");
    }

    #[test]
    fn operands_after_a_call_are_refused() {
        assert_snapshot!(extract(
            "
            def f(b: int, c: int):
                return g() + <START>b + c<END>

            def g() -> int: ...
            ",
        ), @"refused: Extract variable `value` (something with effects is evaluated before the expression, which would now run after it)");
    }

    /// reading a module's attribute and multiplying two numbers run nothing of the program's,
    /// so evaluating the power first changes nothing
    #[test]
    fn after_a_module_attribute_and_arithmetic() {
        assert_snapshot!(extract(
            "
            import math

            def f(radius: float, scale: int):
                return scale * math.pi * <START>radius ** 2<END>
            ",
        ), @"
        Extract variable `value`
        ---

        import math

        def f(radius: float, scale: int):
            value = radius ** 2
            return scale * math.pi * value
        ");
    }

    /// `self.price` may be a property, which could see the program differently once the
    /// expression after it is evaluated first
    #[test]
    fn after_an_instance_attribute_is_refused() {
        assert_snapshot!(extract(
            "
            class Item:
                price: float
                quantity: int

                def total(self):
                    return self.price * <START>self.quantity<END>
            ",
        ), @"refused: Extract variable `quantity_1` (something with effects is evaluated before the expression, which would now run after it)");
    }

    #[test]
    fn around_a_caret_every_enclosing_expression() {
        assert_snapshot!(
            RefactorTest::python(
                "
                def f(items: list[int], tax: int):
                    print(len(it<CURSOR>ems) + tax)
                ",
            )
            .around_caret(RefactorKind::ExtractVariable),
            @"
        len(items): Extract variable `value` from `len(items)`
        len(items) + tax: Extract variable `value` from `len(items) + tax`
        "
        );
    }

    #[test]
    fn around_a_caret_the_innermost_refusal_when_nothing_applies() {
        assert_snapshot!(
            RefactorTest::python(
                "
                def f(queue):
                    while queue.pen<CURSOR>ding():
                        queue.pop()
                ",
            )
            .around_caret(RefactorKind::ExtractVariable),
            @"queue.pending: refused: Extract variable `pending` from `queue.pending` (the expression can't be evaluated before its statement: it is evaluated again on every iteration of the loop)"
        );
    }

    #[test]
    fn around_a_caret_constants_are_the_literal_expressions() {
        assert_snapshot!(
            RefactorTest::python(
                "
                def f(seconds: int):
                    return seconds / (60 * 6<CURSOR>0 * 24)
                ",
            )
            .around_caret(RefactorKind::IntroduceConstant),
            @"
        60: Introduce constant `NUMBER` from `60`
        60 * 60: Introduce constant `VALUE` from `60 * 60`
        60 * 60 * 24: Introduce constant `VALUE` from `60 * 60 * 24`
        "
        );
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
