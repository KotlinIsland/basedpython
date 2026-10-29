//! Which parts of a statement Python evaluates whenever the statement runs, and in what order.
//!
//! A call that cannot return ends the flow at the point where it is made, not only at the end of
//! its statement:
//!
//! ```python
//! def f():
//!     print(exit())  # `print` is loaded, `exit()` is made, and the call to `print` never is
//!     return         # unreachable
//! ```
//!
//! The semantic index records a reachability constraint for every call that is evaluated whenever
//! its statement is ([`terminal_call_candidates`]), and the editor greys out what of the statement
//! is evaluated after a call that never returns ([`ranges_evaluated_after`]). Both read the same
//! description of the evaluation order, [`for_each_certain_child`], so that what is greyed out is
//! exactly what the constraint describes.
//!
//! Only what is *certainly* evaluated is described. A part that runs on one path only — the right
//! operand of `a or b`, a branch of `x if c else y`, a lambda's body, every part of a
//! comprehension but its first iterable, the target of a basedpython `a?.b` once `a` is `None` —
//! is left out, along with everything below it.

use ruff_python_ast::{
    self as ast, Expr, InterpolatedStringElement, InterpolatedStringElements, Stmt, StmtRef,
};
use ruff_text_size::{Ranged, TextRange, TextSize};

/// A run of a statement that Python evaluates in one go: the expressions it evaluates, in order,
/// and the text that stands for what the statement then does with them.
///
/// A simple statement is one unit, its whole text. A compound statement's unit is its header,
/// `for x in items:` up to its body.
#[derive(Debug)]
pub struct EvaluationUnit<'a> {
    pub region: TextRange,
    pub expressions: Vec<&'a Expr>,
}

/// The units of `statement`, each with the expressions it certainly evaluates, in order.
///
/// A statement that evaluates nothing, or whose evaluation order is not certain, has none: an
/// `assert`, which `python -O` leaves out, a `def` or `class`, whose decorators, defaults and
/// annotations are evaluated in an order that depends on the Python version, and a basedpython
/// assignment written under a decorator.
pub fn statement_evaluation_units<'a>(
    statement: impl Into<StmtRef<'a>>,
) -> Vec<EvaluationUnit<'a>> {
    fn whole<'a>(statement: StmtRef<'_>, expressions: Vec<&'a Expr>) -> EvaluationUnit<'a> {
        EvaluationUnit {
            region: statement.range(),
            expressions,
        }
    }
    // A compound statement's header runs from its keyword to its body, whose statements have
    // reachability of their own.
    let header = |start: TextSize, body: Option<TextSize>, expression: &Expr| {
        TextRange::new(start, body.unwrap_or_else(|| expression.end()))
    };
    let body_start = |body: &[Stmt]| body.first().map(Ranged::start);

    let statement = statement.into();
    match statement {
        StmtRef::Expr(node) => vec![whole(statement, vec![&node.value])],
        StmtRef::Assign(node) if node.decorator_list.is_empty() => {
            // `a[i] = b = value` evaluates the value, then each target from left to right
            let mut expressions = vec![node.value.as_ref()];
            expressions.extend(&node.targets);
            vec![whole(statement, expressions)]
        }
        // `x[i] += value` loads `x[i]` before it evaluates the value
        StmtRef::AugAssign(node) => vec![whole(statement, vec![&node.target, &node.value])],
        // An annotation is evaluated, if at all, after the target is stored.
        StmtRef::AnnAssign(ast::StmtAnnAssign {
            value: Some(value),
            target,
            decorator_list,
            ..
        }) if decorator_list.is_empty() => vec![whole(statement, vec![value, target])],
        StmtRef::Return(ast::StmtReturn {
            value: Some(value), ..
        }) => vec![whole(statement, vec![value])],
        StmtRef::Raise(node) => {
            let expressions: Vec<&Expr> =
                node.exc.iter().chain(&node.cause).map(|e| &**e).collect();
            if expressions.is_empty() {
                vec![]
            } else {
                vec![whole(statement, expressions)]
            }
        }
        StmtRef::Delete(node) => vec![whole(statement, node.targets.iter().collect())],
        // The target is stored once per item, so only the iterable is certain.
        StmtRef::For(node) => vec![EvaluationUnit {
            region: header(node.start(), body_start(&node.body), &node.iter),
            expressions: vec![&node.iter],
        }],
        StmtRef::With(node) => {
            let expressions: Vec<&Expr> = node
                .items
                .iter()
                .flat_map(|item| {
                    std::iter::once(&item.context_expr).chain(item.optional_vars.as_deref())
                })
                .collect();
            match expressions.last() {
                Some(last) => vec![EvaluationUnit {
                    region: header(node.start(), body_start(&node.body), last),
                    expressions,
                }],
                None => vec![],
            }
        }
        StmtRef::Match(node) => vec![EvaluationUnit {
            region: header(
                node.start(),
                node.cases.first().map(Ranged::start),
                &node.subject,
            ),
            expressions: vec![&node.subject],
        }],
        // The test of an `if` or a `while` is evaluated for its truth, and the semantic index
        // narrows on what it finds. A constraint recorded inside a test would sit among the
        // predicates the test itself records, and the narrowing of places the test does not
        // name — a binding whose specialization later uses refine, among them — loses track of
        // them there, so tests are left out.
        StmtRef::If(_)
        | StmtRef::While(_)
        | StmtRef::Assign(_)
        | StmtRef::AnnAssign(_)
        | StmtRef::Return(_)
        | StmtRef::FunctionDef(_)
        | StmtRef::ClassDef(_)
        | StmtRef::TypeAlias(_)
        | StmtRef::Let(_)
        | StmtRef::Try(_)
        | StmtRef::Assert(_)
        | StmtRef::Import(_)
        | StmtRef::ImportFrom(_)
        | StmtRef::Global(_)
        | StmtRef::Nonlocal(_)
        | StmtRef::Pass(_)
        | StmtRef::Break(_)
        | StmtRef::Continue(_)
        | StmtRef::IpyEscapeCommand(_) => vec![],
    }
}

/// Calls `f` with each child of `expression` that Python certainly evaluates when it evaluates
/// `expression`, in the order it evaluates them.
///
/// What `expression` itself then does — the call, the operation, the store — happens after all of
/// them.
pub fn for_each_certain_child<'a>(expression: &'a Expr, f: &mut impl FnMut(&'a Expr)) {
    match expression {
        // only the first operand: the others are evaluated only when it does not short-circuit
        Expr::BoolOp(node) => {
            if let Some(first) = node.values.first() {
                f(first);
            }
        }
        Expr::Named(node) => f(&node.value),
        // basedpython: `a ?? b` evaluates `b` only when `a` is `None`
        Expr::BinOp(node) if node.op == ast::Operator::Coalesce => f(&node.left),
        Expr::BinOp(node) => {
            f(&node.left);
            f(&node.right);
        }
        Expr::UnaryOp(node) => f(&node.operand),
        Expr::If(node) => f(&node.test),
        Expr::Dict(node) => {
            for item in &node.items {
                if let Some(key) = &item.key {
                    f(key);
                }
                f(&item.value);
            }
        }
        Expr::Set(ast::ExprSet { elts, .. }) | Expr::List(ast::ExprList { elts, .. }) => {
            elts.iter().for_each(f);
        }
        Expr::Tuple(node) => {
            // basedpython: a tuple that spells a type rather than a value
            if !node.is_anon_named_tuple
                && !node.is_parameter_shape
                && node.callable_shape.is_none()
            {
                node.elts.iter().for_each(f);
            }
        }
        // The first iterable is evaluated where the comprehension is written; everything else
        // runs once per item, in the comprehension's own scope.
        Expr::ListComp(ast::ExprListComp { generators, .. })
        | Expr::SetComp(ast::ExprSetComp { generators, .. })
        | Expr::Generator(ast::ExprGenerator { generators, .. }) => {
            if let Some(first) = generators.first() {
                f(&first.iter);
            }
        }
        Expr::DictComp(ast::ExprDictComp { generators, .. }) => {
            if let Some(first) = generators.first() {
                f(&first.iter);
            }
        }
        Expr::Await(node) => f(&node.value),
        Expr::Yield(node) => {
            if let Some(value) = &node.value {
                f(value);
            }
        }
        Expr::YieldFrom(node) => f(&node.value),
        // `a < b < c` evaluates `c` only when `a < b`
        Expr::Compare(node) => {
            node.operands.iter().take(2).for_each(f);
        }
        Expr::Call(node) => {
            // basedpython: a cast's callee and parentheses are not in the source, and the type it
            // casts to is not evaluated
            if node.cast_kind.is_some() {
                return;
            }
            f(&node.func);
            // basedpython: in `a?.b(x)` the call and its arguments are skipped when `a` is `None`
            if is_optional_chain(&node.func) {
                return;
            }
            // Positional arguments, starred ones included, are evaluated before keyword
            // arguments, wherever they are written: `f(k=g(), *h())` calls `h` first.
            node.arguments.args.iter().for_each(&mut *f);
            for keyword in &node.arguments.keywords {
                f(&keyword.value);
            }
        }
        Expr::FString(node) => {
            for element in node.value.elements() {
                for_each_interpolation(element, f);
            }
        }
        Expr::TString(node) => {
            for element in node.value.elements() {
                for_each_interpolation(element, f);
            }
        }
        Expr::Attribute(node) => f(&node.value),
        Expr::Subscript(node) => {
            // basedpython: `typeof X` and `@meta T` spell types
            if node.is_typeof || node.is_type_decoration {
                return;
            }
            f(&node.value);
            if !is_optional_chain(&node.value) {
                f(&node.slice);
            }
        }
        Expr::Starred(node) => f(&node.value),
        Expr::Slice(node) => {
            for bound in [&node.lower, &node.upper, &node.step].into_iter().flatten() {
                f(bound);
            }
        }
        // A lambda's body runs when it is called. A basedpython statement expression's own
        // statements are statements of their own. The rest evaluate nothing, or spell types.
        Expr::Lambda(_)
        | Expr::Statement(_)
        | Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::NumberLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::EllipsisLiteral(_)
        | Expr::Name(_)
        | Expr::IpyEscapeCommand(_)
        | Expr::CallableType(_)
        | Expr::ProtocolType(_)
        | Expr::ProtocolMethod(_) => {}
    }
}

/// An interpolation's value, then the interpolations of its format spec.
fn for_each_interpolation<'a>(
    element: &'a InterpolatedStringElement,
    f: &mut impl FnMut(&'a Expr),
) {
    if let InterpolatedStringElement::Interpolation(interpolation) = element {
        f(&interpolation.expression);
        if let Some(spec) = &interpolation.format_spec {
            for_each_interpolation_in(&spec.elements, f);
        }
    }
}

fn for_each_interpolation_in<'a>(
    elements: &'a InterpolatedStringElements,
    f: &mut impl FnMut(&'a Expr),
) {
    for element in elements {
        for_each_interpolation(element, f);
    }
}

/// Whether `expression` is a link of a basedpython optional chain, `a?.b`, which is skipped along
/// with the rest of the chain when the receiver is `None`.
pub fn is_optional_chain(expression: &Expr) -> bool {
    match expression {
        Expr::Attribute(attribute) => attribute.optional || is_optional_chain(&attribute.value),
        Expr::Call(call) => is_optional_chain(&call.func),
        Expr::Subscript(subscript) => is_optional_chain(&subscript.value),
        _ => false,
    }
}

/// The calls a statement certainly makes, in the order it makes them: each an [`ast::ExprCall`],
/// or the [`ast::ExprAwait`] of one, since awaiting is what an `async` function that never
/// returns does not come back from.
///
/// A cast and a call in an optional chain are not among them; see [`for_each_certain_child`].
pub fn terminal_call_candidates<'a>(statement: impl Into<StmtRef<'a>>) -> Vec<&'a Expr> {
    terminal_call_candidates_by_expression(statement)
        .into_iter()
        .flat_map(|(_, candidates)| candidates)
        .collect()
}

/// [`terminal_call_candidates`], grouped by the expression of the statement each is part of:
/// the value of an assignment, then each of its targets, and so on, in the order the statement
/// evaluates them.
///
/// The semantic index records the constraints of a group once the expression has been
/// evaluated as a whole, as it does for an expression statement's own call. Every call of the
/// group is made whenever the expression is evaluated, so the flow after the expression is the
/// same, and the flow inside the expression — the branches of `a or b`, of `x if c else y` — is
/// left as it is.
pub fn terminal_call_candidates_by_expression<'a>(
    statement: impl Into<StmtRef<'a>>,
) -> Vec<(&'a Expr, Vec<&'a Expr>)> {
    fn collect<'a>(expression: &'a Expr, candidates: &mut Vec<&'a Expr>) {
        match expression {
            // the awaited call is the candidate, not the call that makes the coroutine
            Expr::Await(ast::ExprAwait { value, .. })
                if let Expr::Call(call) = value.as_ref()
                    && is_candidate_call(call) =>
            {
                for_each_certain_child(value, &mut |child| collect(child, candidates));
                candidates.push(expression);
            }
            Expr::Call(call) if is_candidate_call(call) => {
                for_each_certain_child(expression, &mut |child| collect(child, candidates));
                candidates.push(expression);
            }
            _ => for_each_certain_child(expression, &mut |child| collect(child, candidates)),
        }
    }

    statement_evaluation_units(statement)
        .into_iter()
        .flat_map(|unit| unit.expressions)
        .filter_map(|expression| {
            let mut candidates = Vec::new();
            collect(expression, &mut candidates);
            (!candidates.is_empty()).then_some((expression, candidates))
        })
        .collect()
}

fn is_candidate_call(call: &ast::ExprCall) -> bool {
    call.cast_kind.is_none() && !is_optional_chain(&call.func)
}

/// The ranges of `statement` that Python evaluates after the call at `call`, one of its
/// [`terminal_call_candidates`], each trimmed of surrounding whitespace, in source order.
///
/// When the call never returns, none of it runs: the operation each enclosing expression would
/// have performed with the call's value, the operands it would have evaluated next, and what the
/// statement would have done last. What was evaluated before the call — the callee of a call whose
/// argument it is, `a` in `a + exit()` — did run, and is not among the ranges:
///
/// ```python
/// print(exit())       # `(`, `)`
/// x = a + exit() + b  # `x =`, `+`, `+ b`
/// ```
///
/// The body of a compound statement is not among them either: its statements are unreachable in
/// their own right.
pub fn ranges_evaluated_after<'a>(
    statement: impl Into<StmtRef<'a>>,
    call: TextRange,
    source: &str,
) -> Vec<TextRange> {
    let mut ranges = Vec::new();
    for unit in statement_evaluation_units(statement) {
        let Some(index) = unit
            .expressions
            .iter()
            .position(|expression| expression.range().contains_range(call))
        else {
            continue;
        };
        let expression = unit.expressions[index];
        // what the unit evaluated up to and including the call's own expression ran, or is
        // accounted for below
        let mut evaluated = unit.expressions[..=index]
            .iter()
            .map(Ranged::range)
            .collect::<Vec<_>>();
        push_gaps(unit.region, &mut evaluated, source, &mut ranges);
        within(expression, call, source, &mut ranges);
        break;
    }
    ranges.sort_by_key(|range| (range.start(), range.end()));
    ranges
}

/// The ranges of `expression` evaluated after the call at `call`, which it contains.
fn within(expression: &Expr, call: TextRange, source: &str, ranges: &mut Vec<TextRange>) {
    if expression.range() == call {
        // `await f()` is the candidate itself, so the call it awaits is not after it
        return;
    }
    let mut evaluated = Vec::new();
    let mut path = None;
    for_each_certain_child(expression, &mut |child| {
        if path.is_some() {
            return;
        }
        evaluated.push(child.range());
        if child.range().contains_range(call) {
            path = Some(child);
        }
    });
    let Some(child) = path else {
        return;
    };
    push_gaps(expression.range(), &mut evaluated, source, ranges);
    within(child, call, source, ranges);
}

/// Pushes the parts of `region` outside every range of `evaluated`, trimmed of whitespace.
fn push_gaps(
    region: TextRange,
    evaluated: &mut [TextRange],
    source: &str,
    ranges: &mut Vec<TextRange>,
) {
    evaluated.sort_by_key(Ranged::start);
    let mut start = region.start();
    for range in evaluated
        .iter()
        .chain(std::iter::once(&TextRange::empty(region.end())))
    {
        if range.start() > start {
            if let Some(gap) = trim(TextRange::new(start, range.start()), source) {
                ranges.push(gap);
            }
        }
        start = start.max(range.end());
    }
}

fn trim(range: TextRange, source: &str) -> Option<TextRange> {
    let text = source.get(range.start().to_usize()..range.end().to_usize())?;
    let leading = text.len() - text.trim_start().len();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let start = range.start() + TextSize::try_from(leading).ok()?;
    Some(TextRange::at(
        start,
        TextSize::try_from(trimmed.len()).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::{ranges_evaluated_after, terminal_call_candidates};
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::{ParseOptions, parse};
    use ruff_text_size::Ranged;

    /// each call `source`'s statement certainly makes, with what is evaluated after it, parsed as
    /// basedpython, whose grammar is python's and its own extensions
    fn after(source: &str) -> Vec<String> {
        let parsed = parse(source, ParseOptions::from(PySourceType::BasedPython))
            .unwrap()
            .try_into_module()
            .unwrap();
        let statement = &parsed.suite()[0];
        terminal_call_candidates(statement)
            .into_iter()
            .map(|call| {
                let ranges = ranges_evaluated_after(statement, call.range(), source)
                    .into_iter()
                    .map(|range| format!("`{}`", &source[range]))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("{}: {ranges}", &source[call.range()])
            })
            .collect()
    }

    #[test]
    fn a_call_whose_argument_never_returns_is_never_made() {
        assert_eq!(
            after("print(exit())"),
            ["exit(): `(` `)`", "print(exit()): "]
        );
    }

    #[test]
    fn later_arguments_are_never_evaluated() {
        assert_eq!(
            after("f(a(), exit(), b(), k=c())"),
            [
                "a(): `(` `, exit(), b(), k=c())`",
                "exit(): `(` `,` `, b(), k=c())`",
                "b(): `(` `,` `,` `, k=c())`",
                "c(): `(` `,` `,` `, k=` `)`",
                "f(a(), exit(), b(), k=c()): ",
            ]
        );
    }

    #[test]
    fn starred_arguments_are_evaluated_before_keywords() {
        assert_eq!(after("f(k=a(), *b())")[0], "b(): `(k=a(),` `*` `)`");
    }

    #[test]
    fn an_operation_and_the_operands_after_it() {
        assert_eq!(after("y = a + exit() + b"), ["exit(): `y =` `+` `+ b`"]);
    }

    #[test]
    fn a_return_and_a_store() {
        assert_eq!(after("return exit()"), ["exit(): `return`"]);
        assert_eq!(after("x[i()] = exit()")[0], "exit(): `x[i()] =`");
        assert_eq!(after("x[i()] += exit()")[1], "exit(): `+=`");
    }

    #[test]
    fn a_comprehension_runs_only_after_its_first_iterable() {
        assert_eq!(
            after("[g(y) for y in exit() if h(y)]"),
            ["exit(): `[g(y) for y in` `if h(y)]`"]
        );
    }

    #[test]
    fn what_may_not_run_is_no_candidate() {
        assert_eq!(after("a or exit()"), Vec::<String>::new());
        assert_eq!(after("lambda: exit()"), Vec::<String>::new());
        assert_eq!(after("x if c() else exit()"), ["c(): `x if` `else exit()`"]);
        assert_eq!(after("a?.b(exit())"), Vec::<String>::new());
        assert_eq!(after("assert exit()"), Vec::<String>::new());
    }

    #[test]
    fn a_short_circuit_after_the_call_is_never_reached() {
        assert_eq!(after("exit() or a"), ["exit(): `or a`"]);
    }

    #[test]
    fn a_compound_statement_header() {
        assert_eq!(
            after("for x in exit():\n    pass"),
            ["exit(): `for x in` `:`"]
        );
        assert_eq!(
            after("with a(), exit() as f:\n    pass"),
            ["a(): `with` `, exit() as f:`", "exit(): `with` `,` `as f:`"]
        );
        assert_eq!(after("if exit():\n    pass"), Vec::<String>::new());
    }

    #[test]
    fn an_awaited_call_is_the_candidate() {
        assert_eq!(
            after("x = await stop(a())"),
            ["a(): `x =` `await` `(` `)`", "await stop(a()): `x =`"]
        );
    }
}
