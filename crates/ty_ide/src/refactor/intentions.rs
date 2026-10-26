//! Small rewrites of one construct that leave what it means as it was: inverting an `if`,
//! De Morgan's law, a string's quotes, and a loop that builds a collection as a comprehension.
//!
//! Each is offered only where the checker can show the rewrite means the same. A comparison is
//! negated by flipping its operator only where the operands are python's own integers, strings or
//! bytes — `not x < 1` is not `x >= 1` for a float that is `nan` — and is written `not (...)`
//! otherwise. De Morgan's law changes what an `and` or an `or` evaluates to, which is one of its
//! operands, into a `bool`, so it is offered where only the truth of the result is read. A loop
//! becomes a comprehension only where nothing could tell the difference: the loop's variable is
//! not read after it, which a comprehension does not leave bound, and the collection is not read
//! while it is built.

use ruff_diagnostics::Edit;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr};
use ruff_python_ast::{self as ast, AnyNodeRef, BoolOp, CmpOp, Expr, Stmt, StringFlags, UnaryOp};
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_semantic::types::ide_support::{BuiltinScalar, builtin_scalar_kinds};

use ty_python_core::semantic_index;

use super::evaluation::is_effect_free;
use super::flow::{reaching_definitions, resolving_scope};
use super::{Plan, RefactorContext, Refusal};

/// Invert an `if` with an `else`: negate its condition and swap its two bodies.
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
    let condition = negated(context, &statement.test).text;
    Ok(Plan {
        title: title.to_string(),
        edits: vec![
            Edit::range_replacement(condition, statement.test.range()),
            Edit::range_replacement(with_line_end(&source[else_body]), body),
            Edit::range_replacement(with_line_end(&source[body]), else_body),
        ],
    })
}

fn with_line_end(text: &str) -> String {
    if text.ends_with(['\n', '\r']) {
        text.to_string()
    } else {
        format!("{text}\n")
    }
}

/// Rewrite the innermost `and` or `or` around the range by De Morgan's law: `a and b` as
/// `not (not a or not b)`, and `not (a or b)` as `not a and not b`.
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
        BoolOp::And => "or",
        BoolOp::Or => "and",
    };

    // `not (a or b)` becomes `not a and not b`, which is a `bool` either way
    if let Some(AnyNodeRef::ExprUnaryOp(not)) = ancestors.get(position + 1)
        && not.op == UnaryOp::Not
    {
        let text = join(context, bool_op, joined);
        return Ok(Plan {
            title,
            edits: vec![Edit::range_replacement(text, not.range())],
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
    Ok(Plan {
        title,
        edits: vec![Edit::range_replacement(
            format!("not ({inner})"),
            bool_op.range(),
        )],
    })
}

/// The operands of `bool_op`, each negated, joined by `joined`.
fn join(context: &RefactorContext<'_>, bool_op: &ast::ExprBoolOp, joined: &str) -> String {
    bool_op
        .values
        .iter()
        .map(|value| {
            let negated = negated(context, value);
            // an `or` among the operands of an `and` keeps its own parentheses
            if negated.is_or && joined == "and" {
                format!("({})", negated.text)
            } else {
                negated.text
            }
        })
        .collect::<Vec<_>>()
        .join(&format!(" {joined} "))
}

/// Whether what `ancestors[0]` evaluates to is only read for its truth: it is the condition of
/// an `if`, a `while`, an `assert`, a conditional expression or a comprehension's `if`, the
/// operand of `not`, or an operand of an `and` or an `or` that is itself only read for its truth.
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

/// An expression negated, and whether the negation is an `or` at its top.
struct Negated {
    text: String,
    is_or: bool,
}

/// `expr` negated for its truth: `not` taken off, a comparison's operator flipped where that
/// means the same, an `and` and an `or` swapped by De Morgan's law, and `not (...)` otherwise.
fn negated(context: &RefactorContext<'_>, expr: &Expr) -> Negated {
    let source = context.source();
    let text = |expr: &Expr| source[expr.range()].to_string();
    match expr {
        Expr::UnaryOp(unary) if unary.op == UnaryOp::Not => Negated {
            is_or: matches!(&*unary.operand, Expr::BoolOp(inner) if inner.op == BoolOp::Or),
            text: if unary.operand.is_named_expr() {
                format!("({})", text(&unary.operand))
            } else {
                text(&unary.operand)
            },
        },
        Expr::Compare(compare)
            if let [left, right] = &*compare.operands
                && let Some(flipped) = flipped(context, compare) =>
        {
            Negated {
                text: format!("{} {} {}", text(left), flipped.as_str(), text(right)),
                is_or: false,
            }
        }
        Expr::BoolOp(bool_op) => {
            let joined = match bool_op.op {
                BoolOp::And => "or",
                BoolOp::Or => "and",
            };
            Negated {
                text: join(context, bool_op, joined),
                is_or: joined == "or",
            }
        }
        Expr::Name(_) | Expr::Attribute(_) | Expr::Call(_) | Expr::Subscript(_) => Negated {
            text: format!("not {}", text(expr)),
            is_or: false,
        },
        _ => Negated {
            text: format!("not ({})", text(expr)),
            is_or: false,
        },
    }
}

/// The operator that negates `compare`'s, where swapping it means the same.
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

/// Write a string literal in the other quotes: `'it'` as `"it"`, escaping what the new quote
/// would end and unescaping the old one.
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
    })
}

/// Rewrite a loop that only adds to a collection made empty right above it as a comprehension:
///
/// ```py
/// result = []                         result = [item * 2 for item in items if item > 0]
/// for item in items:
///     if item > 0:
///         result.append(item * 2)
/// ```
///
/// for a list's `append`, a set's `add` and a dict's item assignment.
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
    let (open, close, element) = match (&*initial.value, addition) {
        (Expr::List(list), Stmt::Expr(expression)) if list.elts.is_empty() => {
            let element = added(&expression.value, name, "append").ok_or(Refusal::NotApplicable)?;
            ("[", "]", source[element.range()].to_string())
        }
        (Expr::Call(call), Stmt::Expr(expression))
            if matches!(&*call.func, Expr::Name(func) if func.id.as_str() == "set")
                && call.arguments.is_empty() =>
        {
            let element = added(&expression.value, name, "add").ok_or(Refusal::NotApplicable)?;
            ("{", "}", source[element.range()].to_string())
        }
        (Expr::Dict(dict), Stmt::Assign(assign)) if dict.is_empty() => {
            let [Expr::Subscript(target)] = assign.targets.as_slice() else {
                return Err(Refusal::NotApplicable);
            };
            if !matches!(&*target.value, Expr::Name(value) if value.id.as_str() == name) {
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
            (
                "{",
                "}",
                format!(
                    "{}: {}",
                    &source[target.slice.range()],
                    &source[assign.value.range()]
                ),
            )
        }
        _ => return Err(Refusal::NotApplicable),
    };
    let kind = match open {
        "[" => "list",
        _ if element.contains(": ") && matches!(&*initial.value, Expr::Dict(_)) => "dict",
        _ => "set",
    };
    let title = format!("Convert loop to {kind} comprehension");

    if !for_loop.orelse.is_empty() {
        return Err(Refusal::refused(
            title,
            "its `else` runs after the loop, which a comprehension has no place for",
        ));
    }
    // `set` is python's own unless the program binds the name
    if let Expr::Call(call) = &*initial.value
        && let Expr::Name(func) = &*call.func
    {
        let index = semantic_index(context.db, context.file);
        let shadowed = index
            .try_expression_scope_id(&ast::ExprRef::Name(func))
            .is_none_or(|scope| resolving_scope(index, scope, func.id.as_str()).is_some());
        if shadowed {
            return Err(Refusal::refused(title, "`set` is not python's own here"));
        }
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
    // a comprehension does not leave its variable bound after it
    let mut bound = Vec::new();
    names_in(&for_loop.target, &mut bound);
    let after = TextRange::new(for_loop.end(), scope_end(context, for_loop));
    for variable in &bound {
        if reads_binding_after(context, variable, after, for_loop.target.range()) {
            return Err(Refusal::refused(
                title,
                format!(
                    "`{variable}` is read after the loop, which a comprehension leaves unbound"
                ),
            ));
        }
    }
    // a class body's names are not visible inside a comprehension
    if in_class_body(context, for_loop) {
        return Err(Refusal::refused(
            title,
            "a comprehension in a class body cannot see the class's names",
        ));
    }

    let mut comprehension = format!(
        "{open}{element} for {} in {}",
        &source[for_loop.target.range()],
        &source[for_loop.iter.range()]
    );
    if let Some(condition) = condition {
        comprehension.push_str(" if ");
        comprehension.push_str(&source[condition.range()]);
    }
    comprehension.push_str(close);
    Ok(Plan {
        title,
        edits: vec![
            Edit::range_replacement(comprehension, initial.value.range()),
            Edit::range_deletion(TextRange::new(
                source.full_line_end(initial.end()),
                source.full_line_end(for_loop.end()),
            )),
        ],
    })
}

/// The element `name.method(element)` adds.
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

/// The suite `for_loop` is written in, and its index there.
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

/// Counts the loads of a name.
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

/// Where the scope the loop is in ends: its function, or the module.
fn scope_end(context: &RefactorContext<'_>, for_loop: &ast::StmtFor) -> TextSize {
    let covering = covering_node(context.parsed.syntax().into(), for_loop.range());
    covering
        .ancestors()
        .find_map(|node| match node {
            AnyNodeRef::StmtFunctionDef(function) => Some(function.end()),
            _ => None,
        })
        .unwrap_or_else(|| context.parsed.syntax().end())
}

/// Whether a read of `name` in `range` may read what the loop bound to it, the binding in
/// `target`: a read the loop's binding reaches, or a read in a nested scope, which could run at
/// any time.
fn reads_binding_after(
    context: &RefactorContext<'_>,
    name: &str,
    range: TextRange,
    target: TextRange,
) -> bool {
    struct Loads<'a> {
        name: &'a str,
        range: TextRange,
        found: Vec<&'a ast::ExprName>,
    }
    impl<'a> SourceOrderVisitor<'a> for Loads<'a> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Name(name) = expr
                && name.id.as_str() == self.name
                && name.ctx.is_load()
                && self.range.contains_range(name.range())
            {
                self.found.push(name);
            }
            walk_expr(self, expr);
        }
    }
    let mut loads = Loads {
        name,
        range,
        found: Vec::new(),
    };
    loads.visit_body(context.parsed.suite());
    let index = semantic_index(context.db, context.file);
    let Some(loop_scope) = covering_node(context.parsed.syntax().into(), target)
        .ancestors()
        .find_map(|node| match node {
            AnyNodeRef::StmtFor(for_loop) => {
                index.try_expression_scope_id(&ast::ExprRef::from(&*for_loop.iter))
            }
            _ => None,
        })
    else {
        return !loads.found.is_empty();
    };
    loads.found.into_iter().any(|load| {
        let Some(scope) = index.try_expression_scope_id(&ast::ExprRef::Name(load)) else {
            return true;
        };
        if scope != loop_scope {
            return true;
        }
        let reaching = reaching_definitions(context.db, index, scope, load);
        reaching.maybe_unbound && reaching.definitions.is_empty()
            || reaching.definitions.iter().any(|definition| {
                target.contains_range(definition.focus_range(context.db, &context.parsed).range())
            })
    })
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

    /// `not x > 1` is not `x <= 1` for a float that is `nan`.
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

    /// `a and b` is `a` or `b`, which the rewrite would make a `bool`.
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

    /// A comprehension leaves `item` unbound, which the loop left bound to the last item.
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
    /// A later loop binds `item` afresh before reading it, so the first loop's `item` is not
    /// read after it.
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
}
