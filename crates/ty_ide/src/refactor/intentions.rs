//! small rewrites of one construct that leave what it means as it was: inverting an `if`,
//! De Morgan's law, a string's quotes, and a loop that builds a collection as a comprehension
//!
//! each is offered only where the checker can show the rewrite means the same. a comparison is
//! negated by flipping its operator only where the operands are python's own integers, strings or
//! bytes — `not x < 1` is not `x >= 1` for a float that is `nan` — and is written `not (...)`
//! otherwise. De Morgan's law changes what an `and` or an `or` evaluates to, which is one of its
//! operands, into a `bool`, so it is offered where only the truth of the result is read. a loop
//! becomes a comprehension only where nothing could tell the difference: nothing outside the loop
//! reads the loop's variable, which a comprehension does not bind where the loop is, nothing reads
//! the collection while it is built, and the loop has nothing a comprehension cannot hold — a
//! comment, or a `yield` or a walrus python does not allow in one

use ruff_diagnostics::Edit;
use ruff_python_ast::PythonVersion;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::token::parenthesized_range;
use ruff_python_ast::visitor::source_order::{
    SourceOrderVisitor, TraversalSignal, walk_expr, walk_node, walk_stmt,
};
use ruff_python_ast::{
    self as ast, AnyNodeRef, BoolOp, CmpOp, Expr, ExprRef, OperatorPrecedence, Stmt, StringFlags,
    UnaryOp,
};
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_semantic::HasType;
use ty_python_semantic::types::Type;
use ty_python_semantic::types::ide_support::{BuiltinScalar, builtin_scalar_kinds, is_super};

use ty_python_core::scope::FileScopeId;
use ty_python_core::semantic_index;

use super::evaluation::is_effect_free;
use super::flow::{reaching_definitions, resolving_scope};
use super::hazards::contains_named_expression;
use super::{Plan, RefactorContext, Refusal};

/// invert an `if` with an `else`: negate its condition and swap its two bodies
pub(super) fn invert_if(context: &RefactorContext<'_>, range: TextRange) -> Result<Plan, Refusal> {
    let source = context.source();
    let covering = covering_node(context.parsed.syntax().into(), range);
    let Some(statement) = covering.ancestors().find_map(|node| match node {
        AnyNodeRef::StmtIf(statement) => Some(statement),
        _ => None,
    }) else {
        return Err(Refusal::NotApplicable);
    };
    // asked about on the header: the `if` and its condition
    if range.start() > statement.test.end() {
        return Err(Refusal::NotApplicable);
    }
    let title = "Invert `if` condition";
    let [clause] = statement.elif_else_clauses.as_slice() else {
        return Err(Refusal::refused(
            title,
            if statement.elif_else_clauses.is_empty() {
                "it has no `else` to swap its body with"
            } else {
                "it has an `elif`, which would have to be nested to be inverted"
            },
        ));
    };
    if clause.test.is_some() {
        return Err(Refusal::refused(
            title,
            "it has an `elif`, which would have to be nested to be inverted",
        ));
    }
    if statement.pattern.is_some() || clause.pattern.is_some() {
        return Err(Refusal::NotApplicable);
    }
    let (Some(first), Some(last)) = (statement.body.first(), statement.body.last()) else {
        return Err(Refusal::NotApplicable);
    };
    let (Some(else_first), Some(else_last)) = (clause.body.first(), clause.body.last()) else {
        return Err(Refusal::NotApplicable);
    };
    let header_end = source.full_line_end(statement.test.end());
    let else_header_end = source.full_line_end(clause.start());
    if first.start() < header_end || else_first.start() < else_header_end {
        return Err(Refusal::refused(
            title,
            "a body is written on the line of its header",
        ));
    }
    let body = TextRange::new(header_end, source.full_line_end(last.end()));
    let else_body = TextRange::new(else_header_end, source.full_line_end(else_last.end()));
    let condition = negated(context, &statement.test);
    if let Some(line) = lost_comment(context, &[statement.test.range()], &condition.kept) {
        return Err(Refusal::refused(
            title,
            format!("the comment on line {line} would be lost"),
        ));
    }
    let condition = condition.in_place_of(
        context,
        ExprRef::from(&*statement.test),
        Some(AnyNodeRef::StmtIf(statement)),
    );
    Ok(Plan {
        title: title.to_string(),
        edits: vec![
            Edit::range_replacement(condition, statement.test.range()),
            Edit::range_replacement(with_line_end(&source[else_body]), body),
            Edit::range_replacement(with_line_end(&source[body]), else_body),
        ],
        name: None,
    })
}

fn with_line_end(text: &str) -> String {
    if text.ends_with(['\n', '\r']) {
        text.to_string()
    } else {
        format!("{text}\n")
    }
}

/// the one-based line of a comment inside one of `replaced` that none of `kept` holds, which a
/// rewrite writing only `kept`'s source in place of `replaced` would lose
fn lost_comment(
    context: &RefactorContext<'_>,
    replaced: &[TextRange],
    kept: &[TextRange],
) -> Option<u32> {
    let comment = context.parsed.tokens().iter().find(|token| {
        token.kind() == TokenKind::Comment
            && replaced
                .iter()
                .any(|range| range.contains_range(token.range()))
            && !kept.iter().any(|range| range.contains_range(token.range()))
    })?;
    Some(
        context
            .source()
            .count_lines(TextRange::up_to(comment.start()))
            + 1,
    )
}

/// rewrite the innermost `and` or `or` around the range by De Morgan's law: `a and b` as
/// `not (not a or not b)`, and `not (a or b)` as `not a and not b`
pub(super) fn de_morgan(context: &RefactorContext<'_>, range: TextRange) -> Result<Plan, Refusal> {
    let covering = covering_node(context.parsed.syntax().into(), range);
    let ancestors: Vec<AnyNodeRef> = covering.ancestors().collect();
    let Some(position) = ancestors
        .iter()
        .position(|node| matches!(node, AnyNodeRef::ExprBoolOp(_)))
    else {
        return Err(Refusal::NotApplicable);
    };
    let AnyNodeRef::ExprBoolOp(bool_op) = ancestors[position] else {
        return Err(Refusal::NotApplicable);
    };
    let title = "Apply De Morgan's law".to_string();
    let joined = match bool_op.op {
        BoolOp::And => BoolOp::Or,
        BoolOp::Or => BoolOp::And,
    };

    // `not (a or b)` becomes `not a and not b`, which is a `bool` either way
    if let Some(AnyNodeRef::ExprUnaryOp(not)) = ancestors.get(position + 1)
        && not.op == UnaryOp::Not
    {
        let joined = join(context, bool_op, joined);
        if let Some(line) = lost_comment(context, &[not.range()], &joined.kept) {
            return Err(Refusal::refused(
                title,
                format!("the comment on line {line} would be lost"),
            ));
        }
        let text = joined.in_place_of(
            context,
            ExprRef::UnaryOp(not),
            ancestors.get(position + 2).copied(),
        );
        return Ok(Plan {
            title,
            edits: vec![Edit::range_replacement(text, not.range())],
            name: None,
        });
    }

    // `a and b` evaluates to `a` or `b`, which `not (...)` makes a `bool`: only where the
    // truth of it is all that is read, or where it is a `bool` already
    let is_bool = builtin_scalar_kinds(&context.model, &Expr::BoolOp(bool_op.clone()))
        .is_some_and(|kinds| kinds.iter().all(|kind| *kind == BuiltinScalar::Bool));
    if !is_bool && !only_truth_is_read(&ancestors[position..]) {
        return Err(Refusal::refused(
            title,
            "its value is used, and the rewrite would make it a `bool`",
        ));
    }
    let inner = join(context, bool_op, joined);
    if let Some(line) = lost_comment(context, &[bool_op.range()], &inner.kept) {
        return Err(Refusal::refused(
            title,
            format!("the comment on line {line} would be lost"),
        ));
    }
    let text = Written {
        text: format!("not ({})", inner.text),
        precedence: OperatorPrecedence::Not,
        kept: inner.kept,
    }
    .in_place_of(
        context,
        ExprRef::BoolOp(bool_op),
        ancestors.get(position + 1).copied(),
    );
    Ok(Plan {
        title,
        edits: vec![Edit::range_replacement(text, bool_op.range())],
        name: None,
    })
}

/// the operands of `bool_op`, each negated, joined by `joined`
fn join(context: &RefactorContext<'_>, bool_op: &ast::ExprBoolOp, joined: BoolOp) -> Written {
    let precedence = OperatorPrecedence::from(joined);
    let mut kept = Vec::new();
    let text = bool_op
        .values
        .iter()
        .map(|value| {
            let operand = negated(context, value);
            kept.extend_from_slice(&operand.kept);
            // `and` and `or` evaluate to the same however a run of them is grouped, so an
            // operand as tight as the operator it is joined by needs no parentheses
            operand.within(|operand| operand >= precedence)
        })
        .collect::<Vec<_>>()
        .join(&format!(" {joined} "));
    Written {
        text,
        precedence,
        kept,
    }
}

/// whether what `ancestors[0]` evaluates to is only read for its truth: it is the condition of
/// an `if`, a `while`, an `assert`, a conditional expression or a comprehension's `if`, the
/// operand of `not`, or an operand of an `and` or an `or` that is itself only read for its truth
fn only_truth_is_read(ancestors: &[AnyNodeRef]) -> bool {
    let Some((node, rest)) = ancestors.split_first() else {
        return false;
    };
    let Some(parent) = rest.first() else {
        return false;
    };
    let range = node.range();
    match parent {
        AnyNodeRef::StmtIf(statement) => statement.test.range() == range,
        AnyNodeRef::ElifElseClause(clause) => clause
            .test
            .as_ref()
            .is_some_and(|test| test.range() == range),
        AnyNodeRef::StmtWhile(statement) => statement.test.range() == range,
        AnyNodeRef::StmtAssert(statement) => statement.test.range() == range,
        AnyNodeRef::ExprIf(expression) => expression.test.range() == range,
        AnyNodeRef::Comprehension(comprehension) => {
            comprehension.ifs.iter().any(|test| test.range() == range)
        }
        AnyNodeRef::ExprUnaryOp(unary) => unary.op == UnaryOp::Not,
        AnyNodeRef::ExprBoolOp(_) => only_truth_is_read(rest),
        _ => false,
    }
}

/// the text of an expression the rewrite writes, how tightly its operator binds, and the ranges
/// of the source it copies, outside which a comment it replaces is lost
struct Written {
    text: String,
    precedence: OperatorPrecedence,
    kept: Vec<TextRange>,
}

impl Written {
    /// the text as an operand, in parentheses unless `fits` says its precedence lets it stand
    /// without them
    fn within(self, fits: impl FnOnce(OperatorPrecedence) -> bool) -> String {
        if fits(self.precedence) {
            self.text
        } else {
            format!("({})", self.text)
        }
    }

    /// the text written in place of `node`, a child of `parent`: in parentheses where python
    /// would otherwise read it differently there, or where it spans lines, which only brackets
    /// allow. `node`'s own parentheses stay, since the edit replaces what is inside them
    fn in_place_of(
        self,
        context: &RefactorContext<'_>,
        node: ExprRef<'_>,
        parent: Option<AnyNodeRef<'_>>,
    ) -> String {
        let Some(parent) = parent else {
            return format!("({})", self.text);
        };
        if parenthesized_range(node, parent, context.parsed.tokens()).is_some() {
            return self.text;
        }
        let multiline = self.text.contains(['\n', '\r']);
        self.within(|precedence| !multiline && fits_in(precedence, parent, node.range()))
    }
}

/// whether an expression of `precedence`, written without parentheses as the child of `parent`
/// at `child`, is read as that one child
fn fits_in(precedence: OperatorPrecedence, parent: AnyNodeRef<'_>, child: TextRange) -> bool {
    match parent {
        // `and` and `or` evaluate to the same however a run of them is grouped
        AnyNodeRef::ExprBoolOp(bool_op) => precedence >= OperatorPrecedence::from(bool_op.op),
        AnyNodeRef::ExprUnaryOp(unary) => precedence >= OperatorPrecedence::from(unary.op),
        // a comparison inside another would chain with it
        AnyNodeRef::ExprCompare(_) => {
            precedence > OperatorPrecedence::ComparisonsMembershipIdentity
        }
        AnyNodeRef::ExprBinOp(bin_op) => {
            let operator = OperatorPrecedence::from(bin_op.op);
            match precedence.cmp(&operator) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                // `**` groups from the right, everything else from the left
                std::cmp::Ordering::Equal => {
                    (bin_op.left.range() == child) != operator.is_right_associative()
                }
            }
        }
        // `a if b else c` takes an `or` at most as its first two operands, and a conditional
        // expression as its last
        AnyNodeRef::ExprIf(if_expr) => {
            if if_expr.orelse.range() == child {
                precedence >= OperatorPrecedence::IfElse
            } else {
                precedence > OperatorPrecedence::IfElse
            }
        }
        AnyNodeRef::Comprehension(_) => precedence > OperatorPrecedence::IfElse,
        // a condition may also be a walrus
        AnyNodeRef::StmtIf(_) | AnyNodeRef::ElifElseClause(_) | AnyNodeRef::StmtWhile(_) => {
            precedence >= OperatorPrecedence::IfElse || precedence == OperatorPrecedence::Assign
        }
        AnyNodeRef::StmtAssert(_)
        | AnyNodeRef::StmtReturn(_)
        | AnyNodeRef::StmtExpr(_)
        | AnyNodeRef::StmtAssign(_)
        | AnyNodeRef::StmtAnnAssign(_)
        | AnyNodeRef::Arguments(_)
        | AnyNodeRef::Keyword(_)
        | AnyNodeRef::ExprList(_)
        | AnyNodeRef::ExprSet(_)
        | AnyNodeRef::ExprTuple(_)
        | AnyNodeRef::ExprLambda(_)
        | AnyNodeRef::ExprNamed(_) => precedence >= OperatorPrecedence::IfElse,
        _ => precedence >= OperatorPrecedence::CallAttribute,
    }
}

/// `expr` negated for its truth: `not` taken off, a comparison's operator flipped where that
/// means the same, an `and` and an `or` swapped by De Morgan's law, and `not` written before it
/// otherwise
fn negated(context: &RefactorContext<'_>, expr: &Expr) -> Written {
    let source = context.source();
    match expr {
        Expr::UnaryOp(unary) if unary.op == UnaryOp::Not => Written {
            text: source[unary.operand.range()].to_string(),
            precedence: OperatorPrecedence::from_expr(&unary.operand),
            kept: vec![unary.operand.range()],
        },
        Expr::Compare(compare)
            if let [left, right] = &*compare.operands
                && let Some(flipped) = flipped(context, compare) =>
        {
            // each operand keeps the parentheses it is written in
            let operand = |operand: &Expr| {
                context.parenthesized_range(operand, AnyNodeRef::ExprCompare(compare))
            };
            let (left, right) = (operand(left), operand(right));
            Written {
                text: format!("{} {} {}", &source[left], flipped.as_str(), &source[right]),
                precedence: OperatorPrecedence::ComparisonsMembershipIdentity,
                kept: vec![left, right],
            }
        }
        Expr::BoolOp(bool_op) => join(
            context,
            bool_op,
            match bool_op.op {
                BoolOp::And => BoolOp::Or,
                BoolOp::Or => BoolOp::And,
            },
        ),
        _ => {
            // a comparison is parenthesised too, though `not` binds looser than it, so that
            // `not (x > 1)` does not read as `not x` compared with `1`
            let operand = Written {
                text: source[expr.range()].to_string(),
                precedence: OperatorPrecedence::from_expr(expr),
                kept: Vec::new(),
            }
            .within(|precedence| precedence > OperatorPrecedence::ComparisonsMembershipIdentity);
            Written {
                text: format!("not {operand}"),
                precedence: OperatorPrecedence::Not,
                kept: vec![expr.range()],
            }
        }
    }
}

/// the operator that negates `compare`'s, where swapping it means the same
fn flipped(context: &RefactorContext<'_>, compare: &ast::ExprCompare) -> Option<CmpOp> {
    const INTEGERS: &[BuiltinScalar] = &[BuiltinScalar::Bool, BuiltinScalar::Int];
    const NUMBERS: &[BuiltinScalar] = &[
        BuiltinScalar::Bool,
        BuiltinScalar::Int,
        BuiltinScalar::Float,
        BuiltinScalar::Complex,
    ];
    let ([op], [left, right]) = (&*compare.ops, &*compare.operands) else {
        return None;
    };
    let op = *op;
    match op {
        // python defines these as the negations of each other
        CmpOp::Is => return Some(CmpOp::IsNot),
        CmpOp::IsNot => return Some(CmpOp::Is),
        CmpOp::In => return Some(CmpOp::NotIn),
        CmpOp::NotIn => return Some(CmpOp::In),
        _ => {}
    }
    let left = builtin_scalar_kinds(&context.model, left)?;
    let right = builtin_scalar_kinds(&context.model, right)?;
    let within = |kinds: &[BuiltinScalar], allowed: &[BuiltinScalar]| {
        kinds.iter().all(|kind| allowed.contains(kind))
    };
    let same_family = |allowed: &[BuiltinScalar]| within(&left, allowed) && within(&right, allowed);
    let totally_ordered = same_family(INTEGERS)
        || same_family(&[BuiltinScalar::Str])
        || same_family(&[BuiltinScalar::Bytes]);
    match op {
        // `==` and `!=` of python's own scalars are each other's negation, `nan` included
        CmpOp::Eq | CmpOp::NotEq if totally_ordered || same_family(NUMBERS) => {
            Some(if op == CmpOp::Eq {
                CmpOp::NotEq
            } else {
                CmpOp::Eq
            })
        }
        CmpOp::Lt if totally_ordered => Some(CmpOp::GtE),
        CmpOp::LtE if totally_ordered => Some(CmpOp::Gt),
        CmpOp::Gt if totally_ordered => Some(CmpOp::LtE),
        CmpOp::GtE if totally_ordered => Some(CmpOp::Lt),
        _ => None,
    }
}

/// write a string literal in the other quotes: `'it'` as `"it"`, escaping what the new quote
/// would end and unescaping the old one
pub(super) fn flip_quotes(
    context: &RefactorContext<'_>,
    range: TextRange,
) -> Result<Plan, Refusal> {
    let source = context.source();
    let covering = covering_node(context.parsed.syntax().into(), range);
    let Some((literal_range, flags)) = covering.ancestors().find_map(|node| match node {
        AnyNodeRef::StringLiteral(literal) => {
            Some((literal.range(), literal.flags.as_any_string_flags()))
        }
        AnyNodeRef::BytesLiteral(literal) => {
            Some((literal.range(), literal.flags.as_any_string_flags()))
        }
        _ => None,
    }) else {
        return Err(Refusal::NotApplicable);
    };
    if flags.is_triple_quoted() {
        return Err(Refusal::NotApplicable);
    }
    let old = flags.quote_str();
    let (new, title) = if old == "'" {
        ("\"", "Convert to a double-quoted string")
    } else {
        ("'", "Convert to a single-quoted string")
    };
    let text = &source[literal_range];
    let prefix_len = flags.prefix().text_len().to_usize();
    let body = &text[prefix_len + 1..text.len() - 1];
    let raw = flags.prefix().is_raw();
    let mut converted = String::with_capacity(body.len() + 2);
    let mut characters = body.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => match characters.next() {
                // the old quote needs no escape any more
                Some(next) if !raw && next.to_string() == old => converted.push(next),
                Some(next) => {
                    converted.push('\\');
                    converted.push(next);
                }
                None => converted.push('\\'),
            },
            character if character.to_string() == new => {
                if raw {
                    return Err(Refusal::refused(
                        title,
                        format!("a raw string cannot escape the {new} it holds"),
                    ));
                }
                converted.push('\\');
                converted.push(character);
            }
            character => converted.push(character),
        }
    }
    Ok(Plan {
        title: title.to_string(),
        edits: vec![Edit::range_replacement(
            format!("{}{new}{converted}{new}", &text[..prefix_len]),
            literal_range,
        )],
        name: None,
    })
}

/// rewrite a loop that only adds to a collection made empty right above it as a comprehension:
///
/// ```py
/// result = []                         result = [item * 2 for item in items if item > 0]
/// for item in items:
///     if item > 0:
///         result.append(item * 2)
/// ```
///
/// for a list's `append`, a set's `add` and a dict's item assignment
pub(super) fn loop_to_comprehension(
    context: &RefactorContext<'_>,
    range: TextRange,
) -> Result<Plan, Refusal> {
    let source = context.source();
    let covering = covering_node(context.parsed.syntax().into(), range);
    let Some(for_loop) = covering.ancestors().find_map(|node| match node {
        AnyNodeRef::StmtFor(statement) => Some(statement),
        _ => None,
    }) else {
        return Err(Refusal::NotApplicable);
    };
    // asked about on the header
    if range.start() > for_loop.iter.end() || for_loop.is_async || for_loop.pattern.is_some() {
        return Err(Refusal::NotApplicable);
    }
    let Some((suite, index)) = find_suite(context.parsed.suite(), for_loop) else {
        return Err(Refusal::NotApplicable);
    };
    let Some(Stmt::Assign(initial)) = index
        .checked_sub(1)
        .and_then(|previous| suite.get(previous))
    else {
        return Err(Refusal::NotApplicable);
    };
    let [Expr::Name(collection)] = initial.targets.as_slice() else {
        return Err(Refusal::NotApplicable);
    };
    let name = collection.id.as_str();

    // the body: one addition, under at most one `if`
    let (condition, addition) = match for_loop.body.as_slice() {
        [Stmt::If(statement)] if statement.elif_else_clauses.is_empty() => {
            match statement.body.as_slice() {
                [addition] => (Some(&*statement.test), addition),
                _ => return Err(Refusal::NotApplicable),
            }
        }
        [addition] => (None, addition),
        _ => return Err(Refusal::NotApplicable),
    };
    // what the comprehension writes of the loop: its element, as the expressions it is made of
    let (open, close, kind, elements): (_, _, _, Vec<&Expr>) = match (&*initial.value, addition) {
        (Expr::List(list), Stmt::Expr(expression)) if list.elts.is_empty() => {
            let element = added(&expression.value, name, "append").ok_or(Refusal::NotApplicable)?;
            ("[", "]", "list", vec![element])
        }
        (Expr::Call(call), Stmt::Expr(expression))
            if matches!(&*call.func, Expr::Name(func) if func.id.as_str() == "set")
                && call.arguments.is_empty() =>
        {
            let element = added(&expression.value, name, "add").ok_or(Refusal::NotApplicable)?;
            ("{", "}", "set", vec![element])
        }
        (Expr::Dict(dict), Stmt::Assign(assign)) if dict.is_empty() => {
            let [Expr::Subscript(target)] = assign.targets.as_slice() else {
                return Err(Refusal::NotApplicable);
            };
            if !matches!(&*target.value, Expr::Name(value) if value.id.as_str() == name)
                || target.slice.is_slice_expr()
            {
                return Err(Refusal::NotApplicable);
            }
            // the loop evaluates the value before the key, and a comprehension the key first
            if !is_effect_free(&context.model, &target.slice)
                && !is_effect_free(&context.model, &assign.value)
            {
                return Err(Refusal::refused(
                    "Convert loop to dict comprehension",
                    "a comprehension evaluates the key before the value, which the loop evaluates \
                     after it",
                ));
            }
            ("{", "}", "dict", vec![&*target.slice, &*assign.value])
        }
        _ => return Err(Refusal::NotApplicable),
    };
    let title = format!("Convert loop to {kind} comprehension");

    if !for_loop.orelse.is_empty() {
        return Err(Refusal::refused(
            title,
            "its `else` runs after the loop, which a comprehension has no place for",
        ));
    }
    let index = semantic_index(context.db, context.file);
    // `set` is python's own unless the program binds the name
    if let Expr::Call(call) = &*initial.value
        && let Expr::Name(func) = &*call.func
    {
        let shadowed = index
            .try_expression_scope_id(&ast::ExprRef::Name(func))
            .is_none_or(|scope| resolving_scope(index, scope, func.id.as_str()).is_some());
        if shadowed {
            return Err(Refusal::refused(title, "`set` is not python's own here"));
        }
    }
    // a class body's names are not visible inside a comprehension
    if in_class_body(context, for_loop) {
        return Err(Refusal::refused(
            title,
            "a comprehension in a class body cannot see the class's names",
        ));
    }
    let Some(loop_scope) = index.try_expression_scope_id(&ast::ExprRef::from(&*for_loop.iter))
    else {
        return Err(Refusal::NotApplicable);
    };
    let mut bound = Vec::new();
    names_in(&for_loop.target, &mut bound);

    // what python does not allow in a comprehension
    let kept: Vec<&Expr> = elements.iter().copied().chain(condition).collect();
    if kept.iter().any(|expr| yields(expr)) {
        return Err(Refusal::refused(
            title,
            "it adds what a `yield` evaluates to, which a comprehension cannot hold",
        ));
    }
    if let Some(variable) = bound
        .iter()
        .find(|variable| kept.iter().any(|expr| assigns_with_walrus(expr, variable)))
    {
        return Err(Refusal::refused(
            title,
            format!("a walrus assigns `{variable}`, which a comprehension does not allow"),
        ));
    }
    if contains_named_expression(&for_loop.iter) {
        return Err(Refusal::refused(
            title,
            "its iterable assigns with a walrus, which a comprehension does not allow",
        ));
    }
    // before 3.12 a comprehension runs as a function of its own, whose first parameter is the
    // iterator, which is what a zero-argument `super()` in it would be passed as its receiver
    if context.file.python_version(context.db) < PythonVersion::PY312
        && kept
            .iter()
            .copied()
            .chain([&*for_loop.target])
            .any(|expr| calls_zero_argument_super(context, expr))
    {
        return Err(Refusal::refused(
            title,
            "it calls `super()` without arguments, which in a comprehension before python 3.12 \
             is not passed the method's receiver",
        ));
    }

    // a comment the comprehension does not keep would be lost
    let removed = [
        initial.value.range(),
        TextRange::new(
            source.full_line_end(initial.end()),
            source.full_line_end(for_loop.end()),
        ),
    ];
    let kept_ranges: Vec<TextRange> = kept
        .iter()
        .map(Ranged::range)
        .chain([for_loop.target.range(), for_loop.iter.range()])
        .collect();
    if let Some(line) = lost_comment(context, &removed, &kept_ranges) {
        return Err(Refusal::refused(
            title,
            format!("the comment on line {line} has no place in a comprehension"),
        ));
    }

    // read while it is built, it would be read before it is bound
    let mut collection_reads = Reads::new(name);
    collection_reads.visit_expr(&for_loop.iter);
    collection_reads.visit_body(&for_loop.body);
    if collection_reads.count > 1 {
        return Err(Refusal::refused(
            title,
            format!("the loop reads `{name}` while it is built"),
        ));
    }
    if let Some(collection_scope) = resolving_scope(index, loop_scope, name) {
        let in_outer_loop = covering_node(context.parsed.syntax().into(), for_loop.range())
            .ancestors()
            .skip_while(|node| node.range() == for_loop.range())
            .take_while(|node| !is_scope_node(*node))
            .any(|node| matches!(node, AnyNodeRef::StmtFor(_) | AnyNodeRef::StmtWhile(_)));
        let read_while_built = loads_of(context, name).into_iter().any(|load| {
            let Some(scope) = index.try_expression_scope_id(&ast::ExprRef::Name(load)) else {
                return true;
            };
            if scope == loop_scope || resolving_scope(index, scope, name) != Some(collection_scope)
            {
                return false;
            }
            // a function made after the loop is not there to be called while it runs, unless
            // the loop runs again after it
            in_outer_loop
                || made_in(context, load, loop_scope_node(context, for_loop)) < for_loop.end()
        });
        if read_while_built {
            return Err(Refusal::refused(
                title,
                format!("a function reads `{name}`, and may be called while the loop builds it"),
            ));
        }
    }

    // a comprehension does not bind its variable where the loop is
    for variable in &bound {
        let table = index.place_table(loop_scope);
        if table.symbol_id(variable).is_some_and(|symbol| {
            let symbol = table.symbol(symbol);
            symbol.is_global() || symbol.is_nonlocal()
        }) {
            return Err(Refusal::refused(
                title,
                format!(
                    "`{variable}` is declared outside the function, where a comprehension would not assign it"
                ),
            ));
        }
        if let Some(read) = reads_loop_binding(context, variable, loop_scope, for_loop) {
            return Err(Refusal::refused(
                title,
                match read {
                    Read::After => format!(
                        "`{variable}` is read after the loop, which a comprehension leaves unbound"
                    ),
                    Read::Before => format!(
                        "`{variable}` is read above the loop, where the loop may have bound it, \
                         which a comprehension does not"
                    ),
                    Read::Nested => format!(
                        "a function reads `{variable}`, which a comprehension leaves unbound"
                    ),
                },
            ));
        }
    }

    // a list's or a set's element may be a walrus, a dict's key and value no looser than a
    // lambda, and the iterable and the condition no looser than an `or`
    let element = match elements.as_slice() {
        [key, value] => format!(
            "{}: {}",
            comprehension_part(source, key, OperatorPrecedence::Lambda),
            comprehension_part(source, value, OperatorPrecedence::Lambda)
        ),
        elements => elements
            .iter()
            .map(|element| comprehension_part(source, element, OperatorPrecedence::Assign))
            .collect(),
    };
    let mut comprehension = format!(
        "{open}{element} for {} in {}",
        &source[for_loop.target.range()],
        comprehension_part(source, &for_loop.iter, OperatorPrecedence::Or)
    );
    if let Some(condition) = condition {
        comprehension.push_str(" if ");
        comprehension.push_str(&comprehension_part(
            source,
            condition,
            OperatorPrecedence::Or,
        ));
    }
    comprehension.push_str(close);
    Ok(Plan {
        title,
        edits: vec![
            Edit::range_replacement(comprehension, initial.value.range()),
            Edit::range_deletion(removed[1]),
        ],
        name: None,
    })
}

/// whether `expr` calls the builtin `super` without arguments, under whatever name it is bound to,
/// or reads basedpython's `super` keyword, which stands for such a call
fn calls_zero_argument_super(context: &RefactorContext<'_>, expr: &Expr) -> bool {
    struct Super<'c, 'db> {
        context: &'c RefactorContext<'db>,
        found: bool,
    }
    impl<'a> SourceOrderVisitor<'a> for Super<'_, '_> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            let model = &self.context.model;
            match expr {
                Expr::Call(call)
                    if call.arguments.is_empty()
                        && call.func.inferred_type(model).is_some_and(|ty| {
                            !matches!(ty, Type::BoundSuper(_)) && is_super(model.db(), ty)
                        }) =>
                {
                    self.found = true;
                }
                // the keyword is `super` written as an attribute's base, which the checker gives
                // the bound super the lowering's `super()` evaluates to
                Expr::Attribute(attribute)
                    if matches!(&*attribute.value, Expr::Name(name) if name.id.as_str() == "super")
                        && matches!(
                            attribute.value.inferred_type(model),
                            Some(Type::BoundSuper(_))
                        ) =>
                {
                    self.found = true;
                }
                _ => {}
            }
            walk_expr(self, expr);
        }
    }
    let mut calls = Super {
        context,
        found: false,
    };
    calls.visit_expr(expr);
    calls.found
}

/// `expr`'s text as a part of a comprehension where nothing looser than `loosest` may stand
/// without parentheses, in them otherwise, and where it is a tuple or a generator that has none
/// of its own, which a comprehension would read as more of itself
fn comprehension_part(source: &str, expr: &Expr, loosest: OperatorPrecedence) -> String {
    let text = &source[expr.range()];
    let unbracketed = matches!(expr, Expr::Tuple(tuple) if !tuple.parenthesized)
        || matches!(expr, Expr::Generator(generator) if !generator.parenthesized);
    if unbracketed || OperatorPrecedence::from_expr(expr) < loosest {
        format!("({text})")
    } else {
        text.to_string()
    }
}

/// the element `name.method(element)` adds
fn added<'a>(expr: &'a Expr, name: &str, method: &str) -> Option<&'a Expr> {
    let Expr::Call(call) = expr else {
        return None;
    };
    let Expr::Attribute(attribute) = &*call.func else {
        return None;
    };
    if attribute.attr.as_str() != method
        || !matches!(&*attribute.value, Expr::Name(value) if value.id.as_str() == name)
    {
        return None;
    }
    match (&*call.arguments.args, call.arguments.keywords.is_empty()) {
        ([element], true) if !element.is_starred_expr() => Some(element),
        _ => None,
    }
}

/// the suite `for_loop` is written in, and its index there
fn find_suite<'a>(body: &'a [Stmt], for_loop: &ast::StmtFor) -> Option<(&'a [Stmt], usize)> {
    for (index, statement) in body.iter().enumerate() {
        if statement.range() == for_loop.range() {
            return Some((body, index));
        }
        if statement.range().contains_range(for_loop.range()) {
            for nested in super::text::nested_suites(statement) {
                if let Some(found) = find_suite(nested, for_loop) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// counts the loads of a name
struct Reads<'a> {
    name: &'a str,
    count: usize,
}

impl<'a> Reads<'a> {
    fn new(name: &'a str) -> Self {
        Self { name, count: 0 }
    }
}

impl<'b> SourceOrderVisitor<'b> for Reads<'_> {
    fn visit_expr(&mut self, expr: &'b Expr) {
        if let Expr::Name(name) = expr
            && name.id.as_str() == self.name
        {
            self.count += 1;
        }
        walk_expr(self, expr);
    }
}

fn names_in(target: &Expr, names: &mut Vec<String>) {
    match target {
        Expr::Name(name) => names.push(name.id.to_string()),
        Expr::Tuple(tuple) => tuple
            .elts
            .iter()
            .for_each(|element| names_in(element, names)),
        Expr::List(list) => list
            .elts
            .iter()
            .for_each(|element| names_in(element, names)),
        Expr::Starred(starred) => names_in(&starred.value, names),
        _ => {}
    }
}

/// whether a node starts a scope of its own
fn is_scope_node(node: AnyNodeRef<'_>) -> bool {
    matches!(
        node,
        AnyNodeRef::StmtFunctionDef(_)
            | AnyNodeRef::StmtClassDef(_)
            | AnyNodeRef::ExprLambda(_)
            | AnyNodeRef::ExprListComp(_)
            | AnyNodeRef::ExprSetComp(_)
            | AnyNodeRef::ExprDictComp(_)
            | AnyNodeRef::ExprGenerator(_)
    )
}

/// the range of the function the loop is in, or of the module
fn loop_scope_node(context: &RefactorContext<'_>, for_loop: &ast::StmtFor) -> TextRange {
    covering_node(context.parsed.syntax().into(), for_loop.range())
        .ancestors()
        .find_map(|node| match node {
            AnyNodeRef::StmtFunctionDef(function) => Some(function.range()),
            _ => None,
        })
        .unwrap_or_else(|| context.parsed.syntax().range())
}

/// where the scope that `load` is read in is made, as it is written in the scope `within`
/// spans: the start of the outermost function, class, lambda or comprehension around the load
/// inside `within`
fn made_in(context: &RefactorContext<'_>, load: &ast::ExprName, within: TextRange) -> TextSize {
    covering_node(context.parsed.syntax().into(), load.range())
        .ancestors()
        .take_while(|node| node.range() != within)
        .filter(|node| is_scope_node(*node))
        .last()
        .map_or(load.start(), |node| node.start())
}

/// every read of `name` in the module: its loads, and the `del` and augmented assignment of it,
/// which read it too
fn loads_of<'a>(context: &'a RefactorContext<'_>, name: &'a str) -> Vec<&'a ast::ExprName> {
    struct Loads<'a> {
        name: &'a str,
        found: Vec<&'a ast::ExprName>,
    }
    impl<'a> SourceOrderVisitor<'a> for Loads<'a> {
        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            if let Stmt::AugAssign(assign) = stmt
                && let Expr::Name(target) = &*assign.target
                && target.id.as_str() == self.name
            {
                self.found.push(target);
            }
            walk_stmt(self, stmt);
        }

        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Name(name) = expr
                && name.id.as_str() == self.name
                && (name.ctx.is_load() || name.ctx.is_del())
            {
                self.found.push(name);
            }
            walk_expr(self, expr);
        }
    }
    let mut loads = Loads {
        name,
        found: Vec::new(),
    };
    loads.visit_body(context.parsed.suite());
    loads.found
}

/// where a read of `variable` outside the loop's body is
enum Read {
    After,
    Before,
    Nested,
}

/// a read of `variable` outside the loop's body that may read what the loop bound to it, which a
/// comprehension would not bind there: one the loop's binding reaches, one that finds the name
/// bound nowhere, where a comprehension would leave the name to the scopes around, or one in a
/// nested scope that resolves to the loop's, which could run at any time
fn reads_loop_binding(
    context: &RefactorContext<'_>,
    variable: &str,
    loop_scope: FileScopeId,
    for_loop: &ast::StmtFor,
) -> Option<Read> {
    let index = semantic_index(context.db, context.file);
    let body = TextRange::new(for_loop.body.first()?.start(), for_loop.body.last()?.end());
    let target = for_loop.target.range();
    loads_of(context, variable)
        .into_iter()
        .filter(|load| !body.contains_range(load.range()))
        .find_map(|load| {
            let Some(scope) = index.try_expression_scope_id(&ast::ExprRef::Name(load)) else {
                return Some(Read::Nested);
            };
            if scope != loop_scope {
                return (resolving_scope(index, scope, variable) == Some(loop_scope))
                    .then_some(Read::Nested);
            }
            let reaching = reaching_definitions(context.db, index, scope, load);
            let reads_it = reaching.maybe_unbound && reaching.definitions.is_empty()
                || reaching.definitions.iter().any(|definition| {
                    target
                        .contains_range(definition.focus_range(context.db, &context.parsed).range())
                });
            reads_it.then(|| {
                if load.start() >= for_loop.end() {
                    Read::After
                } else {
                    Read::Before
                }
            })
        })
}

/// whether `expr` evaluates a `yield` in its own scope
fn yields(expr: &Expr) -> bool {
    struct Yields(bool);
    impl<'a> SourceOrderVisitor<'a> for Yields {
        fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
            if matches!(
                node,
                AnyNodeRef::ExprYield(_) | AnyNodeRef::ExprYieldFrom(_)
            ) {
                self.0 = true;
            }
            // a lambda's `yield` is the lambda's own
            if self.0 || node.is_expr_lambda() {
                TraversalSignal::Skip
            } else {
                TraversalSignal::Traverse
            }
        }
    }
    let mut yields = Yields(false);
    walk_node(&mut yields, AnyNodeRef::from(expr));
    yields.0
}

/// whether `expr` assigns `variable` with a walrus
fn assigns_with_walrus(expr: &Expr, variable: &str) -> bool {
    struct Walrus<'a> {
        variable: &'a str,
        found: bool,
    }
    impl<'a> SourceOrderVisitor<'a> for Walrus<'_> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Named(named) = expr
                && matches!(&*named.target, Expr::Name(target) if target.id.as_str() == self.variable)
            {
                self.found = true;
            }
            walk_expr(self, expr);
        }
    }
    let mut walrus = Walrus {
        variable,
        found: false,
    };
    walrus.visit_expr(expr);
    walrus.found
}

fn in_class_body(context: &RefactorContext<'_>, for_loop: &ast::StmtFor) -> bool {
    let covering = covering_node(context.parsed.syntax().into(), for_loop.range());
    covering
        .ancestors()
        .skip_while(|node| node.range() == for_loop.range())
        .find(|node| {
            matches!(
                node,
                AnyNodeRef::StmtClassDef(_) | AnyNodeRef::StmtFunctionDef(_)
            )
        })
        .is_some_and(|node| matches!(node, AnyNodeRef::StmtClassDef(_)))
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use ruff_python_ast::PythonVersion;

    use crate::refactor::RefactorKind;
    use crate::refactor::test_support::RefactorTest;

    #[test]
    fn invert_if_flips_a_comparison_of_integers() {
        let test = RefactorTest::basedpython(
            r#"
            def f(a: final int, b: bool):
                <CURSOR>if a > 1 and not b:
                    print("yes")
                else:
                    print("no")
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::InvertIf), @r#"
        Invert `if` condition
        ---

        def f(a: final int, b: bool):
            if a <= 1 or b:
                print("no")
            else:
                print("yes")
        "#);
    }

    /// an `int` may be an instance of a subclass, whose `>` is its own and need not be the
    /// negation of its `<=`
    #[test]
    fn invert_if_keeps_not_for_an_int_that_may_be_a_subclass() {
        let test = RefactorTest::python(
            r#"
            def f(a: int):
                <CURSOR>if a > 1:
                    return 1
                else:
                    return 2
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::InvertIf), @"
        Invert `if` condition
        ---

        def f(a: int):
            if not (a > 1):
                return 2
            else:
                return 1
        ");
    }

    /// `not x > 1` is not `x <= 1` for a float that is `nan`
    #[test]
    fn invert_if_keeps_not_for_a_float() {
        let test = RefactorTest::python(
            r#"
            def f(x: float):
                <CURSOR>if x > 1:
                    return 1
                else:
                    return 2
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::InvertIf), @"
        Invert `if` condition
        ---

        def f(x: float):
            if not (x > 1):
                return 2
            else:
                return 1
        ");
    }

    #[test]
    fn invert_if_without_else_is_refused() {
        let test = RefactorTest::python(
            r#"
            def f(a):
                <CURSOR>if a:
                    return 1
                return 2
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::InvertIf), @"refused: Invert `if` condition (it has no `else` to swap its body with)");
    }

    #[test]
    fn de_morgan_in_a_condition() {
        let test = RefactorTest::python(
            r#"
            def f(a, b):
                if not (a <CURSOR>or b):
                    return 1
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::DeMorgan), @"
        Apply De Morgan's law
        ---

        def f(a, b):
            if not a and not b:
                return 1
        ");
        let test = RefactorTest::python(
            r#"
            def f(a, b):
                if a <CURSOR>and not b:
                    return 2
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::DeMorgan), @"
        Apply De Morgan's law
        ---

        def f(a, b):
            if not (not a or b):
                return 2
        ");
    }

    /// the rewritten `or` binds looser than the `and` it is an operand of, so it is
    /// parenthesised, on either side of the `and`
    #[test]
    fn de_morgan_parenthesises_an_or_under_an_and() {
        let test = RefactorTest::python(
            r#"
            def f(a, b, c):
                if c and not (a <CURSOR>and b):
                    return 1
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::DeMorgan), @"
        Apply De Morgan's law
        ---

        def f(a, b, c):
            if c and (not a or not b):
                return 1
        ");
        let test = RefactorTest::python(
            r#"
            def f(a, b, c):
                if not (a <CURSOR>and b) and c:
                    return 1
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::DeMorgan), @"
        Apply De Morgan's law
        ---

        def f(a, b, c):
            if (not a or not b) and c:
                return 1
        ");
    }

    /// an `and` binds tighter than the `or` it is an operand of, and an `or` among the operands
    /// of another `or` groups the same either way, so neither is parenthesised
    #[test]
    fn de_morgan_leaves_an_operand_unparenthesised_where_it_binds_tightly_enough() {
        let test = RefactorTest::python(
            r#"
            def f(a, b, c):
                if c or not (a <CURSOR>or b):
                    return 1
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::DeMorgan), @"
        Apply De Morgan's law
        ---

        def f(a, b, c):
            if c or not a and not b:
                return 1
        ");
        let test = RefactorTest::python(
            r#"
            def f(a, b, c):
                if not (a <CURSOR>and not (b or c)):
                    return 1
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::DeMorgan), @"
        Apply De Morgan's law
        ---

        def f(a, b, c):
            if not a or b or c:
                return 1
        ");
    }

    /// a walrus taken out of its `not` keeps parentheses among the operands of an `or`
    #[test]
    fn de_morgan_parenthesises_a_walrus_operand() {
        let test = RefactorTest::python(
            r#"
            def f(a, g):
                if a <CURSOR>and not (x := g()):
                    return x
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::DeMorgan), @"
        Apply De Morgan's law
        ---

        def f(a, g):
            if not (not a or (x := g())):
                return x
        ");
    }

    /// each operand of a flipped comparison keeps the parentheses it is written in
    #[test]
    fn invert_if_keeps_a_comparison_operand_s_parentheses() {
        let test = RefactorTest::python(
            r#"
            def f(a, b):
                <CURSOR>if (a or b) is None:
                    return 1
                else:
                    return 2
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::InvertIf), @"
        Invert `if` condition
        ---

        def f(a, b):
            if (a or b) is not None:
                return 2
            else:
                return 1
        ");
        let test = RefactorTest::python(
            r#"
            def f(g):
                <CURSOR>if (x := g()) is None:
                    return 1
                else:
                    return x
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::InvertIf), @"
        Invert `if` condition
        ---

        def f(g):
            if (x := g()) is not None:
                return x
            else:
                return 1
        ");
    }

    /// a condition that loses its `not` loses the parentheses it needed only under the `not`,
    /// but keeps them where it spans lines
    #[test]
    fn invert_if_takes_off_a_not() {
        let test = RefactorTest::python(
            r#"
            def f(a, b):
                <CURSOR>if not (a or b):
                    return 1
                else:
                    return 2
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::InvertIf), @"
        Invert `if` condition
        ---

        def f(a, b):
            if a or b:
                return 2
            else:
                return 1
        ");
        let test = RefactorTest::python(
            r#"
            def f(a, b):
                <CURSOR>if not (a or
                        b):
                    return 1
                else:
                    return 2
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::InvertIf), @"
        Invert `if` condition
        ---

        def f(a, b):
            if (a or
                    b):
                return 2
            else:
                return 1
        ");
    }

    /// `a and b` is `a` or `b`, which the rewrite would make a `bool`
    #[test]
    fn de_morgan_on_a_value_is_refused() {
        let test = RefactorTest::python(
            r#"
            def f(a: str, b: str):
                return a <CURSOR>and b
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::DeMorgan), @"refused: Apply De Morgan's law (its value is used, and the rewrite would make it a `bool`)");
    }

    #[test]
    fn flip_quotes_escapes_and_unescapes() {
        let test = RefactorTest::python("x = 'sin<CURSOR>gle'\n");
        assert_snapshot!(test.apply(RefactorKind::FlipQuotes), @r#"
        Convert to a double-quoted string
        ---
        x = "single"
        "#);
        let test = RefactorTest::python("x = \"it'<CURSOR>s\"\n");
        assert_snapshot!(test.apply(RefactorKind::FlipQuotes), @r"
        Convert to a single-quoted string
        ---
        x = 'it\'s'
        ");
        let test = RefactorTest::python("x = 'say \\'hi\\' \"<CURSOR>now\"'\n");
        assert_snapshot!(test.apply(RefactorKind::FlipQuotes), @r#"
        Convert to a double-quoted string
        ---
        x = "say 'hi' \"now\""
        "#);
        let test = RefactorTest::python("x = r'it\"<CURSOR>s'\n");
        assert_snapshot!(test.apply(RefactorKind::FlipQuotes), @r#"refused: Convert to a double-quoted string (a raw string cannot escape the " it holds)"#);
    }

    #[test]
    fn a_loop_that_builds_a_list_becomes_a_comprehension() {
        let test = RefactorTest::python(
            r#"
            def f(items: list[int]):
                result = []
                <CURSOR>for item in items:
                    if item > 0:
                        result.append(item * 2)
                return result
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"
        Convert loop to list comprehension
        ---

        def f(items: list[int]):
            result = [item * 2 for item in items if item > 0]
            return result
        ");
        let test = RefactorTest::python(
            r#"
            def f(items: list[int]):
                squares = {}
                <CURSOR>for item in items:
                    squares[item] = item * item
                return squares
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"
        Convert loop to dict comprehension
        ---

        def f(items: list[int]):
            squares = {item: item * item for item in items}
            return squares
        ");
    }

    /// a comprehension leaves `item` unbound, which the loop left bound to the last item
    #[test]
    fn a_loop_whose_variable_is_read_after_it_is_refused() {
        let test = RefactorTest::python(
            r#"
            def f(items: list[int]):
                result = []
                <CURSOR>for item in items:
                    result.append(item)
                return result, item
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"refused: Convert loop to list comprehension (`item` is read after the loop, which a comprehension leaves unbound)");
    }

    #[test]
    fn a_loop_that_reads_what_it_builds_is_refused() {
        let test = RefactorTest::python(
            r#"
            def f(items: list[int]):
                seen = []
                <CURSOR>for item in items:
                    if item not in seen:
                        seen.append(item)
                return seen
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"refused: Convert loop to list comprehension (the loop reads `seen` while it is built)");
    }
    /// a closure made before the loop reads the `item` the loop leaves bound, which a
    /// comprehension would not bind in the function at all
    #[test]
    fn a_loop_whose_variable_a_closure_reads_is_refused() {
        let test = RefactorTest::python(
            r#"
            def f(items: list[int]):
                last = lambda: item
                result = []
                <CURSOR>for item in items:
                    result.append(item)
                return result, last()
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"refused: Convert loop to list comprehension (a function reads `item`, which a comprehension leaves unbound)");
    }

    /// inside another loop, a read of `item` above the loop reads what the loop bound on the
    /// round before
    #[test]
    fn a_loop_whose_variable_is_read_before_it_on_the_next_round_is_refused() {
        let test = RefactorTest::python(
            r#"
            def f(items: list[int], rounds: int):
                for _ in range(rounds):
                    if rounds > 1:
                        print(item)
                    result = []
                    <CURSOR>for item in items:
                        result.append(item)
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"refused: Convert loop to list comprehension (`item` is read above the loop, where the loop may have bound it, which a comprehension does not)");
    }

    /// a function that reads the collection may be called while the loop builds it, where a
    /// comprehension has not bound it yet
    #[test]
    fn a_loop_whose_collection_a_closure_reads_is_refused() {
        let test = RefactorTest::python(
            r#"
            def f(items: list[int]):
                def counted(item):
                    print(len(result))
                    return item
                result = []
                <CURSOR>for item in items:
                    result.append(counted(item))
                return result
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"refused: Convert loop to list comprehension (a function reads `result`, and may be called while the loop builds it)");
    }

    /// `yield` is not allowed inside a comprehension
    #[test]
    fn a_loop_that_adds_what_it_yields_is_refused() {
        let test = RefactorTest::python(
            r#"
            def f(items: list[int]):
                result = []
                <CURSOR>for item in items:
                    result.append((yield item))
                return result
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"refused: Convert loop to list comprehension (it adds what a `yield` evaluates to, which a comprehension cannot hold)");
    }

    /// a walrus may not rebind a comprehension's variable, nor be part of its iterable
    #[test]
    fn a_loop_with_a_walrus_a_comprehension_rejects_is_refused() {
        let test = RefactorTest::python(
            r#"
            def f(items: list[int]):
                result = []
                <CURSOR>for item in items:
                    result.append(item := item + 1)
                return result
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"refused: Convert loop to list comprehension (a walrus assigns `item`, which a comprehension does not allow)");
        let test = RefactorTest::python(
            r#"
            def f(items: list[int]):
                result = []
                <CURSOR>for item in (chosen := items):
                    result.append(item)
                return result, chosen
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"refused: Convert loop to list comprehension (its iterable assigns with a walrus, which a comprehension does not allow)");
    }

    /// the loop's comments have no place in a comprehension, unless they are inside what it
    /// keeps
    #[test]
    fn a_loop_with_a_comment_is_refused() {
        let test = RefactorTest::python(
            r#"
            def f(items: list[int]):
                result = []
                <CURSOR>for item in items:
                    # doubled, since the total is halved later
                    result.append(item * 2)
                return result
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"refused: Convert loop to list comprehension (the comment on line 5 has no place in a comprehension)");
        let test = RefactorTest::python(
            r#"
            def f(items: list[int]):
                result = []
                <CURSOR>for item in items:
                    result.append(min(
                        item,  # never above the cap
                        10,
                    ))
                return result
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"
        Convert loop to list comprehension
        ---

        def f(items: list[int]):
            result = [min(
                    item,  # never above the cap
                    10,
                ) for item in items]
            return result
        ");
    }

    /// a set is built by `add`, and a dict by item assignment, whatever its key reads like
    #[test]
    fn a_loop_that_builds_a_set_becomes_a_set_comprehension() {
        let test = RefactorTest::python(
            r#"
            def f(items: list[str]):
                seen = set()
                <CURSOR>for item in items:
                    seen.add(item + ": ")
                return seen
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @r#"
        Convert loop to set comprehension
        ---

        def f(items: list[str]):
            seen = {item + ": " for item in items}
            return seen
        "#);
        let test = RefactorTest::python(
            r#"
            def f(items: list[str]):
                labels = {}
                <CURSOR>for item in items:
                    labels[item]=item
                return labels
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"
        Convert loop to dict comprehension
        ---

        def f(items: list[str]):
            labels = {item: item for item in items}
            return labels
        ");
    }

    /// what python would read as more of the comprehension keeps parentheses: a generator
    /// passed bare, a condition with a walrus, an iterable that is a conditional expression
    #[test]
    fn a_comprehension_part_keeps_the_parentheses_it_needs() {
        let test = RefactorTest::python(
            r#"
            def f(groups: list[list[int]], spare: list[int], g):
                result = []
                <CURSOR>for group in groups if groups else spare:
                    if (found := g(group)):
                        result.append(sum(x for x in group) for _ in found)
                return result
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"
        Convert loop to list comprehension
        ---

        def f(groups: list[list[int]], spare: list[int], g):
            result = [(sum(x for x in group) for _ in found) for group in (groups if groups else spare) if (found := g(group))]
            return result
        ");
        let test = RefactorTest::python(
            r#"
            def f(items: list[int]):
                pairs = {}
                <CURSOR>for item in items:
                    pairs[item, item] = item, item
                return pairs
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"
        Convert loop to dict comprehension
        ---

        def f(items: list[int]):
            pairs = {(item, item): (item, item) for item in items}
            return pairs
        ");
    }

    /// a later loop binds `item` afresh before reading it, so the first loop's `item` is not
    /// read after it
    #[test]
    fn a_variable_rebound_before_it_is_read_again_does_not_refuse() {
        let test = RefactorTest::python(
            r#"
            def f(items: list[int]):
                result = []
                <CURSOR>for item in items:
                    result.append(item)
                names = []
                for item in items:
                    names.append(str(item))
                return result, names
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"
        Convert loop to list comprehension
        ---

        def f(items: list[int]):
            result = [item for item in items]
            names = []
            for item in items:
                names.append(str(item))
            return result, names
        ");
    }

    /// a comment between the operands has nowhere to go once they are rewritten one by one
    #[test]
    fn de_morgan_with_a_comment_between_the_operands_is_refused() {
        let test = RefactorTest::python(
            r#"
            def f(a, b):
                if not (
                    a  # the first
                    <CURSOR>or b
                ):
                    return 1
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::DeMorgan), @"refused: Apply De Morgan's law (the comment on line 4 would be lost)");
        let test = RefactorTest::python(
            r#"
            def f(a, b):
                if (
                    a  # the first
                    <CURSOR>and b
                ):
                    return 1
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::DeMorgan), @"refused: Apply De Morgan's law (the comment on line 4 would be lost)");
    }

    /// the `not (...)` the inversion takes off would take a comment inside it with it, while
    /// one inside the operand it keeps stays
    #[test]
    fn invert_if_keeps_or_refuses_a_comment_in_its_condition() {
        let test = RefactorTest::python(
            r#"
            def f(a, b):
                <CURSOR>if not (  # either
                    a or b
                ):
                    return 1
                else:
                    return 2
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::InvertIf), @"refused: Invert `if` condition (the comment on line 3 would be lost)");
        let test = RefactorTest::python(
            r#"
            def f(a, b):
                <CURSOR>if not (
                    a  # the first
                    or b
                ):
                    return 1
                else:
                    return 2
            "#,
        );
        assert_snapshot!(test.apply(RefactorKind::InvertIf), @"
        Invert `if` condition
        ---

        def f(a, b):
            if (a  # the first
                or b):
                return 2
            else:
                return 1
        ");
    }

    /// before 3.12 a comprehension runs as a function of its own, where a zero-argument
    /// `super()` is passed the iterator rather than the method's receiver
    #[test]
    fn a_loop_calling_zero_argument_super_needs_python_3_12() {
        let source = r#"
            class Base:
                def scale(self, x):
                    return x


            class Child(Base):
                def scaled(self, items):
                    result = []
                    <CURSOR>for item in items:
                        result.append(super().scale(item))
                    return result
            "#;
        let test = RefactorTest::python(source).with_python_version(PythonVersion::PY311);
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"refused: Convert loop to list comprehension (it calls `super()` without arguments, which in a comprehension before python 3.12 is not passed the method's receiver)");
        let test = RefactorTest::python(source).with_python_version(PythonVersion::PY312);
        assert_snapshot!(test.apply(RefactorKind::LoopToComprehension), @"
        Convert loop to list comprehension
        ---

        class Base:
            def scale(self, x):
                return x


        class Child(Base):
            def scaled(self, items):
                result = [super().scale(item) for item in items]
                return result
        ");
    }
}
